//! `textDocument/hover` — what the thing under the cursor is, as markdown.

use rubydex::model::{
    declaration::Declaration, definitions::Definition, graph::Graph, ids::DeclarationId,
    visibility::Visibility,
};

use super::{
    locator::{self, Resolution},
    render,
    synthesized::Synthesized,
    types, views,
};

/// How many guesses to list before giving up on being useful.
const MAX_CANDIDATES: usize = 10;

/// Markdown for a resolved cursor, or `None` when there is nothing worth saying.
///
/// # The shape of a card
///
/// Every card reads in the same order, and the order is the point: **the answer, then what ya-lsp
/// knows about the answer.**
/// 1. A fenced signature.
/// 2. A rule.
/// 3. RDoc's prose.
/// 4. The footnotes: one italic line each, at the bottom, never woven into the text above.
///
/// A reader who trusts the answer stops at the fence; one who does not finds the reason in the same
/// place every time.
///
/// A footnote is about ya-lsp's confidence, not the code: the receiver's type was unknown, or
/// derived rather than stated, or the class is reopened somewhere this card cannot show. That is
/// the test for whether a new line belongs there.
///
/// # The three tiers, and why they are said out loud
///
/// A reader who cannot tell the three apart loses what makes this server different from one that
/// guesses well:
/// - **resolved**: the code names the type. No footnote; nothing to doubt.
/// - **derived**: ya-lsp followed something, like a return type RBS declares or an assignment in
///   the same class. Correct if the signature is correct and that assignment is the one that ran.
///   The footnote names what was followed, so a reader can check it.
/// - **guessed**: matched on the method name alone. Always footnoted.
///
/// `assignment_line` is the one-based line of an instance variable's assignment, because a card
/// names a place a person can go to. It arrives converted: offsets become lines where the
/// document's text is, which is not here.
#[must_use]
pub fn markdown(
    synthesized: &Synthesized,
    modifiers: &locator::Modifiers<'_>,
    sources: &types::Sources<'_>,
    resolution: &Resolution,
    assignment_line: Option<u32>,
    cursor: Option<&str>,
) -> Option<String> {
    // The graph and the layout are read off `sources`, not passed again: they are two of its
    // fields, and this signature should make it impossible to build a card from one graph with
    // types from another.
    let graph: &Graph = sources.graph;
    match resolution.declarations.as_slice() {
        [] => None,
        [only] => {
            let mut card = card(synthesized, modifiers, sources, *only, cursor)?;
            if !resolution.precise {
                card.push_str(&footnote(&why_guessed(resolution.missed.as_ref())));
            }
            for note in provenance(&resolution.derivation, assignment_line) {
                card.push_str(&footnote(&note));
            }
            Some(card)
        }
        // Only the name-based fallback can produce more than one, and picking one arbitrarily would
        // present a coin flip as an answer.
        many => Some(candidate_list(graph, many, resolution.missed.as_ref())),
    }
}

const GUESS: &str = "Matched on the method name alone — the receiver's type is unknown.";

/// Why a guessed answer is a guess. Not the same sentence every time.
///
/// **"The receiver has no type" and "the receiver has no such member" are different facts.**
/// `completion` at the same cursor never reaches a member lookup, so it keeps offering that class's
/// members. Saying *the receiver's type is unknown* there is a contradiction the user sees one
/// keystroke later.
///
/// The tier and the answer are untouched. A list matched on a name is a guess either way, and
/// [`Tier::Guessed`](types::Tier) is what a reader acts on. This only says which case it is, so the
/// half with a class in it can be checked.
fn why_guessed(missed: Option<&locator::Missed>) -> String {
    let Some(missed) = missed else {
        return GUESS.to_owned();
    };
    let receiver = if missed.class_object {
        format!("the class object `{}`", missed.class)
    } else {
        format!("a `{}`", missed.class)
    };
    // **What the class did with the member: two different facts.**
    // - An empty lookup means the class has no such method.
    // - A lookup the privacy gate refused means the method exists, but Ruby will not let a written
    //   receiver call it.
    //
    // Printing the first sentence for the second is the defect this gate exists to remove. See
    // `locator::Missed::private`.
    //
    // **"keeps", not "declares", because the member is usually inherited.** `RSpec.describe` is
    // `Kernel`'s, reached through `Object`. `RSpec` does not declare it but does keep it private,
    // and that is the true fact a reader needs.
    let outcome = if missed.private {
        "which keeps it private"
    } else {
        "which has no such method"
    };
    match &missed.guessed_from {
        Some(name) => format!(
            "Matched on the method name alone — the receiver was guessed from the name `{name}` \
             to be {receiver}, {outcome}."
        ),
        None => {
            format!("Matched on the method name alone — the receiver is {receiver}, {outcome}.")
        }
    }
}

