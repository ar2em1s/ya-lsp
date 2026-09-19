//! `textDocument/signatureHelp`: the parameters of the call the cursor is inside.
//!
//! Nothing here is resolved: `locator::precise_call` already found the method and
//! `render::signature_label` already spelled it. This module decides the two numbers LSP asks for
//! on top: which overload the call fits, and which parameter the cursor is writing.
//!
//! It is only asked when the callee resolved *exactly*. A name-based guess would put another
//! class's parameters under the cursor while the user types into them. Keyword-argument completion
//! refuses the same wrong answer for the same reason: it would look valid.

use lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureHelp,
    SignatureInformation,
};
use rubydex::model::{
    definitions::{Definition, Parameter},
    graph::Graph,
    ids::DeclarationId,
};

use super::{cursor::Active, locator, render};

/// The signature card for a resolved call, or `None` when the declaration is not a method anybody
/// wrote parameters for.
///
/// The declaration is the one the *call* resolves to, which for `Foo.new(` is `Foo#initialize`: the
/// redirect `locator` makes, and the only reading under which the parameters shown are the ones the
/// call takes. The label says `Foo#initialize`, not `Foo.new`, for hover's reason: the user is
/// looking at the method they are passing arguments to.
#[must_use]
pub fn help(
    graph: &Graph,
    declaration_id: DeclarationId,
    active: &Active,
) -> Option<SignatureHelp> {
    let declaration = graph.declarations().get(&declaration_id)?;
    let definitions = locator::definitions_of(graph, declaration_id);
    let method = definitions.iter().find_map(|definition| match definition {
        Definition::Method(method) => Some(method),
        _ => None,
    })?;

    let name = render::qualified_name(graph, declaration.name());
    // Hover's rule: the first definition with anything to say. A reopened class documents a method
    // in one of its parts, not all of them.
    let documentation = definitions
        .iter()
        .find_map(|definition| render::documentation(definition.comments()))
        .map(|value| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            })
        });

    let overloads = method.signatures().as_slice();
    let signatures: Vec<SignatureInformation> = overloads
        .iter()
        .map(|signature| {
            let rendered = render::signature_label(graph, &name, signature);
            SignatureInformation {
                label: rendered.label,
                documentation: documentation.clone(),
                parameters: Some(
                    rendered
                        .parameters
                        .into_iter()
                        .map(|(start, end)| ParameterInformation {
                            label: ParameterLabel::LabelOffsets([start, end]),
                            documentation: None,
                        })
                        .collect(),
                ),
                active_parameter: active_parameter(graph, signature, active),
            }
        })
        .collect();

    // Only RBS gives more than one, and it declares them so one fits: `String#gsub` has three, and
    // the argument count says which is being written. Flattening them into one would mean knowing
    // less than the signatures do.
    let chosen = overloads
        .iter()
        .position(|signature| accepts(graph, signature, active))
        .unwrap_or(0);

    Some(SignatureHelp {
        // `SignatureInformation::activeParameter` is LSP 3.16 and overrides this one per signature.
        // An older client reads only this, so the chosen overload's answer is repeated here, not
        // left for it to infer.
        active_parameter: signatures
            .get(chosen)
            .and_then(|signature| signature.active_parameter),
        active_signature: u32::try_from(chosen).ok(),
        signatures,
    })
}

/// Whether a signature has the parameter the cursor is writing.
///
/// The only question an overload set is picked by: a call with three arguments is not being written
/// against the two-argument arm.
fn accepts(graph: &Graph, signature: &[Parameter], active: &Active) -> bool {
    match active {
        Active::Nth(nth) => {
            (*nth as usize) < signature.len()
                || signature.iter().any(|parameter| {
                    matches!(
                        parameter,
                        Parameter::RestPositional(_) | Parameter::Forward(_)
                    )
                })
        }
        // An arm that *declares* this keyword, or has a `**opts` for it to fall into. An arm that
        // only takes some other keyword does not count: `(a:, b:)` is not the arm being written
        // when the cursor is inside `c:`.
        Active::Keyword(name) => {
            keyword(graph, signature, name).is_some() || rest_keyword(signature).is_some()
        }
        Active::AnyKeyword => takes_keywords(signature),
    }
}

/// Whether a signature has anywhere for a keyword argument to go.
fn takes_keywords(signature: &[Parameter]) -> bool {
    first_keyword(signature).is_some() || rest_keyword(signature).is_some()
}

