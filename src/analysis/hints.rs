//! `textDocument/inlayHint`: a derived type, drawn without being asked for.
//!
//! The one request in the crate that shows an answer nobody requested, and that changes what may be
//! said. A hover card is read after a deliberate keystroke, has room to say it guessed, and is
//! understood as ya-lsp's opinion. A hint is painted into the margin of every line, wanted or not,
//! has room for nothing, and is read as fact. So the tier decides what may be drawn here, not just
//! how it is labelled.
//!
//! # The three tiers, and the one that cannot occur here
//!
//! - **Guessed**: matched on a name alone. **Never drawn.** A name-matched type painted beside
//!   every method in the file is exactly the failure this project is built against, and no label is
//!   small enough to fix it. The refusal tests [`Tier`], not a list of shapes, so the next rung
//!   added below the graph is refused by the same line instead of appearing in everybody's margin
//!   the day it ships.
//! - **Derived**: a signature, an assignment or a convention was followed. Drawn, as the label
//!   alone: no surface says how a type was found (decided 2026-09-29), so a hint has no tooltip.
//! - **Resolved**: the code names the type. **Unreachable, by construction**, and that is the one
//!   thing worth knowing about this module.
//!
//! A hint exists exactly where the code does *not* state the type. Where it does (`x = 1`,
//! `x = Foo.new`, `x = Foo`), the label would repeat a word on the line, and [`worth_saying`]
//! refuses it as noise. Those are exactly the bindings whose type is resolved: **a type the code
//! states is a type the margin need not repeat.**
//!
//! So every drawn hint is derived, and there is no second kind on screen to tell it apart from,
//! which is why labels carry no marker.
//!
//! # Three families, and why not a fourth
//!
//! A block parameter, a local assigned from a call, and a method's declared return. What they share
//! is the paragraph above: the line does not already say the type. A server that draws noise gets
//! turned off, and then the three useful families are gone too.

use std::collections::HashMap;

use rubydex::model::{
    definitions::Definition,
    graph::Graph,
    ids::{DeclarationId, UriId},
};

use crate::workspace::{DocUri, config::HintsConfig};

use super::{
    cursor::{self, Binding, Receiver},
    locator, render,
    synthesized::generated_prefix,
    types::{self, Derivation, Sources, Tier},
};

/// Which family a hint belongs to, and so which setting silences it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `stories.each do |story|` — what the called method says its block receives.
    BlockParameter,
    /// `author = story.author` — what the call on the right hands back.
    Local,
    /// `def title` — what a signature says the method returns.
    Return,
}

/// Why a label is the type it is: what decides its tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Because {
    /// A receiver ya-lsp typed, and what it followed to get there. Empty means the resolved tier:
    /// the code named the type and nothing was followed.
    ///
    /// **Boxed** because of the other variant: a [`Derivation`] carries a name or a place for every
    /// rung (mostly `None` on any one answer), while the other variant carries nothing.
    /// Hints are built one per binding across a visible range, so the enum's size is paid per
    /// label, not only by the few that followed anything.
    Followed(Box<Derivation>),
    /// A signature declares what the method returns, and Ruby has no syntax for saying so.
    ///
    /// Always the derived tier, honestly, not as a shortcut. A signature is a *claim* about a
    /// method: nothing checks it unless a type checker runs, and where core RBS is overridden by
    /// the `def` under the label, it is a claim about a method no longer there.
    /// [`annotations`](super::annotations) makes the same argument about the two hand-written
    /// syntaxes.
    Declared,
    /// Every path through the `def` raises, which its own code says ([`types::never_returns`]).
    Raises,
}

/// One label, and everything needed to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub family: Family,
    /// Where the label goes, as a byte offset into the text this was read from.
    pub at: u32,
    /// The label exactly as it is drawn, separator included.
    pub label: String,
    pub because: Because,
}

impl Hint {
    /// Which tier this answer is. [`Tier::Guessed`] never reaches here; see the module docs.
    #[must_use]
    pub fn tier(&self) -> Tier {
        match &self.because {
            Because::Followed(derivation) => derivation.tier(),
            Because::Declared | Because::Raises => Tier::Derived,
        }
    }
}

/// Every hint for the part of `source` inside `within`, in source order.
///
/// `within` is the range the editor has on screen, and it bounds the *work*, not the answer;
/// [`cursor::margin`] takes it for that reason. One parse either way; the range saves every graph
/// lookup after it.
///
/// **The buffer's offsets are the graph's here, by the request, not by assumption.**
/// `textDocument/inlayHint` is not one of the three requests that answer between a keystroke and
/// its index, so `Analysis::serve` has settled first and the two texts are one string, as for
/// `documentHighlight` and `references`.
#[must_use]
pub fn of(
    sources: &Sources<'_>,
    uri: &DocUri,
    source: &str,
    within: (u32, u32),
    shown: &HintsConfig,
) -> Vec<Hint> {
    let uri_id = UriId::from(uri.as_str());
    // **One parse of the document for the whole request** ([`cursor::margin`]): the bindings, the
    // `def`s, and the walk the type side reads next, unless it is held for this text already. The
    // type side then finds it held and parses nothing.
    let held = sources.held_exits;
    let margin = cursor::margin(source, within, !held.holds(uri.as_str(), source));
    if let Some(shapes) = margin.shapes {
        held.keep(uri.as_str(), source, shapes);
    }
    let mut hints: Vec<Hint> = margin
        .bindings
        .into_iter()
        .filter(|bound| match bound.binding {
            Binding::BlockParameter => shown.block_parameters,
            Binding::Local => shown.locals,
        })
        .filter(|bound| worth_saying(&bound.was))
        .filter_map(|bound| {
            let scope = sources.scope_at(uri_id, bound.name.0);
            let typed = types::method_receiver(sources, uri_id, &bound.was, &scope)?;
            Some(Hint {
                family: match bound.binding {
                    Binding::BlockParameter => Family::BlockParameter,
                    Binding::Local => Family::Local,
                },
                at: bound.name.1,
                label: label(sources.graph, &typed)?,
                because: Because::Followed(Box::new(typed.derivation)),
            })
        })
        .collect();

    if shown.returns {
        hints.extend(returns(sources, uri, uri_id, source, margin.defs));
    }
    // The bottom tier is refused here, once, after every family has answered: a test on the tier,
    // never on the shape that produced it. See the module docs.
    hints.retain(|hint| hint.tier() != Tier::Guessed);
    hints.sort_by_key(|hint| hint.at);
    hints
}

/// Whether a binding's shape says anything its line does not already say.
///
/// The whole "not every local" rule, and it is about *shape*, not type: `x = Foo.new`, `x = Foo`,
/// `x = "s"` and `x = 1` all name their class in the assignment, and a label repeating it is noise
/// on an already clear line. What remains is a call whose return nothing writes down, which is
/// worth a margin.
///
/// A block parameter is always worth saying (nothing about `|story|` says what it holds), and it
/// arrives here as the one shape that can only come from a signature.
fn worth_saying(was: &Receiver) -> bool {
    match was {
        // A read of another variable names no class on this line either, whatever its writes say.
        // What a block hands back is no more written on its line than a call's return.
        Receiver::Returned { .. }
        | Receiver::Yielded { .. }
        | Receiver::Yield { .. }
        | Receiver::Variable(_) => true,
        // The two wrappers are not shapes of their own: an instance variable carries where it was
        // written and a spelled local carries its name, and either is worth what it wraps.
        Receiver::Assigned { was, .. } | Receiver::Spelled { was, .. } => worth_saying(was),
        // One position of a tuple, worth exactly what its call is worth:
        // `read_io, write_io = IO.pipe` names no class on its line either. Without this arm, the
        // wrap `cursor::bindings_in` applies would read as "not worth saying", and every
        // destructured target would go unlabelled, including those the tuple table answers.
        Receiver::Destructured { of, .. } => worth_saying(of),
        // One side of `block_given?`, and an assignment's value, worth what each holds.
        Receiver::BlockGiven { value, .. } | Receiver::Stored(value) => worth_saying(value),
        // `lambda { }` and `proc { }` are calls, worth what a call is: nothing on the line says
        // `Kernel#lambda` makes a `Proc`, and a class may define its own `proc`.
        Receiver::Proc {
            call: Some(call), ..
        } => worth_saying(call),
        // A conditional is worth what its branches are: `x = ok ? "a" : "b"` names `String` twice,
        // and `x = ok ? a.b : c.d` names nothing.
        Receiver::Either(arms) => arms.iter().any(worth_saying),
        // The line names the class: a constant, `Foo.new`, a literal, `self`, the top level.
        Receiver::Constant(_)
        | Receiver::Instance { .. }
        // `rescue Timeout::Error => e` names the class, and a bare `rescue => e` is
        // `StandardError` by Ruby's rule.
        | Receiver::Rescued(_)
        | Receiver::Literal { .. }
        // `->(x) { }` names `Proc` on its face.
        | Receiver::Proc { call: None, .. }
        | Receiver::SelfObject(_)
        | Receiver::TopLevel
        // `!x` is a `bool` on its face.
        | Receiver::Negated(_)
        // Not drawn: `super(...)`, `a || b` and a parameter's own read are labels nobody has
        // decided on. Listed, not left to a wildcard, so a new shape is decided too.
        | Receiver::Super { .. }
        | Receiver::Shortcut { .. }
        | Receiver::Parameter { .. }
        | Receiver::ProcParameter { .. }
        // A guess or nothing: never drawn (`hints.md`'s first rule).
        | Receiver::Named(_)
        | Receiver::Unknown => false,
    }
}

/// A label for the declaration a type resolved to, or `None` where there is nothing to draw.
///
/// **Classes, modules and class objects only**, as [`render::typed`] spells them: a class object is
/// `Foo:class`. A `Namespace::Todo` is a name nothing defines, so a label from one points at
/// nothing a reader could check, and a module object has no spelling.
fn label(graph: &Graph, typed: &types::Typed) -> Option<String> {
    Some(format!(
        ": {}",
        readable(typed, render::typed(graph, typed)?)?
    ))
}

/// A spelled type, unless it is a union over a concern's including classes too wide to read
/// ([`MAX_INCLUDERS_SPELLED`]).
fn readable(typed: &types::Typed, spelled: String) -> Option<String> {
    let wide = spelled.matches(" | ").count() >= MAX_INCLUDERS_SPELLED;
    (!(wide && typed.derivation.each.is_some())).then_some(spelled)
}

/// The widest union over a concern's including classes a margin spells. A label is
/// read at a glance, and a scope in a concern two dozen models include is `Account::Relation |
/// Block::Relation | …` there, which nobody reads. The card, with room for it, still shows every
/// member.
///
/// - **Only such a union**, whose width is the number of includers, not the value's: a numeric
///   tower (`BigDecimal | Integer | Float | Rational | Complex`) is drawn as before.
/// - **Counted as spelled**, so a union that collapses into the class every member inherits from
///   (`render::typed`) is one member however many it holds.
const MAX_INCLUDERS_SPELLED: usize = 4;

/// Every `def` in `within` whose return a signature declares and the source does not.
///
/// Two halves that agree by construction: the anchor comes from Prism (the end of the parameter
/// list, the only place ` -> String` reads as Ruby), and the declaration from the graph, joined by
/// the name span both parsers report for the same bytes.
fn returns(
    sources: &Sources<'_>,
    uri: &DocUri,
    uri_id: UriId,
    source: &str,
    defs: Vec<cursor::DefSite>,
) -> Vec<Hint> {
    let graph = sources.graph;
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };
    let methods: HashMap<(u32, u32), DeclarationId> = document
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
        .filter(|definition| matches!(definition, Definition::Method(_)))
        .filter_map(|definition| {
            let name = definition.name_offset()?;
            Some((
                (name.start(), name.end()),
                *graph.definition_to_declaration_id(definition)?,
            ))
        })
        .collect();
    // The documents whose declarations were written *here*: a Sorbet `sig` or YARD `@return` two
    // lines up is the source stating the return type, so repeating it in the margin is the noise
    // this third family is defined against. A prefix, not one URI, because one source writes one
    // generated document per body, and the prefix is exact thanks to the trailing `#` in
    // `generated_prefix`.
    let written_here = generated_prefix(uri.as_str());

    defs.into_iter()
        .filter_map(|cursor::DefSite { name, at, raises }| {
            if value_is_discarded(&source[name.0 as usize..name.1 as usize]) {
                return None;
            }
            let id = *methods.get(&name)?;
            if locator::definitions_of(graph, id).iter().any(|definition| {
                graph
                    .documents()
                    .get(definition.uri_id())
                    .is_some_and(|document| document.uri().starts_with(&written_here))
            }) {
                return None;
            }
            // A signature, else the body, and the union where bodies dispute the signature: the
            // same answer the card and a chain read ([`types::method_return`]). A body read arrives
            // as [`Because::Followed`], so the tier rule below still holds: a body whose exit was
            // guessed is guessed, and a guess is never painted into a margin.
            // The method this `def` really defines, where rubydex filed it under another: an RSpec
            // group's helper is its group's, not every spec's one `Object` method.
            let id = types::own_def_member(sources, uri.as_str(), name).unwrap_or(id);
            // **Every path raises**: nothing is handed back, and the margin says so.
            if raises && types::never_returns(sources, id) {
                return Some(Hint {
                    family: Family::Return,
                    at,
                    label: " -> bot".to_owned(),
                    because: Because::Raises,
                });
            }
            let returned = types::method_return(sources, id)?;
            let because = if returned.declared {
                Because::Declared
            } else {
                Because::Followed(Box::new(returned.typed.derivation.clone()))
            };
            let declared = returned.typed;
            Some(Hint {
                family: Family::Return,
                at,
                // **`!` is this `def`'s own `raise`**, the one the label sits on: a reopened
                // method's other `def`s have margins of their own.
                label: format!(
                    " -> {}",
                    readable(&declared, render::returned(graph, &declared, raises)?)?
                ),
                because,
            })
        })
        .collect()
}