/// What ya-lsp followed to type the receiver, as the lines that go under the answer.
///
/// One line per *kind* of thing followed, not per step. A chain of three signatures is one fact,
/// and three italic lines would read as three doubts. Signatures are named the way rubydex names
/// them (`Kernel#tap()`, not `Array#tap()`), because that is the declaration the type came from and
/// the one a reader would open.
///
/// **A run of one signature is named once, however long.** `Kernel#clone()` returns its receiver,
/// so a chain of aliases through it (`@edit_user = @user.clone`) reads that declaration once per
/// hop. [`types::Derivation`] still records every hop; only the sentence collapses. **Consecutive,
/// never global**: `A#foo` → `B#bar` → `A#foo` is an alternation a reader can follow, not a repeat.
///
/// **The line does not say where a signature came from.** It may be `vendor/rbs`, a gem's `sig/`,
/// or text this crate generated; telling them apart costs a lookup. The useful half needs no check:
/// the type was read off a declaration, not off the expression under the cursor. Hovering that
/// declaration shows its source, through the comment its generator wrote.
///
/// **Shared with [`hints`](super::hints), not copied**, which is why it is not private. An inlay
/// hint's tooltip is this card's footnote in the room a hint has. The card and the margin are never
/// on screen together, so a difference between them would go unnoticed.
pub(super) fn provenance(
    derivation: &types::Derivation,
    assignment_line: Option<u32>,
) -> Vec<String> {
    let mut notes = Vec::new();
    if !derivation.signatures.is_empty() {
        // `Vec::dedup` is exactly the rule above: it drops a run and keeps an alternation.
        let mut walked = derivation.signatures.clone();
        walked.dedup();
        notes.push(format!(
            "Type derived through {} — from what those methods declare, not from this expression.",
            walked
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(" → ")
        ));
    }
    // Beside the signatures, not among them: the line above says "those methods", and a constant is
    // not one. Same evidence, same tier: the type is written in a signature, not read off the
    // expression.
    if let Some(constant) = &derivation.constant {
        notes.push(format!(
            "Type taken from the signature for `{constant}` — what it declares the constant \
             holds, not what this expression says."
        ));
    }
    // The same fact as the note above, written in Ruby instead of RBS, so said differently: one
    // names a signature a reader can read, the other a line that will have run. Naming the file
    // makes the second actionable: the constant is on screen, the initializer that builds it is
    // not.
    //
    // One shape, not two: `types::where_written` never answers empty, so the file is never left
    // out.
    if let Some(from) = &derivation.assigned_constant {
        notes.push(format!(
            "Type taken from where `{}` is assigned — `{}` line {} — and not from what this \
             expression says.",
            from.constant, from.file, from.line
        ));
    }
    // Same shape as the two above, one rung down: no signature said what the method returns, so the
    // Ruby in its body answered. Naming the file and line makes it checkable. The body is, by
    // construction, not the code on screen, and a reader who thinks another branch runs can go and
    // look.
    if let Some(from) = &derivation.body {
        notes.push(format!(
            "Type read out of `{}`'s body — `{}` line {} — because nothing declares what it \
             returns.",
            from.method, from.file, from.line
        ));
    }
    if let Some(line) = assignment_line {
        notes.push(format!(
            "Type taken from the assignment on line {line}, which may not be the one that ran."
        ));
    }
    // A convention, not a fact about this file, so the line it names is in another file. Naming
    // both is what makes the convention shippable: a reader who thinks Rails renders this template
    // from elsewhere can go and look.
    if let Some(from) = &derivation.renderer {
        notes.push(format!(
            "Type taken from `{}`, line {} — the {} Rails renders this template from.",
            from.renderer,
            from.line,
            // A mailer is not a controller, and this footnote reads as a claim about the one class
            // it names. `views::RenderedBy` carries which convention answered, for exactly this
            // sentence.
            if from.controller {
                "controller"
            } else {
                "mailer"
            }
        ));
    }
    // The other file an instance variable can be written in. Here the class is the reason, not a
    // convention: the code's own `<` or `include` put it above this one. Name the file as well as
    // the class: a concern is usually a file the reader never opened, and `AccountOwnedConcern`
    // alone does not say where to look.
    if let Some(from) = &derivation.ancestor {
        notes.push(format!(
            "Type taken from `{}` — `{}` line {} — which is above this class in its ancestry; \
             nothing in this file assigns it.",
            from.class, from.file, from.line
        ));
    }
    // Which method `super` climbed to: the one thing the body note beside it cannot say, since it
    // names the `def` the reader stands in, which is a bare `super`. A name, not a place: the note
    // above already gave a place, and the name is what a reader types into the jump box next.
    if let Some(reached) = &derivation.superclass {
        notes.push(climbed_to(reached));
    }
    // The view context: a convention for the same reason as the line above. The template says
    // neither which class renders it nor that `app/helpers` is in scope. Both name what a reader
    // would have to check.
    if let Some(reached) = &derivation.view {
        notes.push(match reached {
            views::InView::Helper => "Reached through the view context — Rails includes every \
                 `app/helpers` module in it."
                .to_owned(),
            views::InView::Exported(renderer) => format!(
                "Reached through `helper_method` in `{renderer}` — the class Rails renders this \
                 template from."
            ),
            views::InView::Framework => "Reached through the view context — ActionView includes \
                 its own helper modules in it."
                .to_owned(),
        });
    }
    // A block in a class body is the one place `self` is not what the file says: the block is a
    // value, and whoever takes it may run it against something else. So the line states the
    // evidence, not the conclusion (the name is not on the class object but is on an instance), and
    // names the class a reader would have to check.
    if let Some(class) = &derivation.closure {
        notes.push(format!(
            "Found on an instance of `{class}` — `self` in a block written into a class \
             body is the class object unless whoever takes the block re-binds it, and this \
             name is only on an instance."
        ));
    }
    // The symbol's own line: the only one of these about what the cursor is, not what a receiver
    // turned out to be.
    if let Some(macro_name) = &derivation.named_by {
        notes.push(format!(
            "Named by `{macro_name}` — the symbol is a method name because the macro says so, \
             not because the code spells it as a call."
        ));
    }
    // Last: it is the largest caveat on the card and the one a reader must not miss.
    if let Some(name) = &derivation.guess {
        notes.push(format!(
            "Type guessed from the name `{name}` alone — nothing in the code says so."
        ));
    }
    notes
}