/// Which parameter of this signature to highlight, as an index into it.
///
/// The ceiling matters, and it is not always the end of the list.
/// - **A `*rest` absorbs every positional argument after it.** The fourth argument to
///   `def new(name, age = 18, *nicknames)` is still `*nicknames`, not something past the end;
///   counting straight through would move one parameter further for every argument typed.
/// - **With no splat, the last parameter is the ceiling.** LSP 3.17 cannot say that *no* parameter
///   is active (an index outside the list and an omitted one both mean zero), so an answer that ran
///   off the end must land on the nearest parameter that is still true, not fall back to the first.
///
/// A `Post` parameter (the `c` in `def f(a, *b, c)`) is the one case the ceiling gets wrong, and it
/// is unknowable, not unhandled: which argument fills it depends on how many there will be, which
/// is undecided while the call is being written.
fn active_parameter(graph: &Graph, signature: &[Parameter], active: &Active) -> Option<u32> {
    let index = match active {
        Active::Nth(nth) => {
            let last = signature.len().checked_sub(1)?;
            (*nth as usize).min(rest_positional(signature).unwrap_or(last))
        }
        // A keyword is found by name, because keywords are written in any order. One the method
        // does not declare belongs in `**opts`.
        Active::Keyword(name) => keyword(graph, signature, name)
            .or_else(|| rest_keyword(signature))
            .or_else(|| first_keyword(signature))?,
        // Nothing to look up yet, so the answer is the region, not the parameter: the first keyword
        // the method declares, and `**opts` only if it declares none.
        Active::AnyKeyword => first_keyword(signature).or_else(|| rest_keyword(signature))?,
    };
    u32::try_from(index).ok()
}

/// The keyword parameter written under `name`, positionally.
fn keyword(graph: &Graph, signature: &[Parameter], name: &str) -> Option<usize> {
    signature.iter().position(|parameter| {
        matches!(
            parameter,
            Parameter::RequiredKeyword(_) | Parameter::OptionalKeyword(_)
        ) && spelled(graph, parameter) == Some(name)
    })
}

fn first_keyword(signature: &[Parameter]) -> Option<usize> {
    signature.iter().position(|parameter| {
        matches!(
            parameter,
            Parameter::RequiredKeyword(_) | Parameter::OptionalKeyword(_)
        )
    })
}

fn rest_positional(signature: &[Parameter]) -> Option<usize> {
    signature.iter().position(|parameter| {
        matches!(
            parameter,
            Parameter::RestPositional(_) | Parameter::Forward(_)
        )
    })
}

fn rest_keyword(signature: &[Parameter]) -> Option<usize> {
    signature
        .iter()
        .position(|parameter| matches!(parameter, Parameter::RestKeyword(_)))
}

/// A parameter's name as rubydex interned it. `None` where the string is gone: the same "nothing to
/// match" as a name nobody wrote.
fn spelled<'g>(graph: &'g Graph, parameter: &Parameter) -> Option<&'g str> {
    Some(graph.strings().get(parameter.inner().str())?.as_str())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::testing::*;

    /// One class carrying every parameter kind, for the one request that is *about* parameters.
    ///
    /// `initialize`, not an ordinary method, so `Person.new(` (by far the most common call) is
    /// pinned by the same fixture that pins the rendering. `shout` exists to be called on a
    /// receiver nothing can type, where the answer must be `null`.
    const CALLS: &str = "\
class Person
  # Make one.
  def initialize(name, age = 18, *nicknames, admin: false, **extra, &block)
  end

  # Build one.
  def self.build(name, sep:)
  end

  def self.locate(x, y)
  end

  def self.tag(**attributes)
  end

  class << self
    attr_reader :registry
  end

  def shout(volume)
  end