/// Whether Ruby's own call syntax throws this method's return value away.
///
/// Two cases, one rule, and the rule is about the **language**, not the body: for these, the
/// expression a caller writes does not evaluate to what the method returned, so a margin stating it
/// would be true but unreachable.
///
/// - **`initialize`.** Nobody calls it directly: `Report.new` allocates, runs it, **discards its
///   value** and returns the object. Keying on the name is not invented here: Ruby makes this one
///   private by name at definition, which is why `completion::ALWAYS_PRIVATE` already lists it.
/// - **A setter.** `obj.title = v` evaluates to `v` whatever `def title=` returns, and so does
///   `obj[k] = v`, with no exceptions. The name ends in `=` and the byte before is not one of
///   `=<>!`, which is exactly Ruby's grammar for `title=` versus `==`, `!=`, `<=`, `>=`, `===`.
///
/// **This is the margin's rule, not the module's.** `types::method_return` still answers for both, so
/// a hover card on a setter still says what the body returns, and a chain through one still steps.
/// The refusal lives here because a hint is the answer nobody asked for and has a higher bar than a
/// card. Constructors hit it most, because their last line is usually an assignment, which returns
/// its value.
///
/// The setter rule is permanent: the honest label would be the argument's type, which nothing
/// declares.
fn value_is_discarded(name: &str) -> bool {
    if name == "initialize" {
        return true;
    }
    match name.as_bytes() {
        [.., before, b'='] => !matches!(before, b'=' | b'<' | b'>' | b'!'),
        _ => false,
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    /// What a generic was written holding, drawn beside its head.
    ///
    /// `Typed` carries a generic's positions, and this is where the margin spells them:
    /// `"a,b".scan(",")` and `[1, 2].map { |n| "x" }` draw `Array[String]`, not a bare `Array`.
    /// A `-> bot` label is drawn: its code says every path raises, which is no guess.
    #[test]
    fn a_label_on_a_method_that_never_returns_is_no_guess() {
        let hint = Hint {
            family: Family::Return,
            at: 0,
            label: " -> bot".to_owned(),
            because: Because::Raises,
        };
        assert_eq!(hint.tier(), Tier::Derived);
    }

    #[test]
    fn a_generic_is_drawn_holding_what_it_was_written_holding() {
        let source = "\
class Ledger
  def use
    parts = \"a,b\".scan(\",\")
    names = [1, 2].map { |n| \"x\" }
    blanks = [1, 2].map { }
    keys = { \"a\" => 1 }.keys
    table = \"hi\".table
    mixed = [1, \"x\"].first(1)
    plain = \"hi\".upcase
  end
end
";
        // A position the harvest refused is `untyped` and keeps its place; a blank would break the
        // rule, because `Hash[untyped]` would read as a hash of one thing.
        let (mut harness, uri) = with_declared_types(
            source,
            "class String\n  def table: () -> Hash[String, untyped]\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def use -> String
    parts: Array[String] = \"a,b\".scan(\",\")
    names: Array[String] = [1, 2].map { |n| \"x\" }
    names = [1, 2].map { |n: Integer| \"x\" }
    blanks: Array[nil] = [1, 2].map { }
    keys: Array[String] = { \"a\" => 1 }.keys
    table: Hash[String, untyped] = \"hi\".table
    mixed: Array = [1, \"x\"].first(1)
    plain: String = \"hi\".upcase"
        );
    }

    /// A workspace whose signatures are [`TYPED_RBS`] plus `rbs`, holding one Ruby file.
    ///
    /// [`with_types`]'s shape with a second half: the hint families read what a signature says
    /// about the *user's own* classes, which nothing that types core receivers can stand in for.
    fn with_declared_types(source: &str, rbs: &str) -> (Harness, DocUri) {
        with_declared_types_and_config(source, rbs, "")
    }

    fn with_declared_types_and_config(source: &str, rbs: &str, config: &str) -> (Harness, DocUri) {
        let mut harness = signed(&[("core/core.rbs", &format!("{TYPED_RBS}\n{rbs}"))], config);
        let uri = harness.write("app/report.rb", source);
        harness.index();
        harness.index_gems();
        (harness, uri)
    }

    const HINTS: &str = "\
class Person
  def shout
  end
end

class Report
  def initialize
    @title = \"hi\".upcase
  end

  def headline
  end

  def bindings(first, second)
    plain = \"hi\"
    derived = \"hi\".upcase
    chained = \"hi\".upcase.length
    made = Report.new
    guessed = person.shout
    from_ivar = @title.length
    bare = @title
    \"hi\".bytes do |byte|
      byte
    end
    each do |unyielded|
      unyielded
    end
  end

  def label name
  end

  def chainable
  end

  def slug=(value)
    @slug = value.to_s
  end
end
";

    /// How many bindings [`HINTS`] holds, whether or not each is worth saying.
    ///
    /// Seven locals and two block parameters, all inside `def bindings`. Not the hint count (most
    /// are refused by [`worth_saying`] or by tier) but the number of candidates a whole-document
    /// request classifies, which `hints_answer_for_the_range_they_were_asked_about` counts.
    const BINDINGS_IN_HINTS: usize = 9;

    const HINTS_RBS: &str = "\
class Person
  def shout: () -> String
end

class Report
  def headline: () -> String
  def bindings: (untyped first, untyped second) -> Integer
  def label: (untyped name) -> String
  def chainable: () -> self
end
";

    #[test]
    fn every_inlay_hint_in_one_file_drawn_side_by_side() {
        // `GALLERY`'s treatment for the answer nobody asks for. Read the margin down the page: what
        // is *absent* is as much the assertion as what is drawn, and the absences come from
        // different rules.
        //
        // - `plain = "hi"` and `made = Report.new` state the class on the line. A label repeating
        //   it is noise, and those are exactly the resolved bindings, which is why no hint here is
        //   resolved.
        // - `guessed = person.shout` types to `Person` from six letters and nothing else. Its card
        //   says so and is read on purpose; the margin refuses it outright.
        // - `each do |unyielded|` writes no receiver, so it is a `yield`, not a call, and what a
        //   `yield` hands over comes from the method body, not a signature. Nothing to read.
        // - `def initialize` and `def slug=` both have a readable body (each ends in an assignment,
        //   which returns its value), and neither draws, because Ruby's call syntax discards that
        //   value: `Report.new` returns the object, and `report.slug = v` returns `v`.
        //   `value_is_discarded` is the rule, and only the margin's: a card over either still shows
        //   what the body returns.
        // - `def chainable` is declared `-> self`, which at the `def` means the class the label
        //   would be written inside. Repeating the line above is the first rule again, one
        //   construct out.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);

        assert_eq!(
            drawn_hints(HINTS, &harness.hints_in(&uri)),
            "  def shout -> String
  def headline -> String
  def bindings(first, second) -> Integer
    derived: String = \"hi\".upcase
    chained: Integer = \"hi\".upcase.length
    from_ivar: Integer = @title.length
    bare: String = @title
    \"hi\".bytes do |byte: Integer|
  def label name -> String"
        );
    }

    /// A `raise` is no exit's value, and a method whose own code writes one is marked `!`, after
    /// any `?`. The mark is the method's: what it returned, held in a variable, is not marked.
    #[test]
    fn a_method_that_writes_raise_is_marked_and_keeps_its_type() {
        let source = "\
class Gate
  def pick(x)
    return \"a\" if x
    raise ArgumentError, \"no\"
  end

  def either(x)
    x ? \"a\" : fail(\"no\")
  end

  def guard(x)
    raise ArgumentError unless x
    \"a\"
  end

  def maybe(x)
    return nil if x
    raise \"no\" if x == 1
    \"a\"
  end

  def listed(items)
    items.each { |item| raise \"no\" if item }
    \"a\"
  end

  def plain(x)
    return \"a\" if x
    \"b\"
  end

  def outer
    def inner
      raise \"no\"
    end
    \"a\"
  end

  def never
    raise NotImplementedError
  end

  def declared
    raise \"no\" if @off
    1
  end

  def sure(x)
    maybe(x) || raise(\"no\")
  end

  def gone
    nil || raise(\"no\")
  end

  def use
    held = maybe(1)
    names = [1, 2].map { |n| n ? \"x\" : raise(\"no\") }
    raised = [1, 2].map { raise(\"no\") }
  end
end
";
        // `either`'s signature says nothing of its return, so the card reads the body, and the
        // mark is asked of both definitions: the signature's has no body, the `def`'s raises.
        let (mut harness, uri) = with_declared_types(
            source,
            "class Gate\n  def declared: () -> Integer\n  def either: (untyped) -> untyped\nend\n",
        );
        let drawn = drawn_hints(source, &harness.hints_in(&uri));
        assert_eq!(
            drawn,
            "  def pick(x) -> String!
  def either(x) -> String!
  def guard(x) -> String!
  def maybe(x) -> String?!
  def listed(items) -> String!
  def plain(x) -> String
  def outer -> String
    def inner -> bot
  def never -> bot
  def declared -> Integer!
  def sure(x) -> String!
  def use -> Array!
    held: String? = maybe(1)
    names: Array[String] = [1, 2].map { |n| n ? \"x\" : raise(\"no\") }
    names = [1, 2].map { |n: Integer| n ? \"x\" : raise(\"no\") }
    raised: Array = [1, 2].map { raise(\"no\") }"
        );

        // The card says what the margin says, and so does a call's.
        let card = card(&mut harness, &uri, source, "maybe(1)");
        assert!(
            card.contains("Gate#maybe(Integer | untyped x) -> String?!"),
            "{card}"
        );
        let signed = harness.hover_at(&uri, source, "either(x)");
        let signed = signed["contents"]["value"].as_str().unwrap_or_default();
        assert!(signed.contains("-> String!"), "{signed}");
        let card = harness.hover_at(&uri, source, "plain(x)");
        let card = card["contents"]["value"].as_str().unwrap_or_default();
        assert!(card.contains("Gate#plain(x) -> String\n"), "{card}");
    }

    /// A hint request reads its document **once**: the bindings, every `def`'s label and what the
    /// type side reads out of the text come from one parse, and a second request over the same
    /// text parses it once more only for its window.
    #[test]
    fn a_hint_request_parses_its_document_once() {
        let source = "\
class Ledger
  def total(x)
    rows = [1, 2].map { |n| n.to_s }
    rows.first
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        cursor::parses_taken();
        let first = harness.hints_in(&uri);
        let cold = cursor::parses_taken();
        let second = harness.hints_in(&uri);
        let warm = cursor::parses_taken();
        assert_eq!(first, second);
        assert_eq!((cold, warm), (1, 1));
    }

    #[test]
    fn a_union_over_many_including_classes_is_not_spelled_in_the_margin() {
        // A value that is one of five classes is drawn: the width is the value's. The same width
        // made of a concern's includers is not ([`MAX_INCLUDERS_SPELLED`]), and four of them are.
        let (mut harness, _, _) = models_project("");
        harness.write("app/models/application_record.rb", CONCERNS);
        let concern = "\
module Stamped
  included do
    before_save do
      stamp = marker
    end
  end
end
";
        let uri = harness.write("app/models/concerns/stamped.rb", concern);
        let few = "\
module Few
  included do
    before_save do
      stamp = marker
    end
  end
end
";
        let few_uri = harness.write("app/models/concerns/few.rb", few);
        for (index, class) in ["Aa", "Bb", "Cc", "Dd", "Ee"].into_iter().enumerate() {
            let included = if index < 4 {
                "Stamped\n  include Few"
            } else {
                "Stamped"
            };
            harness.write(
                &format!("app/models/{}.rb", class.to_lowercase()),
                &format!(
                    "class {class} < ApplicationRecord\n  include {included}\n\n  def marker\n    \
                     {class}.new\n  end\nend\n"
                ),
            );
        }
        let numbers = "\
class A; end
class B; end
class C; end
class D; end
class E; end

def tower(n)
  case n
  when 1 then A.new
  when 2 then B.new
  when 3 then C.new
  when 4 then D.new
  else E.new
  end
end
";
        let tower = harness.write("lib/tower.rb", numbers);
        harness.index();
        assert_eq!(drawn_hints(concern, &harness.hints_in(&uri)), "null");
        assert_eq!(
            drawn_hints(few, &harness.hints_in(&few_uri)),
            "      stamp: Aa | Bb | Cc | Dd = marker"
        );
        assert!(
            drawn_hints(numbers, &harness.hints_in(&tower)).contains("def tower(n) ->"),
            "{}",
            drawn_hints(numbers, &harness.hints_in(&tower))
        );
    }

    #[test]
    fn a_type_matched_on_a_name_alone_is_never_drawn_in_the_margin() {
        // The refusal, asserted **by tier, not by example**: the same expression is asked twice, as
        // a card and as a label. The card names `Person#shout` and says in its last line what that
        // rests on (the six letters of `person`), because a card is read on purpose and has room to
        // qualify itself. The margin has no room, is read as fact, and is drawn on every line, so
        // the bottom tier never appears in it.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);

        let card = harness.hover_at(&uri, HINTS, "shout\n    from");
        let card = card["contents"]["value"].as_str().unwrap_or("null");
        assert!(card.contains("Person#shout"), "{card}");
        assert!(card.contains("Guessed from name alone"), "{card}");

        assert!(
            !drawn_hints(HINTS, &harness.hints_in(&uri)).contains("guessed"),
            "a guess reached the margin"
        );
    }

    #[test]
    fn a_mailers_template_is_a_margin_the_renderer_rung_reaches() {
        // `inlayHint` needs nothing extra for a template, so the view↔renderer rung (the controller
        // a template's directory names, or the mailer where there is no controller) decides what is
        // painted into the margin. That rung widening to mailer views adds answers here too, and a
        // template's card is what a label here is built from.
        //
        // The population moves, not the tier. Every such answer is a convention, therefore
        // *Derived*, which is already drawn; the refusal below is the same test on [`Tier`], in the
        // same file and request.
        let mut harness = signed(
            &[(
                "core/core.rbs",
                &format!(
                    "{TYPED_RBS}\nclass Story\n  def headline: () -> String\nend\n\n\
                 class Person\n  def shout: () -> String\nend\n"
                ),
            )],
            "",
        );
        harness.write(
            "app/models/story.rb",
            "class Story\n  def headline\n  end\nend\n",
        );
        harness.write(
            "app/models/person.rb",
            "class Person\n  def shout\n  end\nend\n",
        );
        harness.write(
            "app/mailers/application_mailer.rb",
            "class ApplicationMailer\nend\n",
        );
        harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  def digest\n    @story = Story.new\n  \
             end\nend\n",
        );
        let template = "<% headline = @story.headline %>\n<% shouted = @person.shout %>\n";
        let view = harness.write("app/views/user_mailer/digest.html.erb", template);
        harness.index();
        harness.index_gems();

        // `@story` is the mailer's, reached through a convention that names the class and the line,
        // so the local it types is drawn. `@person` is six letters and nothing else, and the margin
        // is where that answer may not appear.
        assert_eq!(
            drawn_hints(template, &harness.hints_in(&view)),
            "<% headline: String = @story.headline %>"
        );
    }

    #[test]
    fn a_hint_is_its_label_with_nothing_to_resolve() {
        // No surface says how a type was found (decided 2026-09-29), so a hint carries neither a
        // tooltip nor the `data` a client would send back to fetch one.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);
        let hints = harness.hints_in(&uri);
        let hints = hints.as_array().expect("hints");
        assert!(!hints.is_empty());
        for hint in hints {
            assert_eq!(hint["tooltip"], serde_json::Value::Null, "{hint}");
            assert_eq!(hint["data"], serde_json::Value::Null, "{hint}");
        }
    }

    #[test]
    fn hints_answer_for_the_range_they_were_asked_about() {
        // The protocol asks for the window the editor shows, and the range bounds the *work*:
        // `cursor::bindings_in` takes it, so a chain outside the window is never classified, rather
        // than classified and dropped.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);

        assert_eq!(
            drawn_hints(HINTS, &harness.hints_within(&uri, (15, 0), (16, 99))),
            "    derived: String = \"hi\".upcase
    chained: Integer = \"hi\".upcase.length"
        );
        // The answer above cannot prove that on its own: a version that classified the whole file
        // and filtered afterwards returns the same list. What separates them is work. The window
        // classified the two bindings it drew; the whole file classifies every binding. Counted,
        // not timed: a clock says one thing on a quiet machine and another on a busy one, which is
        // what `cursor::CLASSIFIED` is for.
        assert_eq!(cursor::classifications_taken(), 2);
        harness.hints_in(&uri);
        assert_eq!(cursor::classifications_taken(), BINDINGS_IN_HINTS);

        // A window with no binding answers `null`, not an empty array, as every other list here
        // does: nothing to say is not an empty answer.
        assert_eq!(
            harness.hints_within(&uri, (2, 0), (4, 0)),
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_name_the_call_syntax_discards_the_value_of_is_not_labelled() {
        // `HINTS` pins the two absences beside everything drawn; this is the boundary between them
        // and the five operators that merely *look* like setters. `==`, `===`, `!=`, `<=` and `>=`
        // end in `=`, none is an assignment, and each keeps its label, which is why the rule reads
        // the byte before the `=`, not the `=` alone. `[]=` is on the other side: `report[k] = v`
        // evaluates to `v` exactly like `report.title = v`.
        let source = "\
class Report
  def ==(other)
    \"x\"
  end

  def ===(other)
    \"x\"
  end

  def !=(other)
    \"x\"
  end

  def <=(other)
    \"x\"
  end

  def >=(other)
    \"x\"
  end

  def title=(value)
    \"x\"
  end

  def []=(key, value)
    \"x\"
  end

  def initialize
    \"x\"
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def ==(other) -> String
  def ===(other) -> String
  def !=(other) -> String
  def <=(other) -> String
  def >=(other) -> String"
        );
    }

    #[test]
    fn each_family_of_hint_is_silenced_by_its_own_setting() {
        // Three families, three flags, and no fourth flag for all of them: every client that asks
        // for inlay hints has its own switch, and a duplicate setting would be a second place to
        // look when the margin is empty.
        for (setting, gone) in [
            ("block_parameters", "|byte: Integer|"),
            ("locals", "derived: String"),
            ("returns", "def headline -> String"),
        ] {
            let (mut harness, uri) = with_declared_types_and_config(
                HINTS,
                HINTS_RBS,
                &format!("\n[hints]\n{setting} = false\n"),
            );
            let drawn = drawn_hints(HINTS, &harness.hints_in(&uri));
            assert!(!drawn.contains(gone), "{setting} was set false:\n{drawn}");
            // And only that one: turning off one family must not take another with it.
            assert_eq!(
                drawn.lines().count(),
                match setting {
                    // Nine in all: four returns, four locals and the block's parameter.
                    "returns" => 5,
                    "locals" => 5,
                    _ => 8,
                },
                "{setting} took more than its own family:\n{drawn}"
            );
        }
    }

    #[test]
    fn a_file_the_index_never_saw_still_gets_the_hints_that_need_no_document() {
        // `tmp/**/*` is excluded by default, so this file is on disk, open in the editor, and
        // absent from the graph. The two halves then answer differently, and both are right: a
        // local's type is read from the buffer and looked up by class name, which needs no
        // document, while a `def`'s declared return is keyed by the declaration rubydex filed for
        // it, and there is none. So the margin is thinner, not empty.
        let source = "\
def stray
  derived = \"hi\".upcase
end
";
        let (mut harness, _) = with_declared_types("class Kept\nend\n", "");
        let uri = harness.write("tmp/stray.rb", source);
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  derived: String = \"hi\".upcase"
        );
    }

    #[test]
    fn a_return_the_file_itself_declares_is_not_repeated_in_its_margin() {
        // "RBS declares it and the source does not" is the whole third family, and a YARD tag two
        // lines up is the source declaring it. The type is the same either way (`annotations`
        // generates RBS from the tag just like a `sig/` file), so what tells the two apart is
        // *which document* the declaration was generated from: a pure function of this one's URI.
        let source = "\
class Ledger
  # @return [String]
  def stamped
  end

  def plain
  end
end
";
        let (mut harness, uri) =
            with_declared_types(source, "class Ledger\n  def plain: () -> String\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def plain -> String"
        );
    }

    #[test]
    fn a_method_that_falls_out_of_a_conditional_is_declined_rather_than_labelled() {
        // The margin's half of the fall-through rule: `cursor::returns_in` files the `nil`, and
        // this is what a reader sees. `guarded` is a real method in miniature: one branch, no
        // `else`, so the value on the common path is `nil`. Without the `nil` it would draw a flat
        // `-> String`, wrong on the majority path, and with two unmerged answers it would draw
        // nothing. It draws **`-> String?`**, the whole sentence.
        //
        // The other two are the controls. `both` writes every branch and answers plainly, so the
        // mark is about the missing branch, not conditionals. `empty` has no branch at all, and
        // `nil` is what it returns, not a gap: the one case the mark cannot cover, because there is
        // no other answer to sit on.
        let source = "\
class Ledger
  def guarded
    if stamped?
      \"x\"
    end
  end

  def both
    if stamped?
      \"x\"
    else
      \"y\"
    end
  end

  def empty
    if stamped?
    end
  end
end
";
        // `NilClass` is declared here because `TYPED_RBS` is a handful of classes, not core, and a
        // class the graph never heard of is not a label, as for any other literal.
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def guarded -> String?
  def both -> String
  def empty -> nil"
        );
    }

    #[test]
    fn a_method_whose_guard_bails_with_a_bare_return_is_marked_too() {
        // The same rule in the spelling real guards use. `guarded` is `return if query.blank?`
        // above the work: it returns `nil` whenever the guard takes, so it draws the work's type
        // **with the mark**, the whole sentence rather than half.
        //
        // `always` is the control that keeps the keyword: a `return` with a value answers plainly,
        // so the mark is about the missing value, not `return`. `bare` is the whole method, and
        // `nil` is a right answer, not a gap.
        let source = "\
class Ledger
  def guarded
    return if stamped?
    \"x\"
  end

  def always
    return \"x\" if stamped?
    \"y\"
  end

  def bare
    return
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def guarded -> String?
  def always -> String
  def bare -> nil"
        );
    }

    #[test]
    fn a_method_that_answers_true_or_false_is_labelled_boolean() {
        // Ruby has no boolean class, and RBS calls the pair `bool`. `predicate` writes the two
        // halves in two branches, as an application does; `flagged` in the RBS beside it is
        // declared `bool`, as a signature does (core RBS uses it heavily). Both draw the same word,
        // because they are the same type and the word is already in the bundle.
        //
        // `only_true` is the control, and why the fold is of the **pair**: a method that can only
        // return `true` is not a predicate, and `true` is both narrower than `bool` and correct.
        // `maybe` is both folds at once.
        let source = "\
class Gate
  def predicate
    if stamped?
      true
    else
      false
    end
  end

  def only_true
    true
  end

  def maybe
    return if stamped?
    predicate
  end

  def read
    held = Switch.new.flagged
    held
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\nclass TrueClass\nend\n\nclass FalseClass\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def predicate -> bool
  def only_true -> true
  def maybe -> bool?
  def read -> bool
    held: bool = Switch.new.flagged"
        );
    }

    #[test]
    fn two_signature_documents_declaring_one_method_are_merged_and_never_the_last_one_read() {
        // One method declared on one class by two documents: each document's arms are kept, and the
        // partition is settled over their union, the same computation as for a single document, one
        // level out. Otherwise whichever document was read last would silently win.
        let source = "class Reader
  def both_declare_it
    Widget.new.plus
  end

  def only_the_first
    Widget.new.only_core
  end

  def only_the_second
    Widget.new.only_extra
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Widget\n  def plus: () -> String\n  def only_core: () -> String\nend\n",
        );
        harness.write(
            "sig/extra.rbs",
            "class Widget\n  def plus: () -> Integer\n  def only_extra: () -> Integer\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // `plus` is both documents' answers: two arms of one arity that nothing tells apart
            // are joined (`types::join_arms`), so the label holds whichever document is right.
            // Picking one would be confident and depend only on read order.
            //
            // The other two show this is a merge, not a refusal to read a method two documents
            // mention: a name only one document declares still answers alone.
            "  def both_declare_it -> String | Integer
  def only_the_first -> String
  def only_the_second -> Integer"
        );
    }

    #[test]
    fn a_ruby_alias_and_an_alias_method_are_the_method_they_rename() {
        // An RBS `alias map collect` is the method it renames in all four tables. The two Ruby
        // spellings are the same fact, so `Array#blank?` (ActiveSupport's
        // `alias_method :blank?, :empty?`) answers like `Array#empty? -> bool`, the real
        // declaration one name away in `vendor/rbs`.
        //
        // Both spellings are here because they are two Prism nodes with one meaning, and the
        // singleton pair is here because `alias` can rename a `self.` method too. An alias of a
        // Ruby method nothing declares is that method too: a call of it reads the method's body,
        // and an inherited one is found where the alias is written.
        let source = "class Reader
  def by_keyword
    Widget.new.renamed
  end

  def by_call
    Widget.new.called
  end

  def on_the_singleton
    Widget.built
  end

  def of_nothing
    Widget.new.of_untyped
  end

  def of_a_body
    Widget.new.caption
  end

  def of_an_inherited_body
    Widget.new.titled
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Widget\n  def size: () -> Integer\n  def self.make: () -> String\nend\n",
        );
        harness.write(
            "app/widget.rb",
            "class Widget < Base\n  alias renamed size\n  alias_method :called, :size\n\n  \
             class << self\n    alias built make\n  end\n\n  \
             def anonymous\n    yield\n  end\n  alias of_untyped anonymous\n\n  \
             def label\n    \"x\"\n  end\n  alias_method :caption, :label\n  \
             alias_method :titled, :title\nend\n",
        );
        harness.write(
            "app/base.rb",
            "class Base\n  def title\n    1\n  end\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // `of_nothing` is the refusal that shows this is a copy, not an invention: the target's
            // own row was never filed, so the alias gets nothing instead of a guess.
            "  def by_keyword -> Integer
  def by_call -> Integer
  def on_the_singleton -> String
  def of_a_body -> String
  def of_an_inherited_body -> Integer"
        );
    }

    #[test]
    fn a_ruby_def_answers_what_rbs_writes_for_it_on_a_stand_in() {
        // `SecureRandom.hex` written out: Ruby's `random/formatter.rb` writes `def hex` on
        // `Random::Formatter`, and RBS writes its signature on `RBS::Unnamed::Random_Formatter`,
        // which `module Random::Formatter` includes. The `def` is found first and its body types
        // nothing, so the stand-in's row is the `def`'s own, on either side of what mixes it in,
        // and under a Ruby alias of it.
        //
        // `through_a_module` is the refusal: `Thing` reaches the stand-in only through `Fmt`, so
        // its own `def hex` may be a different method. `its_own_row` keeps the signature `Fmt`
        // writes itself, and a stand-in declared nowhere, or a constant on one, adds nothing.
        let source = "class Reader
  def hexed
    Rng.hex
  end

  def aliased
    Rng.uuid_v4
  end

  def on_the_class_object
    Maker.hex
  end

  def through_a_module
    Thing.new.hex
  end

  def its_own_row
    Rng.base
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "module RBS\n  module Unnamed\n    module Stand_In\n      VERSION: String\n      \
             def hex: (?Integer? n) -> String\n      def uuid: () -> String\n      \
             def base: () -> String\n    end\n  end\nend\n\
             module Fmt\n  include RBS::Unnamed::Stand_In\n  def base: () -> Integer\nend\n\
             module Maker\n  extend RBS::Unnamed::Stand_In\nend\n\
             module Lost\n  include RBS::Unnamed::Nowhere\nend\n",
        );
        harness.write(
            "app/fmt.rb",
            "module Fmt\n  def hex(n = nil)\n    gen(n).unpack1(\"H*\")\n  end\n\n  \
             def uuid\n    gen(16).unpack1(\"H*\")\n  end\n  alias uuid_v4 uuid\n\n  \
             def base\n    gen(1)\n  end\nend\n\n\
             module Rng\n  extend Fmt\nend\n\n\
             module Maker\n  def self.hex(n = nil)\n    gen(n)\n  end\nend\n\n\
             class Thing\n  include Fmt\n\n  def hex\n    gen(1)\n  end\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def hexed -> String
  def aliased -> String
  def on_the_class_object -> String
  def its_own_row -> Integer"
        );
    }

    #[test]
    fn two_gems_defining_one_method_differently_answer_the_union_and_never_one_of_them() {
        // ya-lsp indexes a gem's whole `lib/` instead of tracing real `require`s, so a file almost
        // no application loads contributes a definition as real as one every application loads.
        // `Symbol#as_json` is the live case: ActiveSupport's returns a `String` (`name`), and
        // Ruby's `json` stdlib gem declares an unrelated method of the same name returning a 2-key
        // `Hash`. Both bodies are written out here, splat parameter and all, because the *arity* of
        // the two `def`s differs and their shared declaration does not.
        //
        // **The answer must be the union, never one of the two.** A chain cannot step off a union
        // ([`types::Typed::one`] refuses it), so a confident pick would be a wrong answer where
        // this is a refusal a label can still print.
        let source = "class Reader\n  def read\n    Widget.new.as_json\n  end\n\n  \
                      def agreeing\n    Widget.new.same\n  end\nend\n";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Widget\n  def name: () -> String\n  def to_s: () -> String\nend\n",
        );
        harness.write(
            "app/a.rb",
            "class Widget\n  def as_json(options = nil)\n    name\n  end\n\n  \
             def same\n    to_s\n  end\nend\n",
        );
        harness.write(
            "app/b.rb",
            "class Widget\n  def as_json(*)\n    { \"id\" => to_s, \"s\" => to_s }\n  end\n\n  \
             def same\n    name\n  end\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // And two bodies that *agree* are still one answer, showing the rule above is a fold,
            // not a refusal to read a reopened method.
            "  def read -> String | Hash
  def agreeing -> String"
        );
    }

    #[test]
    fn a_bare_type_name_in_a_signature_is_resolved_from_where_it_was_written() {
        // RBS resolves an unqualified reference the way Ruby resolves a constant: its own enclosing
        // scope first, then each enclosing scope, then the top level. `Thing` written inside
        // `Outer::Holder` names `Outer::Thing`, a *sibling* of `Holder`, not a class nested in it.
        // Looking the name up exactly as spelled would miss it in hand-curated gem signatures.
        let source = "\
class Reader
  def sibling
    Outer::Holder.new.get
  end

  def top_level
    Outer::Holder.new.plain
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "\
class Thing
end

module Outer
  class Thing
  end

  class Holder
    def get: () -> Thing
    def plain: () -> ::Thing
  end
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // The nested one for the bare name, and the top-level one where `::` says so. Fully
            // qualified, which shows the bare name was resolved from `Outer`, not from the top
            // level where the other `Thing` is.
            "  def sibling -> Outer::Thing
  def top_level -> Thing"
        );
    }

    #[test]
    fn a_signature_line_beside_a_real_def_is_not_a_body_with_nothing_in_it() {
        // An RBS `def:` line and a real `def` are two `Definition::Method`s of one declaration. The
        // body walk must not require *both* to produce a readable body: a signature file parsed as
        // Ruby never has an exit at that span, so requiring it would throw away a body the walk
        // already read correctly, and installing real gem signatures would make coverage worse.
        let source = "\
class Reader
  def read
    Widget.new.murky
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            // `untyped` is the signature saying nothing, which is what sends this to the body.
            "\
class Widget
  def murky: () -> untyped
end
",
        );
        harness.write(
            "app/widget.rb",
            "\
class Widget
  def murky
    \"x\"
  end
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def read -> String"
        );
    }

    #[test]
    fn the_weakest_exit_decides_the_tier_of_the_whole_body() {
        // Two exits naming one class, reached two ways: a literal, which the code states outright,
        // and a bare word nothing resolves, matched on the name alone. The class is the same, the
        // *tier* is not, and a guess is never painted into a margin, so the whole label goes
        // instead of half of it being trustworthy.
        let source = "\
class Reader
  def both_tiers(flag)
    return \"literal\" if flag
    string
  end

  def resolved_only(flag)
    return \"literal\" if flag
    \"other\"
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // `both_tiers` is a `String` by both exits and is drawn by neither.
            "  def resolved_only(flag) -> String"
        );
    }

    #[test]
    fn two_arms_one_arity_are_told_apart_by_what_the_call_passed() {
        // `vendor/rbs/core/integer.rbs` declares `Integer#+` four ways (`(Integer) -> Integer`,
        // `(Float) -> Float`, `(Rational) -> Rational`, `(Complex) -> Complex`), and bigdecimal
        // reopens `Integer` for a fifth. One arity, five answers, so the bucket disagrees and
        // `1 + 2` would have no type. What tells them apart is written three characters away: the
        // argument.
        //
        // `through_an_ancestor` is the case an equality test would miss, and why the comparison
        // walks the chain: `Float#+` declares `(Numeric) -> Float`, and `1.5 + 1` hands it an
        // `Integer`.
        //
        // The next four pick no arm, and must not: one of the arms still runs, so each is the
        // union of every arm that takes one argument (`types::join_arms`), never one of them.
        //
        // 1. An arm whose parameter this cannot read cannot be ruled out.
        // 2. An arm with an *optional* positional has no fixed position-to-parameter map.
        // 3. A call writing no argument has nothing to pick with.
        // 4. Two arms fitting alike are the disagreement the partition already refused, from the
        //    other side.
        //
        // The last is a refusal and must stay one: a splat is a count nothing here can know, so
        // no set of arms is known to hold the answer.
        let source = "\
class Reader
  def by_the_first
    Widget.new.pick(1)
  end

  def by_the_second
    Widget.new.pick(\"x\")
  end

  def through_an_ancestor
    Widget.new.wider(Derived.new)
  end

  def one_arm_unreadable
    Widget.new.murky(1)
  end

  def an_optional_lines_up_with_nothing
    Widget.new.loose(1)
  end

  def nothing_to_pick_with
    Widget.new.bare
  end

  def two_arms_fit_alike
    Widget.new.twinned(1)
  end

  def a_splat_cannot_be_counted
    Widget.new.pick(*args)
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "\
class Base
end

class Derived < Base
end

class Widget
  def pick: (Integer) -> String
          | (String) -> Integer
  def wider: (Base) -> String
           | (Integer) -> Float
  def murky: (Integer) -> String
           | (untyped) -> Integer
  def loose: (?Integer) -> String
           | (?String) -> Integer
  def bare: () -> String
          | () -> Integer
  def twinned: (Integer) -> String
             | (Integer) -> Float
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def by_the_first -> String
  def by_the_second -> Integer
  def through_an_ancestor -> String
  def one_arm_unreadable -> String | Integer
  def an_optional_lines_up_with_nothing -> String | Integer
  def nothing_to_pick_with -> String | Integer
  def two_arms_fit_alike -> String | Float"
        );
    }

    #[test]
    fn a_signature_two_gems_bodies_dispute_answers_the_union_and_never_the_signature_alone() {
        // The live shape. `Symbol#as_json` is declared by `vendor/rbs/stdlib/json/0/json.rbs`
        // (Ruby's signature for the opt-in `json/add/symbol.rb` feature, which reopens thirteen
        // core classes), so the signature rung would answer `Hash[String, String]` and never read
        // ActiveSupport's real body, for every `as_json` like it in every Rails application.
        //
        // **The signature is joined, not demoted.** Where two gems wrote a body, the signature can
        // describe at most one, so both answers stand and [`types::Typed::one`] makes the pair
        // terminal. `undisputed` is the other half and the whole cost argument: a single body is
        // the one the signature beside it describes (every `sig/` and every `.gem_rbs_collection`
        // entry), and it keeps exactly its answer, even where the body disagrees outright.
        //
        // `agreed` is the quiet half: two bodies, both read, both matching the signature, which is
        // returned untouched instead of rebuilt around the same class. `renamed` shows why an alias
        // is not a second body: it has no body of its own, so it can neither dispute a signature
        // nor be disputed, and the `Integer` its signature declares survives a `name` that returns
        // a `String`.
        //
        // The last two are facets crossing the join: a body with a `nil` exit marks the pair
        // nilable, and a body whose exit is a predicate marks it `bool`. Both are OR-ed, not
        // compared, as `body_return` folds them over its own exits, so neither is lost by the half
        // that did not earn it.
        let source = "\
class Reader
  def disputed
    Widget.new.as_json
  end

  def undisputed
    Widget.new.only_one_body
  end

  def agreed
    Widget.new.two_bodies_one_answer
  end

  def renamed
    Widget.new.by_another_name
  end

  def maybe_missing
    Widget.new.sometimes_nil
  end

  def predicate_body
    Widget.new.answers_a_flag
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "\
class Widget
  def name: () -> String
  def to_s: () -> String
  def as_json: (*untyped) -> Hash[String, String]
  def only_one_body: () -> Integer
  def two_bodies_one_answer: () -> String
  def by_another_name: () -> Integer
  def sometimes_nil: () -> Integer
  def answers_a_flag: () -> Integer
  def flag: () -> bool
end
",
        );
        harness.write(
            "app/a.rb",
            "\
class Widget
  def as_json(options = nil)
    name
  end

  def only_one_body
    name
  end

  def two_bodies_one_answer
    name
  end

  def by_another_name
    name
  end

  def sometimes_nil
    return nil if name
    name
  end

  def answers_a_flag
    flag
  end
end
",
        );
        harness.write(
            "app/b.rb",
            "\
class Widget
  def as_json(*)
    { \"id\" => to_s }
  end

  def two_bodies_one_answer
    to_s
  end

  alias_method :by_another_name, :name

  def sometimes_nil
    to_s
  end

  def answers_a_flag
    flag
  end
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def disputed -> Hash | String
  def undisputed -> Integer
  def agreed -> String
  def renamed -> Integer
  def maybe_missing -> Integer? | String
  def predicate_body -> Integer | bool"
        );
    }

    #[test]
    fn a_bool_receiver_asks_both_halves_and_folds_where_they_answer_differently() {
        // `bool` carries `TrueClass` and means the pair, which is right while the two declare the
        // same names, and ActiveSupport is exactly the gem that breaks that. It reopens both with
        // `blank?` and opposite bodies, so the carrier is no longer arbitrary: through
        // `TrueClass`'s own override, `"x".empty?.blank?` would confidently answer `false` about a
        // value that is really undetermined.
        let source = "\
class Ledger
  def split_answer
    Switch.new.flagged.blank?
  end

  def agreed_answer
    Switch.new.flagged.to_s
  end

  def one_half_only
    true.blank?
  end

  def shared_member
    Switch.new.flagged.itself_name
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\n\
             class TrueClass\n  def blank?: () -> false\n  def to_s: () -> \"true\"\nend\n\n\
             class FalseClass\n  def blank?: () -> true\n  def to_s: () -> \"false\"\nend\n\n\
             class Object\n  def itself_name: () -> String\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // The two halves disagree (`false` against `true`), and the pair they name is exactly
            // `bool`: the honest answer, and the one the receiver already had.
            "  def split_answer -> bool
  def agreed_answer -> String
  def one_half_only -> false
  def shared_member -> String"
        );
    }

    #[test]
    fn folding_the_two_halves_of_a_bool_keeps_the_mark_the_pair_and_the_weaker_tier() {
        // The fold itself, in the three ways the two halves can differ other than by class. Each is
        // the rule the module already applies to two signature arms and two body exits, asked of
        // the two halves of a `bool`.
        let source = "\
class Ledger
  def one_half_is_nilable
    Switch.new.flagged.marked
  end

  def the_halves_are_the_pair
    Switch.new.flagged.paired
  end

  def one_half_is_a_body
    Switch.new.flagged.bodied
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\n\
             class TrueClass\n  def marked: () -> String?\n  def paired: () -> true\n  \
             def bodied: () -> String\nend\n\n\
             class FalseClass\n  def marked: () -> String\n  def paired: () -> false\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );
        // The undeclared half is written in Ruby, so its answer is *derived* from a body, while the
        // other half's is read off a signature. The pair rests on the weaker of the two, which is
        // `body_return`'s and `shortcut`'s rule.
        harness.write(
            "app/halves.rb",
            "class FalseClass\n  def bodied\n    \"x\"\n  end\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // A mark on either half marks the answer; `true` beside `false` is the pair, which is
            // what `bool` means and so folds back to the receiver's own word; and two halves
            // agreeing on `String` agree whichever rung each read it from.
            "  def one_half_is_nilable -> String?
  def the_halves_are_the_pair -> bool
  def one_half_is_a_body -> String"
        );
    }

    #[test]
    fn a_unary_bang_is_folded_against_the_operand_and_answers_bool_when_it_cannot_be() {
        // `!` has the same guarantee as `&&` and `||` (Ruby returns one of two values whatever the
        // operand), so it folds the same way. The three cases: an operand that can only be truthy,
        // one that can only be falsy, and one nothing can say anything about, which `&&` has no
        // equivalent of and which is the one that pays.
        let source = "\
class Ledger
  def not_a_bool
    !Switch.new.flagged
  end

  def not_a_string
    !\"x\"
  end

  def not_unknown
    !@mystery
  end

  def not_nil
    !nil
  end

  def double
    !!Switch.new.flagged
  end

  def blankish
    respond_to?(:empty?) ? !!Switch.new.flagged : false
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\nclass TrueClass\n  def !: () -> false\nend\n\n\
             class FalseClass\n  def !: () -> true\nend\n\n\
             class String\n  def !: () -> false\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def not_a_bool -> bool
  def not_a_string -> false
  def not_unknown -> bool
  def not_nil -> true
  def double -> bool
  def blankish -> bool"
        );
    }

    #[test]
    fn a_read_is_every_write_that_can_reach_it() {
        // The reaching-writes rule, drawn. A read's type is every write that can reach it, folded: a
        // branch adds, a write on every path kills what is above it, a loop brings back what is
        // below, and `nil` joins where nothing has run yet. One write nothing can type refuses
        // the whole read, and a parameter belongs to its own `def` (#13).
        let source = "\
class Probe
  def branch(c)
    value = \"x\"
    value = 1 if c
    value
  end

  def maybe(c)
    value = 1 if c
    value
  end

  def relayed(thing)
    value = \"x\"
    value = thing
    value
  end

  def straight
    value = nil
    value = 1
    value
  end

  def counted(items)
    total = 0
    items.each { |i| total += 1 }
    total
  end

  def in_block(items)
    value = \"a\"
    items.each { value = 1 }
    value
  end

  def nested(c)
    a = 1
    a = \"s\" if c
    b = a
    b
  end

  def writes
    user = \"x\"
    user
  end

  def reads(user)
    user
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Integer\n  def +: (Integer) -> Integer\nend\n",
        );
        // `relayed` and `reads` are refused: `thing` and `user` are parameters nothing types, and
        // `relayed`'s later write is the one that ran (#11). `writes`'s `user` is not `reads`'s
        // (#13). `counted` settles its loop in two rounds: `0`, then `Integer#+` on it.
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def branch(c) -> String | Integer
  def maybe(c) -> Integer?
  def straight -> Integer
  def counted(items) -> Integer
  def in_block(items) -> String | Integer
  def nested(c) -> Integer | String
    b: Integer | String = a
  def writes -> String"
        );
    }

    #[test]
    fn a_check_narrows_the_values_that_reach_a_read() {
        // A check narrows each value that took effect before it, where the check holds:
        // its branch, the right side of `&&` and `||`, and the rest of a statement list after a
        // guard. A write below the check is its own value, a write in a lambda may run after the
        // check, a loop brings back a write the check above it never saw, and a negated
        // conjunction says nothing. A branch whose value starts at a read the check rules every
        // value out of never runs, and adds nothing (`dead_ends`, `dead_branch`); such a read is no
        // name to guess from either (`dead_else`: `object` is not an `Object`), and a value written
        // from one never took effect (`dead_write`: `parts`). A braceless hash is keywords to a
        // `def` taking `...`, never one more positional (`forwarded`).
        let source = "\
class Probe
  def guard(c)
    value = 1 if c
    return unless value
    a = value
  end

  def guard_nil(c)
    value = 1 if c
    raise \"no\" if value.nil?
    value
  end

  def created(c)
    user = \"x\" if c
    user = \"y\" unless user
    user
  end

  def branches(c)
    value = 1 if c
    if value
      a = value
    else
      b = value
    end
    unless value == nil
      d = value
    end
    value.nil? || (e = value)
  end

  def written_between(c, d)
    value = 1 if c
    if value
      value = nil if d
      a = value
    end
  end

  def in_block(c, items)
    value = 1 if c
    return unless value
    items.each { a = value }
  end

  def later(c)
    value = 1 if c
    reset = -> { value = nil }
    raise \"no\" unless value
    reset.call
    value
  end

  def conjunction(c, d)
    value = 1 if c
    other = 1 if d
    if value && other
      a = value
    else
      b = value
    end
    raise \"no\" unless value && other
    value
  end

  def negated(c)
    value = 1 if c
    raise \"no\" if !value
    value
  end

  def kinds(thing)
    case thing
    when Hash
      a = thing
    when String, Symbol
      b = thing
    when \"x\"
      c = thing
    end
    if thing.is_a?(Array)
      d = thing
    end
    raise \"no\" unless thing.kind_of?(Integer)
    thing
  end

  def mixed(c)
    value = c ? 1 : \"s\"
    if value.is_a?(Integer)
      a = value
    else
      b = value
    end
    case value
    when String
      d = value
    else
      e = value
    end
  end

  def checked_write
    if (found = [1].first)
      a = found
    end
  end

  def looped(c, d)
    value = 1 if c
    raise \"no\" unless value
    while d
      a = value
      value = nil
    end
  end

  def parse(value)
    return value if value.is_a?(Integer)
    \"s\"
  end

  def pick(record)
    record.is_a?(Array) ? record.first : record
  end

  def dead_ends
    parse(\"x\")
  end

  def dead_branch
    pick(\"x\")
  end

  def keys_of(object)
    case object
    when Integer
      object
    else
      object
    end
  end

  def dead_else
    keys_of(1)
  end

  def extract(content)
    return content unless content.is_a?(Array)
    parts = content.first
    parts
  end

  def dead_write
    extract(\"x\")
  end

  def forwarding(target, ...)
    target
  end

  def forwarded
    forwarding(1, page: 2)
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Symbol\nend\n\nclass Array[E]\n  def first: () -> E?\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def guard(c) -> Integer?
    a: Integer = value
  def guard_nil(c) -> Integer!
  def created(c) -> String
      a: Integer = value
      b: nil = value
      d: Integer = value
    value.nil? || (e: Integer = value)
  def written_between(c, d) -> Integer?
      a: Integer? = value
    items.each { a: Integer = value }
  def later(c) -> Integer?!
  def conjunction(c, d) -> Integer!
      a: Integer = value
      b: Integer? = value
  def negated(c) -> Integer!
  def kinds(thing) -> Integer!
      a: Hash = thing
      b: String | Symbol = thing
      d: Array = thing
  def mixed(c) -> String | Integer
      a: Integer = value
      b: String = value
      d: String = value
      e: Integer = value
  def checked_write -> Integer?
    if (found: Integer? = [1].first)
      a: Integer = found
      a: Integer? = value
  def parse(value) -> String
  def pick(record) -> String
  def dead_ends -> String
  def dead_branch -> String
  def keys_of(object) -> Integer
  def dead_else -> Integer
  def extract(content) -> String
  def dead_write -> String
  def forwarding(target, ...) -> Integer
  def forwarded -> Integer"
        );
    }

    #[test]
    fn a_check_is_read_by_ruby_s_rules() {
        // A check's other spellings. `!=`, `instance_of?`, an `elsif`, an `unless … else`, a
        // ternary, parentheses, `and`/`or` guards, a `case` on a write with `when nil`, and the
        // checks that say nothing: a module, a constant that names no class, a negated
        // `instance_of?`, a check on a call, a `case` with no subject, and a `when` of literals
        // (its `else` included).
        let source = "\
class Probe
  def spellings(c, d)
    value = 1 if c
    unless value != nil
      a = value
    end
    if value.nil?
      b = value
    elsif d
      e = value
    else
      f = value
    end
    unless value
      g = value
    else
      h = value
    end
    i = (value) ? value : 0
  end

  def guards(c, d)
    value = 1 if c
    value.nil? and raise \"no\"
    a = value
    other = 1 if d
    other or raise \"no\"
    b = other
    third = 1 if d
    third ||= 2
    unless third
      raise \"no\"
    else
      c = third
    end
    fourth = 1 if d
    fourth or puts(1)
    unless fourth
      puts(2)
    end
    e = fourth
  end

  def kinds(thing, c)
    if thing.instance_of?(Hash)
      a = thing
    end
    unless thing.instance_of?(Hash)
      b = thing
    end
    if thing.is_a?(Enumerable) || thing.is_a?(Missing)
      d = thing
    end
    case (found = [c].first)
    when nil
      e = found
    when Integer
      f = found
    end
    value = [c].first
    if value.is_a?(Kernel)
      g = value
    end
    if value.is_a?(Object)
      h = value
    end
    number = 1 if c
    case
    when number.nil?
      i = number
    end
    case number
    when \"one\"
      j = number
    else
      k = number
    end
    case number
    when Integer
    when nil
      l = number
    end
    unless number.is_a?(Kernel)
      m = number
    end
    if number.nil? { 1 }
      n = number
    end
    unless !number { 1 }
      o = number
    end
  end
end
";
        let (mut harness, uri) =
            with_declared_types(source, "class Array[E]\n  def first: () -> E?\nend\n");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def spellings(c, d) -> Integer
      a: nil = value
      b: nil = value
      e: Integer = value
      f: Integer = value
      g: nil = value
      h: Integer = value
    i: Integer = (value) ? value : 0
  def guards(c, d) -> Integer?!
    a: Integer = value
    b: Integer = other
      c: Integer = third
    e: Integer? = fourth
  def kinds(thing, c) -> Integer?
      a: Hash = thing
      e: nil = found
      f: Integer = found
      h: Object = value
      i: Integer? = number
      j: Integer? = number
      k: Integer? = number
      l: nil = number
      m: Integer? = number
      n: Integer? = number
      o: Integer? = number"
        );
    }

    #[test]
    fn a_check_keeps_what_a_class_can_be() {
        // A check's class half: `is_a?` keeps a class below the one named, makes one above it
        // (or a module) that class, and rules out one beside it, a class object included (`l`);
        // its negation drops what is below.
        // And the guards whose other branch writes, the checks that say nothing (`&.`, `!= 1`, a
        // variable class, a module), and a write in the same block as its check.
        let source = "\
class Probe
  def classes(c)
    pet = Zoo.new.pet
    if pet.is_a?(Dog)
      a = pet
    end
    if pet.is_a?(Rock)
      b = pet
    end
    unless pet.is_a?(Dog)
      d = pet
    end
    if pet.is_a?(Walker)
      e = pet
    end
    either = c ? Zoo.new.pet : Zoo.new.walker
    if either.is_a?(Dog)
      f = either
    end
    mixed = c ? Dog.new : Rock.new
    unless mixed.is_a?(Dog)
      g = mixed
    end
    flag = c ? true : false
    if flag.is_a?(TrueClass)
      h = flag
    end
    unless flag
      i = flag
    end
    maybe = Zoo.new.pet if c
    case maybe
    when Dog, nil
      j = maybe
    else
      k = maybe
    end
    holder = c ? Dog : Rock.new
    if holder.is_a?(Rock)
      l = holder
    end
  end

  def nothing_said(c, klass, thing)
    value = 1 if c
    if value&.nil?
      a = value
    end
    if value != 1
      b = value
    end
    if value.is_a?(klass)
      d = value
    end
    if thing
      e = thing
    end
  end

  def writing_guards(c, items)
    a = 1 if c
    a ||= 2 unless a
    x = a
    b = 1 if c
    b &&= 2 if b
    y = b
    d = 1 if c
    d += 1 unless d.nil?
    z = d
    e = 1 if c
    e.nil? and e = 5
    w = e
    f = 1 if c
    if f
      n = 1
    else
      raise \"no\"
    end
    v = f
    items.each do |item|
      g = 1 if c
      next unless g
      u = g
    end
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Animal\nend\n\nclass Dog < Animal\nend\n\nclass Rock\nend\n\n\
             module Walker\nend\n\n\
             class Zoo\n  def pet: () -> Animal\n  def walker: () -> Walker\nend\n\n\
             class Integer\n  def +: (Integer) -> Integer\nend\n",
        );
        // `y` and `z`: a modifier's body is written before its predicate and runs after it, so
        // its own write is not narrowed by the check.
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def classes(c) -> Rock?
    pet: Animal = Zoo.new.pet
      a: Dog = pet
      d: Animal = pet
      e: Animal = pet
    either: Animal | Walker = c ? Zoo.new.pet : Zoo.new.walker
      f: Dog = either
      g: Rock = mixed
      h: true = flag
      i: false = flag
    maybe: Animal = Zoo.new.pet if c
      j: Dog? = maybe
      k: Animal = maybe
      l: Rock = holder
      a: Integer? = value
      b: Integer? = value
      d: Integer? = value
    x: Integer = a
    y: Integer? = b
    z: Integer? = d
    w: Integer = e
    v: Integer = f
      u: Integer = g"
        );
    }

    #[test]
    fn a_block_on_a_value_that_may_be_nil_is_handed_nil_too() {
        // `nil.then { |v| }` hands `v` a `nil`, so a `T?` receiver hands the block a `T?` wherever
        // `nil` has the method. Where it does not, `nil.each` raises and the block never runs on
        // `nil`. A `&.` call never runs the block on `nil` at all (#4).
        let source = "\
class Shelf
  def fetched
    rows = maybe
    rows.then { |v| v }
  end

  def skipped
    rows = maybe
    rows&.then { |v| v }
  end

  def listed
    rows = named
    rows.each { |r| r }
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Shelf\n  def maybe: () -> Array[String]?\n  def named: () -> Array[String]?\nend\n\n\
             class Object\n  def then: [U] () { (self) -> U } -> U\nend\n",
        );
        // `then`'s answer is the block's value whole, `?` included (#18).
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def fetched -> Array[String]?
    rows: Array[String]? = maybe
    rows.then { |v: Array[String]?| v }
  def skipped -> Array[String]?
    rows: Array[String]? = maybe
    rows&.then { |v: Array[String]| v }
  def listed -> Array[String]
    rows: Array[String]? = named
    rows.each { |r: String| r }"
        );
    }

    #[test]
    fn a_type_argument_that_is_not_exactly_a_class_is_not_claimed() {
        // An argument is held as a bare class, so `String?` and `bool` cannot be held, and a
        // position holding a guess at them would say its elements are never `nil` (#18). The
        // position stays and is drawn `untyped`; the head is still right.
        let source = "\
class Shelf
  def titles
    rows = optional_names
    rows.each { |row| row }
  end

  def flags
    all = checks
    all.each { |flag| flag }
  end

  def upper(rows)
    rows.map { |row| row }
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Shelf\n  def optional_names: () -> Array[String?]\n  def checks: () -> Array[bool]\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def titles -> Array
    rows: Array = optional_names
  def flags -> Array
    all: Array = checks"
        );
    }

    #[test]
    fn a_parameter_is_what_its_declaration_says_and_never_its_default() {
        // A default says what the parameter holds when the caller passed nothing, and a caller
        // may pass anything (#15).
        let source = "\
class Shelf
  def defaulted(limit = 10)
    limit
  end

  def declared(limit)
    limit
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Shelf\n  def declared: (Integer limit) -> untyped\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def declared(limit) -> Integer"
        );
    }

    #[test]
    fn two_variables_that_feed_each_other_in_a_loop_settle_together() {
        // `a` reads `b` and `b` reads `a` on the next turn: a cycle through two reads, where the
        // inner one is answered only once the outer settles. `nil` alone is an answer too.
        let source = "\
class Mutual
  def swap(items)
    a = 1
    b = \"s\"
    items.each do
      a = b
      b = a
    end
    a
  end

  def only_nil(c)
    x = nil if c
    x
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def swap(items) -> Integer | String
      a: String = b
      b: String = a
  def only_nil(c) -> nil"
        );
    }

    /// A workspace whose signatures are [`TYPED_RBS`], holding every file in `files`.
    fn with_files(files: &[(&str, &str)]) -> (Harness, Vec<DocUri>) {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        let uris = files
            .iter()
            .map(|(path, source)| harness.write(path, source))
            .collect();
        harness.index();
        harness.index_gems();
        (harness, uris)
    }

    /// The hints drawn in `files[at]`.
    fn drawn_in(files: &[(&str, &str)], at: usize) -> String {
        let (mut harness, uris) = with_files(files);
        drawn_hints(files[at].1, &harness.hints_in(&uris[at]))
    }

    const BASE_ITEM: (&str, &str) = (
        "app/models/base_item.rb",
        "\
class BaseItem
  def load
    @label = \"x\"
  end

  def base_label
    @label
  end
end
",
    );

    const ITEM: (&str, &str) = (
        "app/models/item.rb",
        "\
class Item < BaseItem
  include Labelled

  def count
    @label = 1
  end

  def show
    @label
  end
end
",
    );

    const ITEM_EXTRAS: (&str, &str) = (
        "app/models/item_extras.rb",
        "\
class Item
  def weight
    @label = 1.5
  end
end
",
    );

    const LABELLED: (&str, &str) = (
        "app/models/labelled.rb",
        "\
module Labelled
  def tag
    @label
  end
end
",
    );

    #[test]
    fn an_instance_variable_is_every_write_any_class_of_its_object_makes() {
        // #16. An `Item` runs `BaseItem#load`, its own `count`, the `weight` another file reopens
        // it with, and `Labelled#tag`: every write reaches every read, whichever class the read is
        // written in. The superclass's read sees the subclass's writes too, because its object may
        // be an `Item`; the module's, because every object it reaches is one.
        let files = [BASE_ITEM, ITEM, ITEM_EXTRAS, LABELLED];
        let every = "String? | Integer | Float";
        assert_eq!(
            drawn_in(&files, 1),
            format!("  def count -> Integer\n  def show -> {every}")
        );
        assert_eq!(
            drawn_in(&files, 0),
            format!("  def load -> String\n  def base_label -> {every}")
        );
        assert_eq!(drawn_in(&files, 3), format!("  def tag -> {every}"));
    }

    #[test]
    fn a_superclass_never_hears_about_a_subclass_it_cannot_be() {
        // The fold is over the object's classes, not every class that spells the name: a sibling
        // of `Item` writes its own `@label`, and no `Item` runs its methods.
        let sibling = (
            "app/models/other_item.rb",
            "class OtherItem < BaseItem\n  def count\n    @label = [1]\n  end\nend\n",
        );
        let files = [BASE_ITEM, ITEM, ITEM_EXTRAS, LABELLED, sibling];
        assert_eq!(
            drawn_in(&files, 1),
            "  def count -> Integer\n  def show -> String? | Integer | Float"
        );
        // The superclass's object may be either, so it hears both.
        assert!(
            drawn_in(&files, 0).contains("def base_label -> String? | Integer | Float | Array"),
            "{}",
            drawn_in(&files, 0)
        );
    }

    #[test]
    fn a_setter_writes_what_its_calls_pass_and_nothing_where_one_is_unplaced() {
        // `attr_accessor :name` is a write no `@name =` spells, of whatever its calls pass
        //. `initialize` writes a `String` and a call on an `Account` an `Integer`:
        // both are drawn. A call on a receiver only its name guesses (`account`) may be on any
        // object passing anything, so there nothing is claimed.
        let account = (
            "app/models/account.rb",
            "\
class Account
  attr_accessor :name

  def initialize
    @name = \"x\"
  end

  def shout
    @name
  end
end
",
        );
        let typed = (
            "app/services/renamer.rb",
            "class Renamer\n  def rename\n    Account.new.name = 1\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[account, typed], 0),
            "  def shout -> String | Integer"
        );
        let untyped = (
            "app/services/renamer.rb",
            "class Renamer\n  def rename(account)\n    account.name = 1\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[account, untyped], 0), "null");

        // Declared in another class of the object and never called, the setter adds nothing.
        let base = (
            "app/models/named.rb",
            "class Named\n  attr_writer :title\nend\n",
        );
        let post = (
            "app/models/post.rb",
            "\
class Post < Named
  def initialize
    @title = \"x\"
  end

  def title
    @title
  end
end
",
        );
        assert_eq!(drawn_in(&[base, post], 1), "  def title -> String");
    }

    #[test]
    fn nil_is_left_out_only_when_every_class_of_the_object_initializes_the_variable() {
        // `Child#initialize` runs `super` as a statement of its own body, so a `Child` has `@seed`
        // set as surely as a `Parent`.
        let parent = (
            "app/models/parent.rb",
            "\
class Parent
  def initialize
    @seed = \"x\"
  end

  def seed
    @seed
  end
end
",
        );
        let child = (
            "app/models/child.rb",
            "\
class Child < Parent
  def initialize
    super
    @extra = 1
  end
end
",
        );
        assert_eq!(drawn_in(&[parent, child], 0), "  def seed -> String");

        // An `Orphan` never runs `Parent#initialize`, so `seed` on one is `nil`.
        let orphan = (
            "app/models/orphan.rb",
            "\
class Orphan < Parent
  def initialize
    @extra = 1
  end
end
",
        );
        assert_eq!(
            drawn_in(&[parent, child, orphan], 0),
            "  def seed -> String?"
        );
    }

    #[test]
    fn an_ancestor_rubydex_could_not_resolve_refuses_the_read() {
        // `Missing::Base` may write `@x`, and nothing here can say what with.
        let widget = (
            "app/models/widget.rb",
            "\
class Widget < Missing::Base
  def load
    @x = \"x\"
  end

  def x
    @x
  end
end
",
        );
        assert_eq!(drawn_in(&[widget], 0), "  def load -> String");
    }

    #[test]
    fn a_subclass_only_the_suite_loads_is_not_one_the_application_s_object_can_be() {
        let gadget = (
            "app/models/gadget.rb",
            "\
class Gadget
  def load
    @x = \"x\"
  end

  def x
    @x
  end
end
",
        );
        let fake = (
            "spec/support/fake_gadget.rb",
            "class FakeGadget < Gadget\n  def fake\n    @x = 1\n  end\n\n  def peek\n    @x\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[gadget, fake], 0),
            "  def load -> String\n  def x -> String?"
        );
        // Nor is a reopening the suite makes of the class itself.
        let reopened = (
            "spec/support/gadget_extras.rb",
            "class Gadget\n  def hack\n    @x = [1]\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[gadget, fake, reopened], 0),
            "  def load -> String\n  def x -> String?"
        );
        // The suite's own read is of an object the suite builds, which may be either.
        assert_eq!(
            drawn_in(&[gadget, fake], 1),
            "  def fake -> Integer\n  def peek -> String? | Integer"
        );
    }

    #[test]
    fn a_class_object_s_variable_is_every_write_its_class_and_subclasses_make_on_that_side() {
        // `SpecialRegistry.seed` writes the `@entries` of `SpecialRegistry`, which
        // `Registry.entries` reads when called on the subclass. The instance's `@entries` is a
        // different variable.
        let registry = (
            "app/models/registry.rb",
            "\
class Registry
  def self.entries
    @entries
  end

  def self.reset
    @entries = 1
  end

  def entries
    @entries
  end

  def fill
    @entries = 1.5
  end
end
",
        );
        let special = (
            "app/models/special_registry.rb",
            "class SpecialRegistry < Registry\n  def self.seed\n    @entries = \"x\"\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[registry, special], 0),
            "  def self.entries -> Integer? | String
  def self.reset -> Integer
  def entries -> Float?
  def fill -> Float"
        );

        // A subclass with no singleton method has no singleton class in the graph, and its class
        // body still writes its own `@entries`, which `Registry.entries` reads when called on it.
        let plain = (
            "app/models/plain_registry.rb",
            "class PlainRegistry < Registry\n  @entries = [1]\nend\n",
        );
        assert!(
            drawn_in(&[registry, special, plain], 0)
                .starts_with("  def self.entries -> Array? | Integer | String"),
            "{}",
            drawn_in(&[registry, special, plain], 0)
        );
    }

    #[test]
    fn a_module_s_method_read_for_one_class_hears_only_that_class_s_writes() {
        // `Labelled#label` is shared by every includer, and one of them stores an `Integer`. Read
        // for a `Plain`, the body runs on a `Plain`, which never runs `Counted#count`.
        let labelled = (
            "app/models/labelled.rb",
            "module Labelled\n  def label\n    @label ||= \"x\"\n  end\nend\n",
        );
        let plain = (
            "app/models/plain.rb",
            "class Plain\n  include Labelled\nend\n",
        );
        let report = (
            "app/report.rb",
            "\
class Report
  def plain
    Plain.new.label
  end

  def counted
    Counted.new.label
  end

  def peek
    Counted.new.peek
  end

  def shared
    Counted.shared
  end
end
",
        );
        let counted = (
            "app/models/counted.rb",
            "\
class Counted
  include Labelled

  def count
    @label = 1
  end

  def peek
    @label
  end

  def self.shared
    @shared ||= 1.5
  end
end
",
        );
        // `peek` is `Counted`'s own, so the receiver narrows nothing; `shared` is the class
        // object's, which a receiver never narrows.
        assert_eq!(
            drawn_in(&[labelled, plain, counted, report], 3),
            "  def plain -> String\n  def counted -> String | Integer\n  def peek -> Integer? | String\n  def shared -> Float"
        );
        // Asked from the module itself, every includer's writes reach.
        assert_eq!(
            drawn_in(&[labelled, plain, counted, report], 0),
            "  def label -> String | Integer"
        );
    }

    #[test]
    fn a_superclass_spelled_like_its_class_is_read_through_the_class_ruby_names() {
        // Ruby reads `< ApplicationController` before `Admin::ApplicationController` exists, so
        // the superclass is the top-level class, and its `@x = 1` reaches `show`. rubydex resolves
        // the name to the class being opened and cuts the chain at a cycle;
        // `Indexed::repair_superclasses` links it to the class Ruby names.
        let base = (
            "app/controllers/application_controller.rb",
            "class ApplicationController\n  def load\n    @x = 1\n  end\nend\n",
        );
        let admin = (
            "app/controllers/admin/application_controller.rb",
            "\
module Admin
  class ApplicationController < ApplicationController
    def fill
      @x = \"s\"
    end

    def show
      @x
    end
  end
end
",
        );
        assert_eq!(
            drawn_in(&[base, admin], 1),
            "    def fill -> String\n    def show -> String? | Integer"
        );

        // The other direction: the top-level class gains `Admin::ApplicationController` as a
        // descendant, so a read there sees its `@x = "s"`.
        let base = (
            "app/controllers/application_controller.rb",
            "class ApplicationController\n  def load\n    @x = 1\n  end\n\n  def show\n    @x\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[base, admin], 0),
            "  def load -> Integer\n  def show -> String? | Integer"
        );

        // A module that includes itself is a cycle with no superclass to blame: its own reads
        // refuse, and nothing else is.
        let looping = (
            "app/models/looping.rb",
            "module Looping\n  include Looping\n\n  def set\n    @x = 1\n  end\n\n  def x\n    @x\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[looping], 0), "  def set -> Integer");

        // A class below the repaired one reads through it, and a top-level class of the same last
        // name is untouched.
        let admin_users = (
            "app/controllers/admin/users_controller.rb",
            "module Admin\n  class UsersController < Admin::ApplicationController\n    def z\n      @x\n    end\n  end\nend\n",
        );
        let users = (
            "app/controllers/users_controller.rb",
            "class UsersController\n  def load\n    @y = \"s\"\n  end\n\n  def y\n    @y\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[base, admin, admin_users, users], 2),
            "    def z -> String? | Integer"
        );
        assert_eq!(
            drawn_in(&[base, admin, admin_users, users], 3),
            "  def load -> String\n  def y -> String?"
        );

        // A superclass Ruby could not find is left as rubydex cut it, and its reads still refuse.
        let orphan = (
            "app/models/admin/thing.rb",
            "module Admin\n  class Thing < Thing\n    def set\n      @x = 1\n    end\n\n    def x\n      @x\n    end\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[orphan], 0), "    def set -> Integer");
    }

    #[test]
    fn a_top_level_variable_is_its_own_text_s_writes() {
        // `main`'s instance variables belong to no class, so no hierarchy is asked: this text's
        // writes. One nothing writes is refused, not `NilClass`. `nil` still joins: no method
        // wrote it first, and the rule for a local's straight line is not applied here.
        let script = (
            "script/tally.rb",
            "@count = 1\ntotal = @count\nother = @missing\n",
        );
        assert_eq!(drawn_in(&[script], 0), "total: Integer? = @count");
    }

    #[test]
    fn an_initialize_written_twice_says_nothing_about_which_one_runs() {
        let first = (
            "app/models/twice.rb",
            "class Twice\n  def initialize\n    @seed = \"x\"\n  end\n\n  def seed\n    @seed\n  end\nend\n",
        );
        let second = (
            "app/models/twice_again.rb",
            "class Twice\n  def initialize\n    @other = 1\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[first, second], 0), "  def seed -> String?");
    }

    #[test]
    fn a_variable_written_by_reflection_is_written_by_something_nothing_types() {
        // #19. `instance_variable_set(:@x, v)` on `self` is `@x = v` with nothing to type `v` by.
        let exact = (
            "app/models/exact.rb",
            "\
class Exact
  def initialize
    @x = \"x\"
  end

  def load(v)
    instance_variable_set(:@x, v)
  end

  def x
    @x
  end
end
",
        );
        assert_eq!(drawn_in(&[exact], 0), "null");

        // An interpolated name reaches every variable it can spell, and no other.
        let cached = (
            "app/models/cached.rb",
            "\
class Cached
  def fill(key, v)
    instance_variable_set(\"@#{key}_cache\", v)
  end

  def warm
    @warm_cache = 1
  end

  def warm_cache
    @warm_cache
  end

  def count
    @count = 1
  end

  def tally
    @count
  end
end
",
        );
        assert_eq!(
            drawn_in(&[cached], 0),
            "  def warm -> Integer\n  def count -> Integer\n  def tally -> Integer?"
        );

        // A removal writes `nil`, whatever `initialize` did first.
        let removed = (
            "app/models/removed.rb",
            "\
class Removed
  def initialize
    @seed = \"x\"
  end

  def reset
    remove_instance_variable(:@seed)
  end

  def seed
    @seed
  end
end
",
        );
        assert_eq!(drawn_in(&[removed], 0), "  def seed -> String?");
    }

    #[test]
    fn a_variable_another_object_writes_by_reflection_is_refused_wherever_it_is_read() {
        // Nothing types `digest` in `call`, so the write may reach any object's `@topic`.
        let digest = (
            "app/mailers/digest.rb",
            "class Digest\n  def initialize\n    @topic = \"t\"\n  end\n\n  def topic\n    @topic\n  end\nend\n",
        );
        let unsubscriber = (
            "lib/unsubscriber.rb",
            "class Unsubscriber\n  def call(digest)\n    digest.instance_variable_set(:@topic, 42)\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[digest, unsubscriber], 0), "null");

        // The suite pokes the application's objects too, but the application never runs it.
        let poke = (
            "spec/support/poke.rb",
            "class Poke\n  def call(digest)\n    digest.instance_variable_set(:@topic, 42)\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[digest, poke], 0), "  def topic -> String");
        // A read the suite makes is of an object the suite may have poked.
        let fake = (
            "spec/support/fake_digest.rb",
            "class FakeDigest\n  def initialize\n    @topic = \"t\"\n  end\n\n  def topic\n    @topic\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[fake, poke], 0), "null");

        // The top level's `self` is `main` (or a template's view), and only the top level reads its
        // variables: a dynamic write there refuses the script's own reads and nobody else's.
        let script = (
            "script/tally.rb",
            "instance_variable_set(\"@#{ARGV[0]}\", 1)\n@count = 1\ntotal = @count\n",
        );
        assert_eq!(drawn_in(&[digest, script], 0), "  def topic -> String");
        assert_eq!(drawn_in(&[digest, script], 1), "null");
    }

    #[test]
    fn a_reflective_name_held_by_a_local_or_passed_by_callers_reaches_only_those_names() {
        let digest = (
            "app/mailers/digest.rb",
            "\
class Digest
  def initialize
    @topic = \"t\"
    @topic_token = \"x\"
  end

  def topic
    @topic
  end

  def token
    @topic_token
  end
end
",
        );
        // A local assigned a pattern names what the pattern can spell.
        let uploader = (
            "lib/uploader.rb",
            "\
class Uploader
  def token(model)
    var = :\"@#{model.name}_token\"
    model.instance_variable_set(var, 1)
  end
end
",
        );
        assert_eq!(drawn_in(&[digest, uploader], 0), "  def topic -> String");

        // A parameter names what its callers pass.
        let preload = (
            "lib/preload.rb",
            "\
class Preload
  def self.fill(record, ivar)
    record.instance_variable_set(ivar, [])
  end

  def self.warm(record)
    fill(record, :@topic_token)
  end
end
",
        );
        assert_eq!(drawn_in(&[digest, preload], 0), "  def topic -> String");

        // With no caller to read, it names every variable.
        let orphan = (
            "lib/orphan.rb",
            "class Orphan\n  def self.fill(record, ivar)\n    record.instance_variable_set(ivar, [])\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[digest, orphan], 0), "null");

        // A caller whose argument cannot be read (a splat) says any name, and so do more callers
        // than are worth reading.
        let splat = (
            "lib/splat.rb",
            "\
class Splat
  def self.fill(record, ivar)
    record.instance_variable_set(ivar, [])
  end

  def self.warm(record, args)
    fill(record, :@topic_token)
    fill(*args)
  end
end
",
        );
        assert_eq!(drawn_in(&[digest, splat], 0), "null");
        let mut busy = String::from(
            "class Busy\n  def self.fill(record, ivar)\n    record.instance_variable_set(ivar, [])\n  end\n\n  def self.warm(record)\n",
        );
        for _ in 0..=crate::analysis::types::CALL_SITES {
            busy.push_str("    fill(record, :@topic_token)\n");
        }
        busy.push_str("  end\nend\n");
        assert_eq!(
            drawn_in(&[digest, ("lib/busy.rb", busy.as_str())], 0),
            "null"
        );

        // The callers' answer is held between requests while their documents are unchanged, and
        // answers the same.
        let (mut harness, uris) = with_files(&[digest, preload]);
        let once = drawn_hints(digest.1, &harness.hints_in(&uris[0]));
        let again = drawn_hints(digest.1, &harness.hints_in(&uris[0]));
        assert_eq!(
            (once.as_str(), again.as_str()),
            ("  def topic -> String", "  def topic -> String")
        );

        // A caller that changes what it passes is read again: the held answer was for the version
        // it came from. Now `@topic` is the one refused.
        let passes_topic = preload.1.replace(":@topic_token", ":@topic");
        harness.open(&uris[1], preload.1);
        harness.change(&uris[1], &passes_topic);
        assert_eq!(
            drawn_hints(digest.1, &harness.hints_in(&uris[0])),
            "  def token -> String"
        );
    }

    #[test]
    fn a_library_s_render_is_not_a_renderer_of_the_application_s_templates() {
        let (dir, _gem_home, env) = project_with_gem(
            "module Shouty\n  def self.go\n    @story = 1\n    render template: \"stories/show\"\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("sig/core.rbs", TYPED_RBS);
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = \"x\"\n  end\nend\n",
        );
        let template = "<% held = @story %>\n";
        let view = harness.write("app/views/stories/show.html.erb", template);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(template, &harness.hints_in(&view)),
            "<% held: String? = @story %>"
        );
    }

    #[test]
    fn a_library_s_name_building_writes_on_its_includers_are_not_read() {
        // A file that writes a variable by a name it builds is walked for every read of
        // its objects, since it never spells the one read; a library's is not, for the reason
        // above. actionpack's test helper removes every variable a controller has.
        let (dir, _gem_home, env) = project_with_gem(
            "module Shouty\n  def clear\n    instance_variables.each { |ivar| \
             remove_instance_variable(ivar) }\n  end\n\n  def put(name, value)\n    \
             instance_variable_set(\"@#{name}\", value)\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("sig/core.rbs", TYPED_RBS);
        let source = "class Digest\n  include Shouty\n\n  def initialize\n    @topic = \"t\"\n  \
                      end\n\n  def topic\n    @topic\n  end\nend\n";
        let uri = harness.write("app/mailers/digest.rb", source);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def topic -> String"
        );
    }

    #[test]
    fn a_library_s_own_reflective_writes_are_not_read() {
        // A gem sets its own objects' state; reading its `obj.instance_variable_set` would refuse
        // every variable its helpers could spell.
        let (dir, _gem_home, env) = project_with_gem(
            "module Shouty\n  def self.poke(obj)\n    obj.instance_variable_set(:@topic, 1)\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("sig/core.rbs", TYPED_RBS);
        let source = "class Digest\n  def initialize\n    @topic = \"t\"\n  end\n\n  def topic\n    @topic\n  end\nend\n";
        let uri = harness.write("app/mailers/digest.rb", source);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def topic -> String"
        );
    }

    #[test]
    fn an_initialize_only_a_signature_declares_says_nothing_about_what_it_writes() {
        // The first `initialize` up `Child`'s chain is `Base`'s, and only its signature is written:
        // no body says it sets `@seed`.
        let base_sig = (
            "sig/base.rbs",
            "class Base\n  def initialize: () -> void\nend\n",
        );
        let base = ("app/models/base.rb", "class Base\nend\n");
        let child = (
            "app/models/child.rb",
            "class Child < Base\n  def load\n    @seed = \"x\"\n  end\n\n  def seed\n    @seed\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[base_sig, base, child], 2),
            "  def load -> String\n  def seed -> String?"
        );
    }

    #[test]
    fn a_class_a_library_can_build_keeps_nil_whatever_its_initialize_writes() {
        // #20. A library whose Ruby the class inherits from may build it with `allocate`, as Active
        // Record builds a loaded record, and `initialize` has not run then.
        let (dir, _gem_home, env) = project_with_gem(
            "module Shouty\n  class Record\n    def self.load\n      allocate\n    end\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("sig/core.rbs", TYPED_RBS);
        let source = "\
class Story < Shouty::Record
  def initialize
    @views = \"x\"
  end

  def views
    @views
  end
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def views -> String?"
        );
    }

    #[test]
    fn a_module_s_own_variable_is_the_module_s_whoever_includes_it() {
        // `Config.value` runs on the module object alone: an includer's singleton methods are its
        // own, and its `@value` is a different object's variable.
        let config = (
            "app/models/config.rb",
            "\
module Config
  def self.value
    @value
  end

  def self.set
    @value = 1
  end
end
",
        );
        let user = (
            "app/models/user.rb",
            "class User\n  include Config\n\n  def self.set\n    @value = \"s\"\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[config, user], 0),
            "  def self.value -> Integer?\n  def self.set -> Integer"
        );
    }

    #[test]
    fn a_file_that_opens_two_classes_of_the_object_is_read_once() {
        // Both classes are the object's, and the file holds both: each write is counted once.
        let both = (
            "app/models/both.rb",
            "\
class Base
  def load
    @x = 1
  end
end

class Leaf < Base
  def x
    @x
  end
end
",
        );
        assert_eq!(
            drawn_in(&[both], 0),
            "  def load -> Integer\n  def x -> Integer?"
        );
    }

    #[test]
    fn a_block_straight_in_a_class_body_may_run_on_either_side() {
        // `before_action { }` runs on an instance and `included do` on the class, and nothing in
        // the block says which. Its write reaches both, and its read claims neither.
        let source = "\
class StoriesController
  before_action { @story = \"x\" }

  def show
    @story
  end

  def index
    @story = 1
  end
end
";
        let files = [("app/controllers/stories_controller.rb", source)];
        assert_eq!(
            drawn_in(&files, 0),
            "  def show -> String? | Integer\n  def index -> Integer"
        );
    }

    #[test]
    fn an_instance_variable_is_every_write_in_its_class_and_nil_until_one_has_run() {
        // Methods run in any order, so every write of `@x` reaches every read (#12), and `nil`
        // does too, except where no instance exists without one (`initialize` writes it as a
        // statement of its own body) or the reading method has already written it.
        let source = "\
class Keeper
  def initialize
    @set = \"x\"
  end

  def load
    @later = \"y\"
  end

  def set
    @set
  end

  def later
    @later
  end

  def here
    @here = \"z\"
    @here
  end

  def a
    @v = 1
  end

  def b
    @v = \"s\"
  end

  def c
    @v
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def load -> String
  def set -> String
  def later -> String?
  def here -> String
  def a -> Integer
  def b -> String
  def c -> Integer? | String"
        );
    }

    #[test]
    fn a_template_s_instance_variable_is_every_write_its_controller_makes() {
        // Any action may have run before the template renders, so `index`'s write reaches `show`'s
        // template as well as `show`'s (#12), and `nil` does too: no controller is built with it
        // set. The old rung answered with the controller's last write.
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = \"x\"\n  end\n\n  def index\n    \
             @story = 1\n  end\nend\n",
        );
        let template = "<% held = @story %>\n";
        let view = harness.write("app/views/stories/show.html.erb", template);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(template, &harness.hints_in(&view)),
            "<% held: String? | Integer = @story %>"
        );
    }

    #[test]
    fn a_template_s_variable_is_every_write_of_every_class_that_renders_it() {
        // #21. `FeedsController#show` renders `stories/show` too, so its `@story` reaches it.
        let stories = (
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = \"x\"\n  end\nend\n",
        );
        let feeds = (
            "app/controllers/feeds_controller.rb",
            "class FeedsController\n  def show\n    @story = 1\n    render \"stories/show\"\n  end\nend\n",
        );
        let template = ("app/views/stories/show.html.erb", "<% held = @story %>\n");
        assert_eq!(
            drawn_in(&[stories, feeds, template], 2),
            "<% held: String? | Integer = @story %>"
        );

        // A partial of the same name is not this template, and the suite's render is not the
        // application's.
        let rows = (
            "app/controllers/rows_controller.rb",
            "class RowsController\n  def show\n    @story = [1]\n    render partial: \"stories/show\"\n    render \"pages/show\"\n  end\nend\n",
        );
        let suite = (
            "spec/support/rerender.rb",
            "class Rerender\n  def go\n    @story = 1.5\n    render \"stories/show\"\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[stories, feeds, rows, suite, template], 4),
            "<% held: String? | Integer = @story %>"
        );

        // A name nothing can read may be this template's.
        let dynamic = (
            "app/controllers/dynamic_controller.rb",
            "class DynamicController\n  def show(options)\n    @story = 1.5\n    render options\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[stories, dynamic, template], 2),
            "<% held: String? | Float = @story %>"
        );

        // A concern's render is made by whichever controller includes it.
        let concern = (
            "app/controllers/concerns/reshows.rb",
            "module Reshows\n  def reshow\n    render \"stories/show\"\n  end\nend\n",
        );
        let pages = (
            "app/controllers/pages_controller.rb",
            "class PagesController\n  include Reshows\n\n  def show\n    @story = [1]\n  end\nend\n",
        );
        assert!(
            drawn_in(&[stories, concern, pages, template], 3).contains("Array"),
            "{}",
            drawn_in(&[stories, concern, pages, template], 3)
        );

        // A render on another object hands the template variables nobody writes, and so does a
        // helper, which runs in whatever view called it.
        let job = (
            "app/jobs/digest_job.rb",
            "class DigestJob\n  def perform\n    ApplicationController.render(template: \"stories/show\", assigns: {})\n  end\nend\n",
        );
        assert_eq!(drawn_in(&[stories, job, template], 2), "null");
        let task = (
            "lib/tasks/digest.rake",
            "task :digest do\n  ApplicationController.render(template: \"stories/show\", assigns: {})\nend\n",
        );
        assert_eq!(drawn_in(&[stories, task, template], 2), "null");

        // A helper renders into whichever view called it, so every class a view is rendered by
        // is a renderer: here only `StoriesController`.
        let helper = (
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\n  def again\n    render template: \"stories/show\"\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[stories, helper, template], 2),
            "<% held: String? = @story %>"
        );
    }

    #[test]
    fn a_partial_s_variable_is_every_write_of_every_class_a_view_is_rendered_by() {
        // A partial is rendered from views, so its path names no class: `FeedsController`'s view
        // may render `stories/_story` as well as `StoriesController`'s.
        let stories = (
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = \"x\"\n  end\nend\n",
        );
        let feeds = (
            "app/controllers/feeds_controller.rb",
            "class FeedsController\n  def index\n    @story = 1\n  end\nend\n",
        );
        let feed = ("app/views/feeds/index.html.erb", "<%= render @stories %>\n");
        let partial = (
            "app/views/stories/_story.html.erb",
            "<% held = @story %>\n<% again = @story %>\n",
        );
        // `StoriesController` renders no view here, and a partial's own path renders nothing.
        assert_eq!(
            drawn_in(&[stories, feeds, feed, partial], 3),
            "<% held: Integer? = @story %>\n<% again: Integer? = @story %>"
        );
        // With no view rendered at all, no object is there to read.
        assert_eq!(drawn_in(&[stories, partial], 1), "null");
        let show = ("app/views/stories/show.html.erb", "<%= render @story %>\n");
        assert_eq!(
            drawn_in(&[stories, feeds, feed, show, partial], 4),
            "<% held: String? | Integer = @story %>\n<% again: String? | Integer = @story %>"
        );
        // A class with no view of its own renders none, and its writes do not reach.
        let api = (
            "app/controllers/api_controller.rb",
            "class ApiController\n  def show\n    @story = 1.5\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[stories, feeds, feed, show, api, partial], 5),
            "<% held: String? | Integer = @story %>\n<% again: String? | Integer = @story %>"
        );
        // A component's own `render` renders the component, with its own variables.
        let component = (
            "app/components/card_component.rb",
            "class CardComponent\n  def call\n    @story = [1]\n    render(Other.new)\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[stories, feeds, feed, show, component, partial], 5),
            "<% held: String? | Integer = @story %>\n<% again: String? | Integer = @story %>"
        );
        // With no class rendering a view, there is no object to read.
        let lone = ("app/views/lone/_row.html.erb", "<% held = @story %>\n");
        assert_eq!(drawn_in(&[lone], 0), "null");
    }

    #[test]
    fn a_keyword_hash_is_one_more_positional_to_an_arm_that_takes_no_keywords() {
        // Ruby hands `build(size: 1)` to `build(options)` as one `Hash`. An arm that declares
        // that position as an `Integer` cannot receive it, and one that takes keywords reads them
        // as keywords.
        let rbs = "\
class Widget
  def self.build: (Hash[Symbol, untyped] options) -> Widget
  def self.make: (Integer count) -> Widget
  def self.shape: (?size: Integer) -> Widget
end
";
        let source = "\
class Widget
end

built = Widget.build(size: 1)
made = Widget.make(size: 1)
shaped = Widget.shape(size: 1)
";
        let (mut harness, uri) = with_declared_types(source, rbs);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "built: Widget = Widget.build(size: 1)\nshaped: Widget = Widget.shape(size: 1)"
        );
    }

    #[test]
    fn a_class_declared_inside_a_singleton_class_is_not_drawn_under_a_name_ruby_cannot_spell() {
        // `Params` is declared in `class << self`, so rubydex names it
        // `Orchestrator::<Orchestrator>::Params`, and `Orchestrator::Params` is not it in Ruby
        // either. A label must be a name a reader can write.
        let source = "\
class Orchestrator
  class << self
    class Params
    end

    def build
      made = Params.new
      made
    end
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        assert_eq!(drawn_hints(source, &harness.hints_in(&uri)), "null");
    }

    #[test]
    fn a_class_object_is_drawn_as_the_class_with_colon_class() {
        // `Widget` the constant is the class itself, not one of its instances, and `Widget:class`
        // says so. A module object has no such spelling and is not drawn, and neither is a class
        // declared inside `class << self` (`Box`), whose own name Ruby cannot write. `x = Widget`
        // states its type on the line, so the local is not drawn. Each carries a class method,
        // because rubydex gives a class object a type only once it has a singleton class. `own`'s
        // `self.class` is a class nothing named, whose class object is not drawn; an instance of it
        // (`make`) keeps the `Class.new` spelling it already had.
        let source = "\
class Widget
  def self.build
    new
  end
end

module Named
  def self.helper
    1
  end
end

class Holder
  class << self
    class Box
      def self.make
        new
      end
    end
  end

  def model
    Widget
  end

  def mixin
    Named
  end

  def box
    Box
  end

  def read
    klass = model
    klass
  end

  def plain
    klass = Widget
    klass
  end

  def built
    Class.new do
      def self.make
        new
      end

      def own
        self.class
      end
    end
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def self.build -> Widget
  def self.helper -> Integer
  def model -> Widget:class
  def read -> Widget:class
    klass: Widget:class = model
  def plain -> Widget:class
      def self.make -> Class.new"
        );
    }

    #[test]
    fn a_method_only_the_suite_mixes_in_is_not_the_application_s() {
        // The suite `extend`s a helper into `Bus`, so rubydex's linearization of
        // `Bus`'s singleton holds the helper's `publish` first. The application never loads it:
        // its `Bus.publish` is the class's own. The suite's own call is the helper's.
        // `publish` comes from a module `Bus` extends, as `MessageBus`'s does; a later `extend`
        // goes ahead of it.
        let bus = (
            "lib/bus.rb",
            "module BusImpl\n  def publish(channel)\n    1\n  end\nend\n\nmodule Bus\n  extend BusImpl\nend\n",
        );
        let helper = (
            "spec/support/bus_helper.rb",
            "module BusHelper\n  def publish(channel)\n    \"x\"\n  end\nend\n\nmodule Bus\n  extend BusHelper\nend\n",
        );
        let app = (
            "app/jobs/notify.rb",
            "class Notify\n  def call\n    Bus.publish(\"c\")\n  end\nend\n",
        );
        let spec = (
            "spec/jobs/notify_spec.rb",
            "class NotifySpec\n  def go\n    Bus.publish(\"c\")\n  end\nend\n",
        );
        assert_eq!(
            drawn_in(&[bus, helper, app, spec], 2),
            "  def call -> Integer"
        );
        assert_eq!(drawn_in(&[bus, helper, app, spec], 3), "  def go -> String");
    }

    #[test]
    fn a_receiverless_call_in_a_helper_or_a_template_is_the_view_s() {
        // `self` in a helper method and in a template is the view, which includes every
        // helper: `shout` is `ApplicationHelper`'s, which `StoriesHelper` does not include. A card
        // answered it through the view context; the margin now does too.
        let application = (
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def shout\n    \"x\"\n  end\nend\n",
        );
        let stories = (
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\n  def headline\n    shout\n  end\nend\n",
        );
        let controller = (
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n  end\nend\n",
        );
        let template = ("app/views/stories/show.html.erb", "<% loud = shout %>\n");
        let files = [application, stories, controller, template];
        assert_eq!(drawn_in(&files, 1), "  def headline -> String");
        assert_eq!(drawn_in(&files, 3), "<% loud: String = shout %>");
    }

    #[test]
    fn a_guess_never_becomes_part_of_a_type_that_is_drawn() {
        // A guess is kept out of the margin by its tier, and three rungs used to drop the tier on
        // the way through: a block's value filling a generic (`map` gives `Array[U]`), an argument
        // picking an overload (`1 + x`), and a left operand deciding which side of `||` runs. Each
        // answer below would have carried the call's tier around a class read off a name.
        //
        // - `wrapped` is still an `Array`: only the element the guess named is dropped.
        // - `summed` picks no arm, because the only thing telling `Integer#+`'s arms apart is a
        //   guess. Every arm answers instead (`types::join_arms`), so the label is what the
        //   signature allows, and the guess decides nothing.
        // - `either` is refused with the guess, not answered with `"x"`'s `String`.
        let source = "\
class Ledger
end

class Probe
  def self.prep(x)
    ledger = x.frobnicate
    ledger
  end

  def self.number(x)
    float = x.frobnicate
    float
  end

  def self.flagged(x)
    nil_class = x.frobnicate
    nil_class
  end

  def wrapped
    [1].map { |v| Probe.prep(v) }
  end

  def summed
    1 + Probe.number(2)
  end

  def either
    Probe.flagged(1) || \"x\"
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Integer\n  def +: (Integer) -> Integer\n       | (Float) -> Float\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def wrapped -> Array
    [1].map { |v: Integer| Probe.prep(v) }
  def summed -> Integer | Float"
        );
    }

    #[test]
    fn a_bang_over_a_guess_is_bool_because_ruby_says_so_not_the_guess() {
        // `!` answers `true` or `false` whatever its operand is, so `!foo` is drawn `bool` when
        // nothing types `foo`. A guess must not do worse than nothing: `admin` is typed `Admin`
        // from its spelling alone, narrowing `!admin` to a guessed `false` would keep it out of
        // the margin, and knowing more would lose the label. An operand a signature types still
        // narrows.
        let source = "\
class Admin
end

class Ledger
  def plain(foo)
    !foo
  end

  def named(admin)
    !admin
  end

  def twice(admin)
    !!admin
  end

  def instance
    !@admin
  end

  def declared
    !\"x\".upcase
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class TrueClass\n  def !: () -> false\nend\n\n\
             class FalseClass\n  def !: () -> true\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def plain(foo) -> bool
  def named(admin) -> bool
  def twice(admin) -> bool
  def instance -> bool
  def declared -> false"
        );
    }

    /// Signatures for a value that may be `nil`, with the names `NilClass` answers differently.
    ///
    /// `Kernel` and `NilClass` disagree the way Ruby's core and ActiveSupport do: `nil?` and
    /// `present?` are `false`/`true` on a record and the opposite on `nil`. `to_h` is `-> {}` on
    /// `NilClass`, a record type the table drops, as in `vendor/rbs`; `to_a` is `-> []`, a tuple,
    /// which is an `Array`.
    const NILABLE_RBS: &str = "\
module Kernel
  def nil?: () -> false
  def dup: () -> self
  def then: [U] () { (self) -> U } -> U
end

class NilClass
  def nil?: () -> true
  def present?: () -> false
  def blank?: () -> true
  def to_i: () -> 0
  def to_a: () -> []
  def to_h: () -> {}
  def to_s: () -> \"\"
end

class Symbol
end

class Record
  def id: () -> Integer
  def label: () -> String
  def present?: () -> true
  def blank?: () -> false
  def to_i: () -> Integer
  def to_a: () -> Array[String]
  def to_h: () -> Hash[String, Integer]
  def to_s: () -> Symbol
end

class Shelf
  def maybe: () -> Record?
  def named: () -> String?
  def flag: () -> bool?
end
";

    #[test]
    fn a_call_on_a_value_that_may_be_nil_asks_nil_the_same_question() {
        // A `Record?` is a record or `nil`, and the call runs on whichever it is. Asking only the
        // record answers `x.nil?` with `Kernel#nil?`'s `false`, drawn as fact about a value that
        // is `nil` half the time. `NilClass` is asked too, and the two answers fold: `false`
        // beside `true` is `bool`, and `self` beside `nil` is `Record?`.
        //
        // Where `NilClass` has no answer, the record's stands, as it did before: Ruby raises on
        // `nil` there, so there is no second value. Three ways to have none: no member
        // (`label`), a private one (`id`, which the script below defines at the top level), and
        // one the table cannot read (`to_h`). `to_a` is `[]` on `nil`, an `Array` with no element to
        // agree with the record's, so the two halves are an `Array`.
        let source = "\
class Ledger
  def checked
    Shelf.new.maybe.nil?
  end

  def negated
    !Shelf.new.maybe.nil?
  end

  def present
    Shelf.new.maybe.present?
  end

  def blank
    Shelf.new.maybe.blank?
  end

  def copy
    Shelf.new.maybe.dup
  end

  def counted
    Shelf.new.maybe.to_i
  end

  def spelled
    Shelf.new.maybe.to_s
  end

  def piped
    Shelf.new.maybe.then { 1 }
  end

  def named
    Shelf.new.maybe.label
  end

  def private_on_nil
    Shelf.new.maybe.id
  end

  def unreadable_on_nil
    Shelf.new.maybe.to_h
  end

  def emptied_on_nil
    Shelf.new.maybe.to_a
  end

  def maybe_body(flag)
    Record.new if flag
  end

  def body_checked
    maybe_body(1).nil?
  end

  def both_halves
    Shelf.new.flag.nil?
  end

  def shortcut
    Shelf.new.maybe.nil? && @mystery
  end
end
";
        let (mut harness, uri) = with_declared_types(source, NILABLE_RBS);
        harness.write("script/tidy.rb", "def id\n  \"x\"\nend\n");
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // Two halves naming two classes are a union, drawn and terminal like any other.
            // `shortcut` is refused: a `bool` left side reaches `@mystery`, which nothing types.
            // Before, a wrong `false` skipped it.
            "  def checked -> bool
  def negated -> bool
  def present -> bool
  def blank -> bool
  def copy -> Record?
  def counted -> Integer
  def spelled -> Symbol | String
  def piped -> Integer
  def named -> String
  def private_on_nil -> Integer
  def unreadable_on_nil -> Hash[String, Integer]
  def emptied_on_nil -> Array
  def maybe_body(flag) -> Record?
  def body_checked -> bool
  def both_halves -> bool"
        );
    }

    #[test]
    fn a_safe_call_adds_nil_where_the_receiver_can_be_nil_and_skips_one_call() {
        // `a&.m` is `nil` where `a` is, so on a `Record?` it answers `M?`, and `NilClass` is never
        // asked: `x&.nil?` is `false` or `nil`, never `true`. Ruby skips that **one** call, so the
        // next link is an ordinary call on an `M?` and asks `NilClass` like any other. A receiver
        // with no mark is taken at its word.
        let source = "\
class Ledger
  def id_or_nil
    Shelf.new.maybe&.id
  end

  def checked
    Shelf.new.maybe&.nil?
  end

  def next_link
    Shelf.new.maybe&.label.nil?
  end

  def unmarked
    Record.new&.id
  end

  def twice
    !!Shelf.new.maybe&.present?
  end

  def spread
    word, size = Shelf.new.named&.pair
  end
end
";
        let (mut harness, uri) = with_declared_types(source, NILABLE_RBS);

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def id_or_nil -> Integer?
  def checked -> false?
  def next_link -> bool
  def unmarked -> Integer
  def twice -> bool
    word: String?, size = Shelf.new.named&.pair
    word, size: Integer? = Shelf.new.named&.pair"
        );
    }

    #[test]
    fn without_a_nil_class_a_value_that_may_be_nil_answers_as_it_always_did() {
        // `NilClass` comes from Ruby's core signatures. Where nothing declares it, there is no
        // second half to ask, and the answer is the one the receiver's own class gives.
        let mut harness = signed(
            &[(
                "core/core.rbs",
                "module Kernel\n  def nil?: () -> false\nend\n\nclass Object\n  include Kernel\nend\n\n\
                 class FalseClass\nend\n\n\
                 class Record\nend\n\nclass Shelf\n  def maybe: () -> Record?\nend\n",
            )],
            "",
        );
        let source = "class Ledger\n  def checked\n    Shelf.new.maybe.nil?\n  end\nend\n";
        let uri = harness.write("app/report.rb", source);
        harness.index();
        harness.index_gems();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def checked -> false"
        );
    }

    #[test]
    fn a_destructured_target_is_the_position_it_is_and_never_the_whole_value() {
        // **The margin asks the same question as `locator`, or it answers a different one.**
        // `blobs, actions = prepare` is three types, not one: each name holds a position of what
        // the call returned. Without the `Receiver::Destructured` wrap the rest of the crate
        // applies, `cursor::bindings_in` would ask "what does `prepare` return", and draw that one
        // `Array` on *every* name left of the `=`, as in `: Array` three times over
        // `blobs, actions, error = ...` in real code. Nothing declares a tuple for `prepare`, so
        // the honest answer is no label at all, which is what a card at a use of `blobs` says too.
        //
        // `pair` is the other half of the rule, and why the wrap is not just a refusal: a tuple
        // that *is* declared answers each position exactly, and the margin is where someone
        // counting names left of an `=` wants to read it.
        let source = "\
class Ledger
  def prepare
    [\"x\"]
  end

  def use
    blobs, actions = prepare
    word, size = \"hi\".pair
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // **`Array[String]`, not `Array`**, and the difference is where the label came from:
            // this is the *body* rung, which read `[\"x\"]` and knows what the literal held. A
            // return drawn from a **signature** stays the bare head: `Typed::declared_as` has no
            // receiver and no value, so a position has nothing to describe.
            "  def prepare -> Array[String]
    word: String, size = \"hi\".pair
    word, size: Integer = \"hi\".pair"
        );
    }

    #[test]
    fn two_classes_left_after_the_folds_are_drawn_as_the_union_they_are() {
        // The residue, and the one thing it may do. `split` returns a `String` on one path and an
        // `Integer` on the other: neither fold applies, and there is no single class. The margin
        // says what the method does, and that is all a union gets: there is no single class for a
        // `.` to answer from or a chain to step to. Both stop at `types::Typed::one`, which answers
        // `None` for a union.
        //
        // `with_nil` is the same residue with a `nil`, and the mark goes on the head, not around
        // the whole: `String? | Integer` and `(String | Integer)?` are one type, and the first
        // needs no brackets in a margin with no room for them.
        let source = "\
class Ledger
  def split
    if stamped?
      \"x\"
    else
      1
    end
  end

  def with_nil
    return if stamped?
    split
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def split -> String | Integer
  def with_nil -> String? | Integer"
        );
    }

    #[test]
    fn the_mark_is_drawn_on_a_local_as_well_as_on_a_return() {
        // The facet reaches every family that draws a type, not only return labels: telling a
        // reader that `held` is a `String` without saying it can be `nil` is the same half-truth in
        // another place. `plain` is the control (same rung, same shape, unmarked answer), and both
        // take their type from a method one `def` away, the hop the mark has to travel.
        let source = "\
class Ledger
  def guarded
    return if stamped?
    \"x\"
  end

  def always
    \"y\"
  end

  def read
    held = guarded
    plain = always
    [held, plain]
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def guarded -> String?
  def always -> String
  def read -> Array
    held: String? = guarded
    plain: String = always"
        );
    }
}