/// What ya-lsp knows about an answer, as one italic line under it.
///
/// The one place this convention lives, so a single guessed match and a guessed list carry the same
/// fact in the same shape.
fn footnote(note: &str) -> String {
    format!("\n\n*{note}*")
}

fn card(
    synthesized: &Synthesized,
    modifiers: &locator::Modifiers<'_>,
    sources: &types::Sources<'_>,
    declaration_id: DeclarationId,
    cursor: Option<&str>,
) -> Option<String> {
    let graph: &Graph = sources.graph;
    let layout = sources.layout;
    let declaration = graph.declarations().get(&declaration_id)?;
    let definitions = locator::definitions_of(graph, declaration_id);
    // What a person wrote, and what ya-lsp wrote about it. That split is this function's rule:
    // prose from a file goes in the body, and prose this crate generated goes in a footnote. A
    // footnote is defined as "what ya-lsp knows about the answer", and a generated comment is
    // exactly that.
    let (generated, written): (Vec<&Definition>, Vec<&Definition>) = definitions
        .iter()
        .copied()
        .partition(|definition| synthesized.is_generated(definition.uri_id()));

    // **The half about the method, not the receiver**: show types even on methods that do not
    // declare them. A `def` whose return **is** declared draws nothing here. Showing a declared
    // return is a separate decision about the card's shape; the body rung only adds what it
    // answered.
    let returns = matches!(declaration, Declaration::Method(_))
        .then(|| {
            sources
                .types
                .declared_return(declaration_id)
                .is_none()
                .then(|| types::body_return(sources, declaration_id))
                .flatten()
        })
        .flatten();
    let mut card = format!(
        "```ruby\n{}{}\n```",
        signature(graph, modifiers, declaration_id, declaration, &definitions),
        // Through `render`, not off the declaration: a class nothing named is keyed
        // `<id>:<offset><anonymous>`, and a key must never be printed at a reader, just as for the
        // receiver half.
        returns
            .as_ref()
            .map_or_else(String::new, |typed| render::typed(graph, typed)
                .map_or_else(String::new, |spelled| format!(" -> {spelled}")))
    );

    if let Some(documentation) = written
        .iter()
        .find_map(|definition| render::documentation(definition.comments()))
    {
        card.push_str("\n\n---\n\n");
        card.push_str(&documentation);
    }

    // Reopened classes and monkey-patched methods are the norm in Ruby, and a hover cannot show
    // code that is elsewhere.
    //
    // **The count is the list**, asked of the function `definition` answers from:
    // - a generated declaration that maps to a line is a place; one that maps to nothing is not;
    // - an annotation typing a method the user wrote is the same place, not two;
    // - a signature, or a copy the project would not load, is not a place.
    //
    // A second arithmetic here would let a card claim a number no jump can produce. That is why the
    // cursor travels this far: `places` fences a copy only the suite loads, and a count that did
    // not would disagree with the jump from the same position.
    let places = locator::places(graph, synthesized, layout, declaration_id, cursor).len();
    if places > 1 {
        card.push_str(&footnote(&format!("Defined in {places} places.")));
    }

    // One line per self-explaining generated definition, deduplicated. Two generators writing the
    // same sentence about one member is their bug, not a fact to show twice.
    let mut said: Vec<String> = Vec::new();
    for documentation in generated
        .iter()
        .filter_map(|definition| render::documentation(definition.comments()))
    {
        if !said.contains(&documentation) {
            card.push_str(&footnote(&documentation));
            said.push(documentation);
        }
    }

    // Last, under everything the file said: the one line on this card that is ya-lsp's inference,
    // not the source's.
    if let Some(from) = returns
        .as_ref()
        .and_then(|typed| typed.derivation.body.as_ref())
    {
        card.push_str(&footnote(&format!(
            "Return type read out of the body — `{}` line {} — because nothing declares it.",
            from.file, from.line
        )));
    }
    // Under that, where the body went. The line above names the `def` a reader can open. When that
    // `def` is a bare `super` it says nothing alone, so this names what the keyword reached.
    if let Some(reached) = returns
        .as_ref()
        .and_then(|typed| typed.derivation.superclass.as_ref())
    {
        card.push_str(&footnote(&climbed_to(reached)));
    }

    Some(card)
}