end
";

    fn calling(marked: &str) -> String {
        format!("{CALLS}{marked}")
    }

    #[test]
    fn a_call_shows_the_method_it_reaches_with_the_argument_being_written_underlined() {
        // The whole card, drawn: the label, the span under the parameter, and the comment. The
        // underline's position pins two things: that `render` spells each parameter kind as Ruby
        // writes it, and that the offsets it returns land on the piece of the label they were
        // computed for. Asserting the numbers instead would pass with the underline three
        // characters to the left.
        //
        // And `Person.new` is answered with `Person#initialize`. `Class#new` is exact and useless:
        // the parameters the call takes are the constructor's. This is the redirect `locator`
        // already makes for hover and navigation, reaching signature help through the same door.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert_eq!(
            harness.signature_card(&uri, &calling("Person.new(~)\n")),
            "Person#initialize(name, age = ..., *nicknames, admin: ..., **extra, &block)\n\
             \u{20}                 ~~~~\n\
             Make one."
        );
    }

    #[test]
    fn the_underline_follows_the_cursor_from_one_argument_to_the_next() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        let underline = |harness: &mut Harness, call: &str| {
            harness
                .signature_card(&uri, &calling(call))
                .lines()
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };

        // `(name, age = ..., *nicknames, admin: ..., **extra, &block)` from column 17.
        assert_eq!(
            underline(&mut harness, "Person.new(~)\n"),
            " ".repeat(18) + "~~~~"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", ~)\n"),
            " ".repeat(24) + "~~~~~~~~~",
            "the second argument is `age = ...`"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", 30, ~)\n"),
            " ".repeat(35) + "~~~~~~~~~~",
            "and the third is the splat"
        );
        // The rule the splat exists for: everything positional after it goes into it, so counting
        // straight through would walk off the end of a method that cannot be over-called. This is
        // the fifth argument, and it is still `*nicknames`.
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", 30, \"a\", \"b\", ~)\n"),
            " ".repeat(35) + "~~~~~~~~~~",
            "and so is the fifth"
        );
    }

    #[test]
    fn a_keyword_argument_is_found_by_name_and_an_unknown_one_lands_in_the_splat() {
        // Keywords are written in any order, so the positional count answers the wrong parameter
        // the moment anybody reorders two. The name is the only thing that identifies one, and a
        // name the method does not declare goes to `**extra`, as shown.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        let underline = |harness: &mut Harness, call: &str| {
            harness
                .signature_card(&uri, &calling(call))
                .lines()
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };

        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", admin: ~)\n"),
            " ".repeat(47) + "~~~~~~~~~~",
            "`admin: ...`"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(admin: true, ~)\n"),
            " ".repeat(47) + "~~~~~~~~~~",
            "still `admin:`, one argument later"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", nickname: ~)\n"),
            " ".repeat(59) + "~~~~~~~",
            "a keyword the method never declared is `**extra`'s"
        );
        // With no `**opts` to fall into, an undeclared keyword still belongs to the keyword half of
        // the signature, not to a positional parameter it cannot be passed as.
        assert_eq!(
            harness.signature_card(&uri, &calling("Person.build(\"ada\", bogus: ~)\n")),
            "Person.build(name, sep:)\n\u{20}                  ~~~~\nBuild one."
        );
        // And a method whose only keyword is the splat is where an unnamed one lands too.
        assert_eq!(
            harness.signature_card(&uri, &calling("Person.tag(id: 1, ~)\n")),
            "Person.tag(**attributes)\n\u{20}          ~~~~~~~~~~~~"
        );
    }

    #[test]
    fn a_singleton_method_is_named_the_way_it_is_called() {
        // `Person::<Person>#build()` is rubydex's spelling and nobody's Ruby. Signature help goes
        // through `render::qualified_name` for the reason hover and the outline do: a construct
        // that reads one way in one card and another way in the next is a bug.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert_eq!(
            harness.signature_card(&uri, &calling("Person.build(~)\n")),
            "Person.build(name, sep:)\n\
             \u{20}            ~~~~\n\
             Build one."
        );
    }

    #[test]
    fn the_innermost_call_is_the_one_being_written() {
        // A cursor inside a nested call's parentheses belongs to the inner call, at no cost: the
        // walk that finds the enclosing argument list is pre-order, so the innermost claimant
        // writes itself down last.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert_eq!(
            harness.signature_card(&uri, &calling("Person.new(Person.build(~))\n")),
            "Person.build(name, sep:)\n\
             \u{20}            ~~~~\n\
             Build one."
        );
        // And back out again, with the inner call now one finished argument.
        assert_eq!(
            harness
                .signature_card(
                    &uri,
                    &calling("Person.new(Person.build(\"ada\", sep: \",\"), ~)\n")
                )
                .lines()
                .next()
                .unwrap_or_default(),
            "Person#initialize(name, age = ..., *nicknames, admin: ..., **extra, &block)"
        );
    }

    #[test]
    fn a_receiver_nothing_can_name_is_answered_with_nothing() {
        // The rule keyword-argument completion applies, for the reason the README states:
        // `person.shout` matches on the name alone, and another class's parameter list under the
        // cursor while the user types into it is a valid-looking wrong answer. Absent beats wrong:
        // the editor falls back to showing nothing.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        for marked in [
            "person = whatever\nperson.shout(~)\n",
            // The graph names a *constant* receiver and nothing else, so an instance of one is not
            // a name either: the line keyword-argument completion draws, reached through the same
            // `locator::precise_call`. Widening it means typing an expression, which is
            // deliberately out of scope.
            "person = Person.new(\"ada\")\nperson.shout(~)\n",
            "Person.new(\"ada\").shout(~)\n",
        ] {
            assert!(
                harness.signature(&uri, &calling(marked)).is_null(),
                "{marked:?} has no receiver the graph can name"
            );
        }
        // The guard, so this cannot pass by never answering: the same file and method, through a
        // constant receiver.
        assert_eq!(
            harness.signature_card(&uri, &calling("Person.build(~)\n")),
            "Person.build(name, sep:)\n\u{20}            ~~~~\nBuild one."
        );
    }

    #[test]
    fn a_keyword_a_method_has_nowhere_to_put_underlines_nothing() {
        // `locate` takes two positionals and no keywords, so no parameter can be a keyword
        // argument. The signature is still worth showing (it tells the user why the call is wrong),
        // but highlighting a positional parameter would claim a keyword can be passed as one.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        for marked in [
            "Person.locate(1, missing: ~)\n",
            "Person.locate(missing: 1, ~)\n",
        ] {
            assert_eq!(
                harness.signature_card(&uri, &calling(marked)),
                "Person.locate(x, y)",
                "{marked:?}"
            );
        }
    }

    #[test]
    fn a_reader_with_no_parameters_to_show_is_answered_with_nothing() {
        // `attr_reader` declares a method rubydex records as an attribute, not a `def`, so it has
        // no parameter list, and a getter takes no arguments, so a card has nothing to say. `null`
        // closes the popup: right for a call that should not have parentheses at all.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert!(
            harness
                .signature(&uri, &calling("Person.registry(~)\n"))
                .is_null()
        );
        // The guard: the same receiver and a method that does have one.
        assert!(
            !harness
                .signature(&uri, &calling("Person.locate(~)\n"))
                .is_null()
        );
    }

    #[test]
    fn a_cursor_outside_every_argument_list_is_answered_with_nothing() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        for marked in [
            "Person.new(\"ada\")~\n",
            "x = 1~\n",
            "Person~.new\n",
            "# a note about Person.new(~)\n",
        ] {
            assert!(
                harness.signature(&uri, &calling(marked)).is_null(),
                "{marked:?} is not inside a call"
            );
        }
    }

    #[test]
    fn a_parameter_span_is_counted_the_way_the_client_indexes_the_label() {
        // The offsets are into a string the client holds as UTF-16, and a Ruby parameter can be
        // spelled in any script: `def приветствие(имя)` is legal Ruby. Counting bytes would put the
        // span too far along for any non-ASCII name (Cyrillic is 2 bytes per UTF-16 unit), and the
        // drawings every other test asserts on cannot see it because every other fixture is ASCII.
        // So this one asserts the numbers.
        let source = "class Greeter\n  def self.hello(имя, sep)\n  end\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/greeter.rb", source);
        harness.index();

        let help = harness.signature(&uri, &format!("{source}Greeter.hello(\"a\", ~)\n"));
        let parameters = help["signatures"][0]["parameters"]
            .as_array()
            .expect("a parameter list")
            .clone();
        assert_eq!(
            help["signatures"][0]["label"].as_str(),
            Some("Greeter.hello(имя, sep)")
        );
        // `Greeter#hello(` is 14 UTF-16 units; `имя` is 3, however many bytes it takes.
        assert_eq!(parameters[0]["label"], serde_json::json!([14, 17]));
        assert_eq!(parameters[1]["label"], serde_json::json!([19, 22]));
        assert_eq!(help["activeParameter"], serde_json::json!(1));
    }

    #[test]
    fn every_overload_is_offered_and_the_one_being_written_is_chosen() {
        // `Signatures::Overloaded` is real (RBS declares three arms for `String#gsub`), and LSP has
        // `activeSignature` for exactly this. Flattening them to the first would mean knowing less
        // than the signatures do.
        //
        // The arity-1 arm is written first on purpose: with the shorter arm second, every cursor
        // position fits the first, and a broken choice would pass.
        let (mut harness, uri) = with_signatures("");
        assert_eq!(
            harness.signature_card(&uri, "Coordinate.new(1, ~)\n"),
            "Coordinate#initialize(text)\n\
             Coordinate#initialize(x, y)\n\
             \u{20}                        ~\n\
             A point, from a pair or from text."
        );
        // Nothing written yet, so both arms fit and the first is the answer: an argument count
        // cannot tell arity 1 from arity 2 before there is an argument.
        assert_eq!(
            harness.signature_card(&uri, "Coordinate.new(~)\n"),
            "Coordinate#initialize(text)\n\
             \u{20}                     ~~~~\n\
             Coordinate#initialize(x, y)\n\
             A point, from a pair or from text."
        );
    }
    #[test]
    fn a_private_signature_is_not_drawn_where_the_jump_has_nowhere_to_go() {
        // **The card and the jump answer one cursor and must not disagree.** Both go through
        // `precise_call`'s rung, so `definition`'s privacy gate must reach here too. Otherwise
        // `Vault.secret_value(` draws a parameter list for a call Ruby raises on, the loudest
        // moment in the server to be confidently wrong.
        //
        // The receiver comes off the `CallNode` this request's own parse already produced, so
        // nothing is re-parsed. See `cursor::Call::allows_private`.
        let mut harness = Harness::new();
        let source = concat!(
            "class Vault\n",
            "  def peek\n",
            "    Vault.secret_value(\"x\")\n",
            "  end\n",
            "\n",
            "  private_class_method def self.secret_value(name)\n",
            "  end\n",
            "end\n",
        );
        let uri = harness.write("app/models/vault.rb", source);
        harness.index();

        assert_eq!(
            harness
                .signature(
                    &uri,
                    concat!(
                        "class Vault\n",
                        "  def peek\n",
                        "    Vault.secret_value(~\"x\")\n",
                        "  end\n",
                        "\n",
                        "  private_class_method def self.secret_value(name)\n",
                        "  end\n",
                        "end\n",
                    )
                )
                .to_string(),
            "null",
            "a written receiver that is not `self` reaches no private method"
        );

        // And the control, at the one spelling Ruby permits: the card comes back.
        assert!(
            harness
                .signature_card(
                    &uri,
                    concat!(
                        "class Vault\n",
                        "  def self.peek\n",
                        "    self.secret_value(~\"x\")\n",
                        "  end\n",
                        "\n",
                        "  private_class_method def self.secret_value(name)\n",
                        "  end\n",
                        "end\n",
                    )
                )
                .contains("secret_value"),
            "written `self` is the exemption Ruby actually grants"
        );
    }

    #[test]
    fn a_signature_only_the_suite_declares_is_not_drawn_over_application_code() {
        // The fence `definition` applies to a root answer, at the same cursor. A top-level `def` in
        // a spec lands on `Object` and so answers for every receiver. Drawing its parameter list
        // under the argument being typed is the loudest place to be confidently wrong, and it would
        // disagree with the jump at the same cursor.
        //
        // `precise_call` has no name rung to fall back to (an exact callee is its whole contract),
        // so the card is simply not drawn.
        let mut harness = Harness::new();
        harness.write(
            "spec/lib/email_cook_spec.rb",
            "def cook(raw, expected)\nend\n",
        );
        let source = "class Post\n  def bake\n    cook(\"x\")\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        assert_eq!(
            harness
                .signature(
                    &post,
                    "class Post\n  def bake\n    cook(~\"x\")\n  end\nend\n"
                )
                .to_string(),
            "null"
        );

        // The control, from a cursor inside the suite: the gate is off there, so the same call
        // keeps its card.
        let spec = harness.write(
            "spec/models/post_spec.rb",
            "describe Post do\n  it \"bakes\" do\n    cook(\"x\")\n  end\nend\n",
        );
        harness.index();
        assert!(
            harness
                .signature_card(
                    &spec,
                    "describe Post do\n  it \"bakes\" do\n    cook(~\"x\")\n  end\nend\n"
                )
                .contains("cook"),
            "a developer editing a spec is who that `def` is for"
        );
    }
}