/// The one sentence `super` earns, written once for the two cards that print it.
///
/// A member's own card and a receiver's chain both reach this rung. Two wordings would make a
/// reader wonder whether they meant the same thing.
fn climbed_to(reached: &str) -> String {
    format!("`super` here reaches `{reached}`, which is where the type was read from.")
}

fn signature(
    graph: &Graph,
    modifiers: &locator::Modifiers<'_>,
    declaration_id: DeclarationId,
    declaration: &Declaration,
    definitions: &[&Definition],
) -> String {
    let name = declaration.name();
    match declaration {
        Declaration::Namespace(namespace) => match namespace {
            // `Class.new` is the whole construct. There is no `class Foo` line to echo, so the call
            // stands alone, as `class << Book` does below.
            rubydex::model::declaration::Namespace::Class(_)
            | rubydex::model::declaration::Namespace::Module(_)
                if render::is_anonymous(name) =>
            {
                render::qualified_name(graph, name)
            }
            rubydex::model::declaration::Namespace::Class(_) => format!("class {name}"),
            rubydex::model::declaration::Namespace::Module(_) => format!("module {name}"),
            // `Person::<Person>` is rubydex's name for what the source writes as `class << self`
            // inside `class Person`.
            rubydex::model::declaration::Namespace::SingletonClass(_) => {
                format!("class << {}", attached_name(name))
            }
            rubydex::model::declaration::Namespace::Todo(_) => name.to_owned(),
        },
        Declaration::Method(_) => {
            let method = definitions.iter().find_map(|definition| match definition {
                Definition::Method(method) => Some(method),
                _ => None,
            });
            let parameters = method.map_or_else(String::new, |method| {
                render::parameter_list(graph, method.signatures())
            });
            let visibility = match method.map(|method| method.visibility()) {
                Some(Visibility::Public) | None => String::new(),
                // **The record is reread before the word is printed.** A bare `private` inside a
                // block is recorded against every `def` below the block. Trusting it would print
                // *private* over a public method: the false sentence the gate refuses to jump on,
                // arriving in the card instead. See `locator::Modifiers`.
                Some(Visibility::Private | Visibility::ModuleFunction)
                    if !modifiers.confirm(graph, declaration_id) =>
                {
                    String::new()
                }
                Some(other) => format!("{other} "),
            };
            format!(
                "{visibility}{}{parameters}",
                render::qualified_name(graph, name)
            )
        }
        _ => name.to_owned(),
    }
}

/// `Person::<Person>` -> `Person`, `Shelf::Book::<Book>` -> `Shelf::Book`, `<Person>` -> `Person`.
///
/// The part *before* the `::<`, not inside it. rubydex writes the attached name unqualified in the
/// brackets, so reading it there would give `class << Book` for a class every other card calls
/// `Shelf::Book`. A top-level module hides the difference, since both spellings are the same
/// string.
fn attached_name(name: &str) -> &str {
    match name.rsplit_once("::<") {
        Some((attached, _)) => attached,
        None => name.trim_start_matches('<').trim_end_matches('>'),
    }
}

fn candidate_list(
    graph: &Graph,
    declarations: &[DeclarationId],
    missed: Option<&locator::Missed>,
) -> String {
    // Paired with whether Ruby named it, which sorts `false` first. A `Class.new` bound to no
    // constant is a row the reader cannot look up, so it goes below every row they can; the list
    // has ten places. The pair also deduplicates, so two such rows collapse into one and the count
    // matches the list.
    let mut names: Vec<(bool, String)> = declarations
        .iter()
        .filter_map(|id| graph.declarations().get(id))
        .map(|declaration| {
            let name = declaration.name();
            (
                render::is_anonymous(name),
                render::qualified_name(graph, name),
            )
        })
        .collect();
    names.sort();
    names.dedup();

    let total = names.len();
    let mut listing = format!("**{total} possible definitions**\n");
    for (_, name) in names.iter().take(MAX_CANDIDATES) {
        listing.push_str(&format!("\n- `{name}`"));
    }
    if total > MAX_CANDIDATES {
        listing.push_str(&format!("\n- …and {} more", total - MAX_CANDIDATES));
    }
    // The list is the answer; why it is a list is the footnote, where every other card puts one. It
    // is the same sentence as the single-match card above: a reader facing eleven candidates needs
    // the reason more than one facing a single row.
    listing.push_str(&footnote(&why_guessed(missed)));
    listing
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    // `hover::card` is a different function with the same name. The tests want the harness helper,
    // and an explicit import outranks both globs.
    use crate::analysis::testing::card;

    #[test]
    fn a_run_of_one_signature_is_one_hop_in_the_footnote() {
        /// The note [`provenance`] writes for a chain that walked these declarations.
        fn through(walked: &[&str]) -> String {
            let derivation = types::Derivation {
                signatures: walked.iter().map(|name| (*name).to_owned()).collect(),
                ..types::Derivation::default()
            };
            provenance(&derivation, None).join("\n")
        }
        // The motivating shape: `Kernel#clone` returns its own receiver, so a chain of aliases
        // through it reads one declaration per hop. Nine hops, one fact.
        assert!(
            through(&["Kernel#clone()"; 9])
                .contains("through `Kernel#clone()` — from what those methods declare"),
            "{}",
            through(&["Kernel#clone()"; 9])
        );
        // A real chain is still a chain, and the arrows are what make it readable.
        assert!(
            through(&["String#upcase()", "String#length()"])
                .contains("through `String#upcase()` → `String#length()` —"),
            "{}",
            through(&["String#upcase()", "String#length()"])
        );
        // Consecutive, never global. An alternation is a walk a reader can follow; collapsing the
        // second `A#foo` into the first would claim a shorter chain.
        assert!(
            through(&["A#foo()", "B#bar()", "A#foo()"])
                .contains("through `A#foo()` → `B#bar()` → `A#foo()` —"),
            "{}",
            through(&["A#foo()", "B#bar()", "A#foo()"])
        );
    }

    #[test]
    fn a_type_read_through_super_names_the_method_super_reached() {
        // The one thing the body note beside it cannot say. That note names the `def` the reader
        // stands in, which is a bare `super`. Without this, the card says a type was read out of a
        // body whose whole text is `super`.
        let derivation = types::Derivation {
            superclass: Some("Base#name()".to_owned()),
            ..types::Derivation::default()
        };
        let notes = provenance(&derivation, None).join("\n");
        assert!(
            notes.contains("`super` here reaches `Base#name()`"),
            "{notes}"
        );
        // A name, not a place: it is what a reader types into the jump box next.
        assert!(!notes.contains("line"), "{notes}");
    }

    #[test]
    fn a_singleton_class_hovers_as_the_source_wrote_it() {
        assert_eq!(attached_name("Person::<Person>"), "Person");
        assert_eq!(attached_name("<Person>"), "Person");
        assert_eq!(attached_name("Person"), "Person");
    }

    #[test]
    fn hover_shows_the_signature_and_the_comment_above_it() {
        let (mut harness, uri) = library();
        let hover = harness.hover_at(&uri, LIBRARY, "shout(volume");
        let markdown = hover["contents"]["value"].as_str().expect("markdown");

        assert!(
            markdown.contains("Person#shout(volume = ..., *rest, sep:, &block)"),
            "{markdown}"
        );
        assert!(markdown.contains("Shout it."), "{markdown}");
        assert_eq!(hover["contents"]["kind"], "markdown");
    }

    #[test]
    fn an_anonymous_rest_parameter_hovers_as_ruby_wrote_it() {
        // rubydex records an anonymous `*`, `**` or `&` under the sigil itself, not an empty name,
        // so adding a sigil in `render` would spell `**` as `****`. The pure test in `render` pins
        // the spelling; this pins rubydex's convention, which rubydex may change.
        let mut harness = Harness::new();
        let source = "class Relay\n  def send_on(one, *, k:, **, &)\n  end\nend\n";
        let uri = harness.write("lib/relay.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "send_on")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Relay#send_on(one, *, k:, **, &)"),
            "{markdown}"
        );
    }

    #[test]
    fn every_kind_of_parameter_ruby_has_is_spelled_the_way_it_was_written() {
        // `render::parameter_list` has an arm per `Parameter` variant, including an optional
        // keyword and a forwarding `...`. `def call(retries: 3)` is ordinary Ruby, and hover is the
        // only place a reader learns the argument is optional: `retries:` and `retries: ...` say
        // different things.
        let mut harness = Harness::new();
        let source = "class Job\n  def call(one, two = 1, *rest, key:, opt: 2, **kw, &blk)\n                        end\n\n  def forward(...)\n  end\nend\n";
        let uri = harness.write("lib/job.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "call(one")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Job#call(one, two = ..., *rest, key:, opt: ..., **kw, &blk)"),
            "{markdown}"
        );

        let forwarding = harness.hover_at(&uri, source, "forward(")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(forwarding.contains("Job#forward(...)"), "{forwarding}");
    }

    #[test]
    fn a_singleton_method_hovers_as_ruby_spells_it() {
        // rubydex calls this `Person::<Person>#build()`. Showing that would show the user the
        // index's internals.
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "build(name)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person.build(name)"), "{markdown}");
    }

    #[test]
    fn hover_names_every_construct_the_way_ruby_writes_it() {
        // `hover::signature` has an arm per kind of declaration, and each must be asserted.
        // Otherwise a module hovering as `class`, or a private method without its visibility, would
        // ship under a green suite.
        let mut harness = Harness::new();
        let source = "\
# A place to keep things.
module Storage
  LIMIT = 10

  class << self
    # Wipe it.
    def reset
    end
  end

  # Stash a thing.
  private def stash(thing)
  end

  protected def peek
  end
end
";
        let uri = harness.write("app/storage.rb", source);
        harness.index();

        let markdown = |harness: &mut Harness, needle: &str| -> String {
            harness.hover_at(&uri, source, needle)["contents"]["value"]
                .as_str()
                .unwrap_or_else(|| panic!("no hover on {needle:?}"))
                .to_owned()
        };

        let module = markdown(&mut harness, "Storage\n");
        assert!(module.contains("module Storage"), "{module}");
        assert!(module.contains("A place to keep things."), "{module}");

        // rubydex spells this `Storage::<Storage>`, which is not what the file says. The cursor is
        // on `self`, not the keyword: a definition matches its *name* span, which for
        // `class << self` is the receiver, so hover does not fire over `class` either.
        assert!(harness.hover_at(&uri, source, "class << self").is_null());
        let singleton = markdown(&mut harness, "self");
        assert!(singleton.contains("class << Storage"), "{singleton}");

        // The visibility prefix, the main reason to hover a method you did not write: `stash` is
        // callable from inside `Storage` and nowhere else.
        let private = markdown(&mut harness, "stash(thing)");
        assert!(private.contains("private "), "{private}");
        assert!(private.contains("Storage#stash(thing)"), "{private}");
        assert!(private.contains("Stash a thing."), "{private}");

        let protected = markdown(&mut harness, "peek\n");
        assert!(protected.contains("protected "), "{protected}");

        // A constant is neither a namespace nor a method, and has no signature to render.
        let constant = markdown(&mut harness, "LIMIT");
        assert!(constant.contains("Storage::LIMIT"), "{constant}");
    }

    /// Every construct in [`GALLERY`], in source order, with its card drawn under it.
    fn gallery_cards(harness: &mut Harness, uri: &DocUri) -> String {
        [
            "Shelf\n",
            "LIMIT = 10",
            "CAP",
            "Book < Object",
            "@@printed",
            "@title = title",
            "title(upcase:",
            "name title",
            "open(*paths)",
            "self\n",
            "shut",
            "hide",
            "peek",
            "$shelf",
        ]
        .into_iter()
        .map(|needle| {
            let found = harness.hover_at(uri, GALLERY, needle);
            let card = found["contents"]["value"].as_str().unwrap_or("null");
            let drawn: String = card
                .lines()
                .map(|line| {
                    if line.is_empty() {
                        "\n".to_owned()
                    } else {
                        format!("  {line}\n")
                    }
                })
                .collect();
            format!("{}\n{drawn}", needle.trim_end())
        })
        .collect()
    }

    #[test]
    fn every_hover_card_in_one_file_drawn_side_by_side() {
        // The first-ten treatment, for an answer that is not a list. `ANCESTRY` pins ten rows
        // because a ranking is a composition; a hover card is too. Checked one `contains` at a
        // time, no two cards are read side by side, and one card can drift (say, the singleton card
        // dropping its namespace: `class << Book` above `private Shelf::Book#hide`) unnoticed.
        //
        // Pinned whole, and pinned *together*: this catches one card drifting from the others,
        // which per-card assertions cannot see.
        let mut harness = Harness::new();
        let uri = harness.write("app/shelf.rb", GALLERY);
        harness.index();

        assert_eq!(
            gallery_cards(&mut harness, &uri),
            "\
Shelf
  ```ruby
  module Shelf
  ```

  ---

  Everything on a shelf.
LIMIT = 10
  ```ruby
  Shelf::LIMIT
  ```
CAP
  ```ruby
  Shelf::CAP
  ```
Book < Object
  ```ruby
  class Shelf::Book
  ```

  ---

  A thing on it.
@@printed
  ```ruby
  Shelf::Book#@@printed
  ```
@title = title
  ```ruby
  Shelf::Book#@title
  ```
title(upcase:
  ```ruby
  Shelf::Book#title(upcase: ..., &block)
  ```

  ---

  What it is called.
name title
  ```ruby
  Shelf::Book#name
  ```
open(*paths)
  ```ruby
  Shelf::Book.open(*paths)
  ```
self
  ```ruby
  class << Shelf::Book
  ```
shut
  ```ruby
  Shelf::Book.shut
  ```
hide
  ```ruby
  private Shelf::Book#hide
  ```
peek
  ```ruby
  protected Shelf::Book#peek
  ```
$shelf
  ```ruby
  $shelf
  ```
"
        );
    }

    /// A `Class.new` and a `Module.new` with nothing binding either to a constant.
    const UNNAMED: &str = "\
helper = Class.new do
  def hop(a)
    self
  end
end

another = Class.new do
  def hop(a)
  end
end

mixin = Module.new do
  def hop(a)
  end
end

class Alpha
  def hop(a)
  end
end

class Beta
  def hop(a)
  end
end

anything.hop(1)
";

    #[test]
    fn a_class_ruby_never_named_hovers_as_the_call_that_built_it() {
        // `Class.new` is an expression, so what it builds has no name until something binds it to a
        // constant. Where nothing does, rubydex keys it by document and offset. A key is not a
        // name, and no card may print one at the user. Such classes are common in real apps, and
        // they often own methods a cursor can reach.
        let mut harness = Harness::new();
        let uri = harness.write("app/unnamed.rb", UNNAMED);
        harness.index();

        // The class itself, which `self` inside its body is how a cursor reaches.
        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "self\n"),
            "```ruby\nClass.new\n```"
        );
        // The method, from its own `def`, and its return, which the body rung reads from the `self`
        // in it. That return is the anonymous class too, so the label goes through `render` as
        // well, or the key would appear twice on one line.
        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "hop(a)\n    self"),
            "```ruby\nClass.new#hop(a) -> Class.new\n```\n\n*Return type read out of the \
             body — `app/unnamed.rb` line 2 — because nothing declares it.*"
        );
        // And a module, which rubydex spells exactly like the class, so the declaration is asked,
        // not assumed. In real apps most of these are modules, so a guess would usually be wrong.
        assert_eq!(
            card(
                &mut harness,
                &uri,
                UNNAMED,
                "hop(a)\n  end\nend\n\nclass Alpha"
            ),
            "```ruby\nModule.new#hop(a)\n```"
        );
    }

    #[test]
    fn a_list_of_candidates_spends_its_rows_on_the_names_a_reader_can_look_up() {
        // Five declarations of `hop`, three in namespaces Ruby never named. Sorted by key alone,
        // those come first (a digit sorts below every letter), so a card with forty candidates
        // would spend all ten rows on numbers. They rank last, and the two `Class.new`s collapse
        // into one row because they spell the same thing, which is what the count above the list
        // counts.
        let mut harness = Harness::new();
        let uri = harness.write("app/unnamed.rb", UNNAMED);
        harness.index();

        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "hop(1)"),
            "\
**4 possible definitions**

- `Alpha#hop`
- `Beta#hop`
- `Class.new#hop`
- `Module.new#hop`

*Matched on the method name alone — the receiver's type is unknown.*"
        );
    }

    #[test]
    fn hover_on_a_reopened_class_says_there_is_more_of_it() {
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "Person\n  MAX_AGE")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("class Person"), "{markdown}");
        assert!(markdown.contains("Someone with a name."), "{markdown}");
        // The point: the rest of the class is somewhere this hover cannot show.
        assert!(markdown.contains("Defined in 2 places"), "{markdown}");
    }

    #[test]
    fn a_core_method_hovers_as_rdoc_written_in_markdown() {
        // The whole card, not a `contains`. A hover card is a composition, and asserting parts one
        // `contains` at a time is how the two shapes of one answer (a guessed single match and a
        // guessed list) drift apart.
        //
        // Pinned along the way:
        // - `<code>self</code>` reaches the user as markdown, not a span a client silently eats;
        // - `[Case Mapping](rdoc-ref:…)` loses a link that goes nowhere but keeps its words;
        // - the call-seq is lifted out of RDoc's HTML header as Ruby;
        // - the indented example survives untouched.
        //
        // **No footnote at all.** `greeting` is a local assigned a string literal. Completion types
        // it exactly, and so does hover, through the same `types::method_receiver`, instead of
        // matching on the name. It is not *derived* either: a literal assigned one line up is code
        // the reader can see.
        let source = "greeting = \"hello\"\ngreeting.upcase\n";
        let (mut harness, uri) = with_signatures(source);
        assert_eq!(
            card(&mut harness, &uri, source, "upcase"),
            "```ruby\n\
             String#upcase(mapping = ...)\n\
             ```\n\
             \n\
             ---\n\
             \n\
             ```ruby\n\
             upcase(mapping = :ascii) -> new_string\n\
             ```\n\
             \n\
             Returns a new string containing `self`'s upcased characters:\n\
             \n\
             \u{20}   'hello'.upcase # => \"HELLO\"\n\
             \n\
             See Case Mapping."
        );
    }

    #[test]
    fn a_stdlib_method_hovers_the_same_way_a_core_one_does() {
        // A different directory under the rbs root, same card. `<tt>` is RDoc's other spelling of
        // `<code>` and appears throughout the vendored signatures; it must not leak either. No
        // footnote, for the same reason as above: `parser = OptionParser.new` is a receiver the
        // code names.
        let source = "parser = OptionParser.new\nparser.parse!\n";
        let (mut harness, uri) = with_signatures(source);
        assert_eq!(
            card(&mut harness, &uri, source, "parse!\n"),
            "```ruby\n\
             OptionParser#parse!(argv = ...)\n\
             ```\n\
             \n\
             ---\n\
             \n\
             ```ruby\n\
             parse!(argv = default_argv) -> argv\n\
             ```\n\
             \n\
             Parses `argv` in place and returns what is left of it."
        );
    }

    #[test]
    fn every_shape_of_card_puts_what_it_knows_in_the_same_place() {
        // The four cards side by side: the only way to see the convention. Answer first, then one
        // italic line per thing ya-lsp knows *about* the answer.
        // - A precise hit says nothing extra.
        // - A reopened class says where else it lives.
        // - A guess says it is a guess.
        // - A guess with several candidates says the same sentence in the same place.
        let source =
            "class Radio\n  def shout; end\nend\n\nPerson.build(\"x\")\nthing.shout\nthing.extra\n";
        let (mut harness, uri) = with_signatures(source);

        // Precise: a constant receiver is the one thing rubydex names without inference. The return
        // is the body rung's, and its footnote is the last line: documentation first, then what
        // ya-lsp knows *about* the answer.
        assert_eq!(
            card(&mut harness, &uri, source, "build("),
            "```ruby\nPerson.build(name) -> Person\n```\n\n---\n\nBuild one.\n\n*Return \
             type read out of the body — `lib/person.rb` line 10 — because nothing declares it.*"
        );

        // Reopened: a hover cannot show the half that is elsewhere.
        assert_eq!(
            card(&mut harness, &uri, source, "Person.build"),
            "```ruby\nclass Person\n```\n\n---\n\nSomeone with a name.\n\nReopened \
             below.\n\n*Defined in 2 places.*"
        );

        // One name-based match: a whole card, and the caveat under it.
        assert_eq!(
            card(&mut harness, &uri, source, "extra"),
            "```ruby\nPerson#extra\n```\n\n*Matched on the method name alone — the \
             receiver's type is unknown.*"
        );

        // Several: a list, with the same caveat in the same place. Naming one would present a coin
        // flip as an answer.
        assert_eq!(
            card(&mut harness, &uri, source, "shout\n"),
            "**2 possible definitions**\n\n- `Person#shout`\n- `Radio#shout`\n\n*Matched on \
             the method name alone — the receiver's type is unknown.*"
        );
    }

    #[test]
    fn an_anonymous_class_is_named_by_the_call_that_built_it() {
        // **A type that cannot be *opened* is still a type that can be *said*.** rubydex keys a
        // `Class.new` bound to no constant by document and offset. `render::spelled` replaces that
        // key with the call that built it on every surface, this card's candidate list included.
        // `locator::missed` must do the same instead of reading the raw key, failing `is_nameable`,
        // and falling back to *the receiver's type is unknown*. The type is known; it just has no
        // name to open.
        //
        // That sentence would also contradict `completion`, which offers this class's own members
        // at the same cursor.
        let source = "class Radio\n  def ping; end\nend\n\nClass.new do\n  self.ping\nend\n";
        let (mut harness, uri) = with_signatures(source);

        // **The class object, and that half depends on the ordering.** `self` in a class body is
        // the singleton, which rubydex spells `<key><anonymous>::<<key><anonymous>>`.
        // `class_object_of` cannot recognise that: its `prefix.ends_with(singleton)` test fails on
        // the raw key. Spelled first, it reads `Class.new::<Class.new>`, which it recognises
        // exactly, so the sentence gets both facts.
        //
        // `"ping\n"` finds the call, not the `def ping; end` above it, which has a `;`.
        assert_eq!(
            card(&mut harness, &uri, source, "ping\n"),
            "```ruby\nRadio#ping\n```\n\n*Matched on the method name alone — the receiver is the \
             class object `Class.new`, which has no such method.*"
        );
    }

    #[test]
    fn a_guessed_card_says_which_of_the_two_guesses_it_is() {
        // **"The receiver has no type" and "the receiver has no such member" are different facts.**
        // Printing the first for both contradicts `completion`, which at the same cursor answers
        // from the class. At a class object that is nearly always the case: the card would say
        // *unknown* over a list of that class's own members.
        let source = "class Radio\n  def tune; end\n  def dial; end\n  def amp; end\n  \
                      def hum; end\nend\n\n\
                      person = Person.new(\"x\")\nperson.tune\nPerson.dial\n@person.amp\n\
                      gadget = Unknown.new\ngadget.hum\n";
        let (mut harness, uri) = with_signatures(source);

        // Typed outright: the class is named, so a reader can open it and see it has no `tune`.
        assert_eq!(
            card(&mut harness, &uri, source, "tune\nPerson"),
            "```ruby\nRadio#tune\n```\n\n*Matched on the method name alone — the receiver is a \
             `Person`, which has no such method.*"
        );

        // A class object is a type Ruby cannot spell, so the sentence says what it is instead of
        // printing rubydex's `Person::<Person>` at someone who cannot look it up.
        assert_eq!(
            card(&mut harness, &uri, source, "dial\n@person"),
            "```ruby\nRadio#dial\n```\n\n*Matched on the method name alone — the receiver is the \
             class object `Person`, which has no such method.*"
        );

        // The guess, the case real code is full of: two weak claims, and the card must make both,
        // not just the stronger one.
        assert_eq!(
            card(&mut harness, &uri, source, "amp\n"),
            "```ruby\nRadio#amp\n```\n\n*Matched on the method name alone — the receiver was \
             guessed from the name `@person` to be a `Person`, which has no such method.*"
        );

        // The one type that is not a name. `Unknown` is a constant no file defines, so rubydex
        // promotes it to a `Namespace::Todo` whose every lookup misses. Naming it would put an
        // undeclared class on the card, so the sentence falls back to the one that is always true.
        assert_eq!(
            card(&mut harness, &uri, source, "hum\n"),
            "```ruby\nRadio#hum\n```\n\n*Matched on the method name alone — the receiver's type \
             is unknown.*"
        );

        // The other half of the contradiction: the list at that same cursor is `Person`'s members.
        let offered = harness.complete(&uri, &source.replace("person.tune", "person.t~une"));
        let owners: Vec<&str> = offered["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["detail"].as_str())
            .collect();
        assert!(
            owners.contains(&"Person#shout"),
            "completion still answers from the class hover called unknown: {owners:?}"
        );
    }

    #[test]
    fn hover_reads_a_method_that_no_def_wrote() {
        // `attr_reader :name` declares a method whose definition is not a `Definition::Method`, so
        // the signature lookup finds no parameters or visibility. It must still name the method
        // instead of falling through to a bare string: `attr_reader` declares a large share of a
        // Rails app's methods.
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "name\n\n  # Build")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person#name"), "{markdown}");
        assert!(
            !markdown.contains('('),
            "no parameter list to render: {markdown}"
        );
    }
}
