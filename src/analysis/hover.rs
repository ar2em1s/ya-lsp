//! `textDocument/hover` — what the thing under the cursor is, as markdown.

use rubydex::model::{
    declaration::Declaration, definitions::Definition, graph::Graph, ids::DeclarationId,
    visibility::Visibility,
};

use super::{
    environment,
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
/// Every hover reads the same way, and the order is the point: **the answer, then what ya-lsp
/// knows about the answer.** A fenced signature, a rule, RDoc's prose, and then the footnotes —
/// one italic line each, at the bottom, never woven into the text above. A reader who trusts
/// the answer stops at the fence; a reader who does not gets told why in the same place every
/// time.
///
/// Everything a footnote says is about ya-lsp's confidence rather than about the code: the
/// receiver's type was not known, or was derived rather than stated, or the class is reopened
/// somewhere this card cannot show. That is the test for whether a new line belongs in one.
///
/// # The three tiers, and why they are said out loud
///
/// There are three answers rather than two, and a reader who cannot tell them
/// apart has lost the property that makes this server different from one that guesses well:
///
/// - **resolved** — the code names the type. No footnote; there is nothing to doubt.
/// - **derived** — ya-lsp followed something: a return type RBS declares, an assignment in the
///   same class. Correct if the signature is correct and if that assignment is the one that
///   ran. The footnote names what was followed, so a reader can go and check it.
/// - **guessed** — matched on the method name alone. The footnote it always had.
///
/// `assignment_line` is the line an instance variable's assignment is on, one-based, because a
/// card names a place a person can go to. It arrives already converted: the offset is turned
/// into a line where the document's text is, which is not here.
#[must_use]
pub fn markdown(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    modifiers: &locator::Modifiers<'_>,
    resolution: &Resolution,
    assignment_line: Option<u32>,
    cursor: Option<&str>,
) -> Option<String> {
    match resolution.declarations.as_slice() {
        [] => None,
        [only] => {
            let mut card = card(graph, synthesized, layout, modifiers, *only, cursor)?;
            if !resolution.precise {
                card.push_str(&footnote(&why_guessed(resolution.missed.as_ref())));
            }
            for note in provenance(&resolution.derivation, assignment_line) {
                card.push_str(&footnote(&note));
            }
            Some(card)
        }
        // Only the name-based fallback can produce more than one, and picking one of them
        // arbitrarily would be presenting a coin flip as an answer.
        many => Some(candidate_list(graph, many, resolution.missed.as_ref())),
    }
}

const GUESS: &str = "Matched on the method name alone — the receiver's type is unknown.";

/// Why a guessed answer is a guess, which is not the same sentence every time.
///
/// **The receiver having no type and the receiver having no such member are different facts,
/// and for two releases this card printed the first for both.** `completion` at the identical
/// cursor never reaches a member lookup, so it goes on offering that class's members — which
/// made *the receiver's type is unknown* a contradiction a user could see by pressing one more
/// key. Measured over six corpora before the split, as cards saying the type was unknown while
/// the list at that cursor was a class's members: **207 of 1,506** at an instance variable and
/// **180 of 209** at a class object.
///
/// The tier is untouched and so is the answer. A list matched on a name is a guess whichever
/// of the two put it there, and [`Tier::Guessed`](types::Tier) is what a reader acts on; this
/// only says which, so that the half with a class in it can be checked.
fn why_guessed(missed: Option<&locator::Missed>) -> String {
    let Some(missed) = missed else {
        return GUESS.to_owned();
    };
    let receiver = if missed.class_object {
        format!("the class object `{}`", missed.class)
    } else {
        format!("a `{}`", missed.class)
    };
    // **What the class did with the member, and the two are not the same fact.** A lookup that
    // came back empty means the class has no such method; a lookup the privacy gate refused
    // means it has one and Ruby will not let it be called through a receiver that is written.
    // Printing the first sentence for the second was the shape of defect this whole gate exists
    // to remove, so it is not introduced here on the way out. See `locator::Missed::private`.
    // **"keeps" and not "declares", because the member is usually inherited.** `RSpec.describe`
    // is `Kernel`'s, reached through `Object`; `RSpec` does not declare it and does keep it
    // private, which is the fact a reader needs and the only one of the two that is true.
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
/// One line per *kind* of thing followed rather than one per step: a chain of three signatures
/// is one fact about this answer, not three, and three italic lines would read as three
/// separate doubts. The signatures are named the way rubydex names them — `Kernel#tap()` and
/// not `Array#tap()` — because that is the declaration the type actually came from and the one
/// a reader would have to open.
///
/// **The line deliberately does not say where a signature came from.** A signature may be
/// `vendor/rbs`, a gem's own `sig/`, or text this crate generated, and telling them apart costs
/// a lookup this line does not do. What it can say without checking anything is the useful
/// half: the type was read off a declaration rather than off the expression under the cursor.
/// Where *that* declaration came from is a question the card answers by hovering it, through
/// the comment its generator wrote above it.
///
/// **Shared with [`hints`](super::hints) rather than copied**, and that is the whole reason it
/// is not private. An inlay hint's tooltip is this card's footnote in the only room a hint has,
/// so the two saying different things about one derived answer would be a difference nobody
/// could see — the card and the margin are never on screen at the same moment.
pub(super) fn provenance(
    derivation: &types::Derivation,
    assignment_line: Option<u32>,
) -> Vec<String> {
    let mut notes = Vec::new();
    if !derivation.signatures.is_empty() {
        notes.push(format!(
            "Type derived through {} — from what those methods declare, not from this expression.",
            derivation
                .signatures
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(" → ")
        ));
    }
    // Beside the signatures rather than among them, because the line above says "those
    // methods" and a constant is not one. The same strength of evidence and the same tier: the
    // type is written down in a signature rather than read off the expression.
    if let Some(constant) = &derivation.constant {
        notes.push(format!(
            "Type taken from the signature for `{constant}` — what it declares the constant \
             holds, not what this expression says."
        ));
    }
    // The same fact as the note above it, written in Ruby rather than in RBS, and said
    // differently for that reason: one names a signature a reader can go and read, the other
    // names a line that will have run. Naming the file is what makes the second actionable —
    // the constant is on screen already and the initializer that builds it is not.
    // One shape and not two: `types::where_written` never answers empty, so there is no case
    // here where the file has to be left out of the sentence.
    if let Some(from) = &derivation.assigned_constant {
        notes.push(format!(
            "Type taken from where `{}` is assigned — `{}` line {} — and not from what this \
             expression says.",
            from.constant, from.file, from.line
        ));
    }
    if let Some(line) = assignment_line {
        notes.push(format!(
            "Type taken from the assignment on line {line}, which may not be the one that ran."
        ));
    }
    // A convention rather than a fact about this file, so the line it names is in another one.
    // Naming both is the whole of what makes the convention shippable: a reader who thinks Rails
    // renders this template from somewhere else can go and look.
    if let Some(from) = &derivation.renderer {
        notes.push(format!(
            "Type taken from `{}`, line {} — the {} Rails renders this template from.",
            from.renderer,
            from.line,
            // A mailer is not a controller, and this footnote is read as a claim about the one
            // class it names. `views::RenderedBy` carries which convention answered for exactly
            // this sentence.
            if from.controller {
                "controller"
            } else {
                "mailer"
            }
        ));
    }
    // The view context, and it is a convention for the same reason the line above is: the template
    // does not say which class renders it, and it does not say that `app/helpers` is in scope
    // either. Both name what a reader would have to go and check.
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
    // A block in a class body is the one place `self` is not what the file says it is: the
    // block is a value, and whoever takes it may run it against something else. So the line
    // states the evidence rather than the conclusion — the name is not on the class object and
    // is on an instance — and names the class, which is what a reader would have to go and
    // check.
    if let Some(class) = &derivation.closure {
        notes.push(format!(
            "Found on an instance of `{class}` — `self` in a block written into a class \
             body is the class object unless whoever takes the block re-binds it, and this \
             name is only on an instance."
        ));
    }
    // The symbol's own line, and the only one of these that is about what the cursor is rather
    // than about what a receiver turned out to be.
    if let Some(macro_name) = &derivation.named_by {
        notes.push(format!(
            "Named by `{macro_name}` — the symbol is a method name because the macro says so, \
             not because the code spells it as a call."
        ));
    }
    // Last, because it is the largest caveat on the card and the one a reader must not miss.
    if let Some(name) = &derivation.guess {
        notes.push(format!(
            "Type guessed from the name `{name}` alone — nothing in the code says so."
        ));
    }
    notes
}

/// What ya-lsp knows about an answer, as one italic line under it.
///
/// The single place the convention lives, because it was two before: a guessed single match
/// carried it as a trailing italic and a guessed *list* carried the same sentence inline after
/// an em dash, in bold, at the top. Same fact, same uncertainty, two shapes.
fn footnote(note: &str) -> String {
    format!("\n\n*{note}*")
}

fn card(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    modifiers: &locator::Modifiers<'_>,
    declaration_id: DeclarationId,
    cursor: Option<&str>,
) -> Option<String> {
    let declaration = graph.declarations().get(&declaration_id)?;
    let definitions = locator::definitions_of(graph, declaration_id);
    // What a person wrote, and what ya-lsp wrote about it. The split is the whole of this
    // function's rule: prose from a file goes in the body, and prose this crate generated goes
    // in a footnote, because a footnote is already defined as "what ya-lsp knows about the
    // answer" and a generated comment is nothing else.
    let (generated, written): (Vec<&Definition>, Vec<&Definition>) = definitions
        .iter()
        .copied()
        .partition(|definition| synthesized.is_generated(definition.uri_id()));

    let mut card = format!(
        "```ruby\n{}\n```",
        signature(graph, modifiers, declaration_id, declaration, &definitions)
    );

    if let Some(documentation) = written
        .iter()
        .find_map(|definition| render::documentation(definition.comments()))
    {
        card.push_str("\n\n---\n\n");
        card.push_str(&documentation);
    }

    // Reopened classes and monkey-patched methods are the norm in Ruby, and the one thing a
    // hover cannot show is the code that is somewhere else. **The count is the list**, asked of
    // the one function `definition` answers from: a generated declaration that maps to a line is
    // a place and one that maps to nothing is not, an annotation that types a method the user
    // already wrote is the same place twice rather than two of them, and a signature or a copy
    // the project would not load is not a place at all. A second arithmetic here is how a card
    // comes to claim a number no jump can produce — which is why the cursor travels this far:
    // `places` fences a copy only the suite loads, and a count that did not would disagree with
    // the jump the reader takes from the same position.
    let places = locator::places(graph, synthesized, layout, declaration_id, cursor).len();
    if places > 1 {
        card.push_str(&footnote(&format!("Defined in {places} places.")));
    }

    // One line per generated definition that explains itself, deduplicated, because two
    // generators writing the same sentence about one member is a bug in them and not a fact a
    // reader should be shown twice.
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

    Some(card)
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
            // `Class.new` is the whole construct — there is no `class Foo` line to echo back,
            // so the call stands on its own the way `class << Book` does below.
            rubydex::model::declaration::Namespace::Class(_)
            | rubydex::model::declaration::Namespace::Module(_)
                if render::is_anonymous(name) =>
            {
                render::qualified_name(graph, name)
            }
            rubydex::model::declaration::Namespace::Class(_) => format!("class {name}"),
            rubydex::model::declaration::Namespace::Module(_) => format!("module {name}"),
            // `Person::<Person>` is rubydex's name for what the source writes as
            // `class << self` inside `class Person`.
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
                // **The record is reread before the word is printed.** A bare `private` written
                // inside a block is recorded against every `def` below the block, so this line
                // printed *private* over a public method — the same false sentence the gate
                // refuses to jump on, arriving in the card instead. See `locator::Modifiers`.
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

/// `Person::<Person>` -> `Person`, `Shelf::Book::<Book>` -> `Shelf::Book`, `<Person>` ->
/// `Person`.
///
/// The part *before* the `::<`, not the part inside it: rubydex writes the attached name
/// unqualified there, so reading it out of the brackets gives `class << Book` for a class every
/// other card on the same page calls `Shelf::Book`. Nothing was wrong with the old spelling
/// until a fixture put the cards side by side — the one test that covered this construct used a
/// top-level module, where the two spellings are the same string.
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
    // Paired with whether Ruby named it, which sorts `false` first: a `Class.new` nothing
    // bound to a constant is a row the reader cannot look up, so it goes below every row they
    // can — the list has ten places and the alphabet was handing them to whatever sorted
    // first. The pair is what deduplicates, so two of them collapse into one row and the
    // count says what the list says.
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
    // The list is the answer; why it is a list is the footnote, in the place every other card
    // puts one — and it is the same sentence the single-match card above draws, because a
    // reader seeing eleven candidates needs the reason more than one seeing a single row does.
    listing.push_str(&footnote(&why_guessed(missed)));
    listing
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    // `hover::card` is a different function with the same name; the tests here want the
    // harness helper, and an explicit import outranks both globs.
    use crate::analysis::testing::card;

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
        // rubydex records an anonymous `*`, `**` or `&` under the sigil itself rather than
        // under an empty name, so prepending a second sigil in `render` would spell `**` as
        // `****`. The pure test in `render` pins the spelling; this pins the convention it is
        // written against, which is rubydex's to change.
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
        // `render::parameter_list` has an arm per `Parameter` variant and two had never been
        // asked for — an optional keyword and a forwarding `...`. `def call(retries: 3)` is
        // ordinary Ruby, and its hover is the only place a reader learns the argument is
        // optional at all: `retries:` and `retries: ...` say different things.
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
        // rubydex calls this `Person::<Person>#build()`. Showing that to a user would be
        // showing them the index's internals.
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "build(name)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person.build(name)"), "{markdown}");
    }

    #[test]
    fn hover_names_every_construct_the_way_ruby_writes_it() {
        // `hover::signature` has an arm per kind of declaration and only two of them — a class
        // and a public method — had ever been asked for. The rest were reachable, rendered, and
        // asserted nowhere: a module hovering as `class`, or a private method hovering without
        // its visibility, would have gone out under a green suite.
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

        // rubydex spells this `Storage::<Storage>`, which is not what the file says.
        // On `self`, not on the keyword: a definition matches its *name* span, which for
        // `class << self` is the receiver, so hover does not fire over the `class` either.
        assert!(harness.hover_at(&uri, source, "class << self").is_null());
        let singleton = markdown(&mut harness, "self");
        assert!(singleton.contains("class << Storage"), "{singleton}");

        // The visibility prefix, which is the whole reason a reader hovers a method they did
        // not write: `stash` is callable from inside `Storage` and nowhere else.
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
        // because a ranking is composition rather than a feature; a hover card is the same kind
        // of object, and until this existed every construct was checked by a `contains`
        // somewhere and no two were ever read next to each other. Which is how the singleton
        // card came to be the only one on this page that drops its namespace — `class << Book`
        // above a `private Shelf::Book#hide` — through a test that covered the construct, on a
        // top-level module where the two spellings are the same string.
        //
        // Pinned whole, and pinned *together*: the failure this shape catches is one card
        // drifting away from the others, which every card asserted on its own is blind to.
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
        // `Class.new` is an expression, so what it builds has no name until something binds it
        // to a constant — and where nothing does, rubydex keys it by document and offset.
        // Every card that reached one was printing that key at the user, and a key is not a
        // name: measured over the five corpora, 571 of these own a method and so can be
        // reached, and not one is bound to a constant that would name it.
        let mut harness = Harness::new();
        let uri = harness.write("app/unnamed.rb", UNNAMED);
        harness.index();

        // The class itself, which `self` inside its body is how a cursor reaches.
        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "self\n"),
            "```ruby\nClass.new\n```"
        );
        // The method, from its own `def`.
        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "hop(a)\n    self"),
            "```ruby\nClass.new#hop(a)\n```"
        );
        // And a module, which rubydex spells exactly as it spells the class — so the
        // declaration is asked rather than assumed. Over the five corpora 365 of those 571 are
        // modules, which is the majority a guess would have got wrong.
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
        // Five declarations of `hop`, three of them in namespaces Ruby never named. Sorting
        // the keys alphabetically put those first — a digit sorts below every letter — so a
        // card with forty candidates spent all ten of its rows on numbers. They rank last now,
        // and the two `Class.new`s collapse into one row because they spell the same thing,
        // which is what the count above the list counts.
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
        // The whole point: the rest of the class is in a place this hover cannot show.
        assert!(markdown.contains("Defined in 2 places"), "{markdown}");
    }

    #[test]
    fn a_core_method_hovers_as_rdoc_written_in_markdown() {
        // The whole card, not a `contains`. A hover card is a composition, so asserting its
        // parts one `contains` at a time is how the two shapes of one answer — a guessed single
        // match and a guessed list — drift apart.
        //
        // What this pins on the way past: `<code>self</code>` reaching the user as markdown
        // rather than as a span a client silently eats, `[Case Mapping](rdoc-ref:…)` losing a
        // link that goes nowhere while keeping its words, the call-seq lifted out of RDoc's HTML
        // header as Ruby, and the indented example surviving untouched.
        //
        // **No footnote at all.** `greeting` is a local assigned a string literal, which
        // completion types exactly and which hover would otherwise match on the name. Both go
        // through `types::method_receiver`, so a card that would carry "matched on the method
        // name alone" carries nothing instead — there is nothing to doubt, and it is not a
        // *derived* answer either: a literal assigned one line up is code the reader can see.
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
        // Different directory under the rbs root, same card. `<tt>` is RDoc's other spelling of
        // `<code>` and appears 22 times in the vendored signatures; it must not be the one that
        // still leaks. The footnote went the same way it did above, and for the same reason:
        // `parser = OptionParser.new` is a receiver the code names.
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
        // The four cards side by side, which is the only way the convention is visible: answer
        // first, then one italic line per thing ya-lsp knows *about* the answer. A precise hit
        // says nothing extra; a reopened class says where else it lives; a guess says it is a
        // guess; and a guess with more than one candidate says the same sentence in the same
        // place, rather than in bold at the top after an em dash.
        let source =
            "class Radio\n  def shout; end\nend\n\nPerson.build(\"x\")\nthing.shout\nthing.extra\n";
        let (mut harness, uri) = with_signatures(source);

        // Precise: a constant receiver is the one thing rubydex can name without inference.
        assert_eq!(
            card(&mut harness, &uri, source, "build("),
            "```ruby\nPerson.build(name)\n```\n\n---\n\nBuild one."
        );

        // Reopened, and the one thing a hover cannot show is the half that is elsewhere.
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

        // Several: a list, and the same caveat in the same place. Naming one of them would be
        // presenting a coin flip as an answer.
        assert_eq!(
            card(&mut harness, &uri, source, "shout\n"),
            "**2 possible definitions**\n\n- `Person#shout`\n- `Radio#shout`\n\n*Matched on \
             the method name alone — the receiver's type is unknown.*"
        );
    }

    #[test]
    fn an_anonymous_class_is_named_by_the_call_that_built_it() {
        // **A type that cannot be *opened* is not a type that cannot be *said*, and this card
        // collapsed the two.** rubydex keys a `Class.new` nothing binds to a constant by
        // document and offset, `render::spelled` replaces that key with the call that built it
        // on five other surfaces — the candidate list on this very card among them — and
        // `locator::missed` was the one place that read the key raw, failed `is_nameable` and
        // fell back to *the receiver's type is unknown*. The type is known. It has no name to
        // open, which is a different sentence.
        //
        // It is also the sentence `completion` contradicts: a cursor here is offered that
        // class's own members, which is what the audit's check 6 holds a card against. Defect
        // 34 was the other half of this — a `self` captured *outside* such a block, where the
        // answer was wrong as well as the sentence.
        let source = "class Radio\n  def ping; end\nend\n\nClass.new do\n  self.ping\nend\n";
        let (mut harness, uri) = with_signatures(source);

        // **The class object, and that half is the ordering.** `self` in a class body is the
        // singleton, which rubydex spells `<key><anonymous>::<<key><anonymous>>` — a shape
        // `class_object_of` cannot recognise, because its `prefix.ends_with(singleton)` test
        // fails on the raw key. Spelled first it reads `Class.new::<Class.new>`, which it
        // recognises exactly, so the sentence gets both facts rather than neither.
        //
        // `"ping\n"` finds the call and not the `def ping; end` above it, which has a `;`.
        assert_eq!(
            card(&mut harness, &uri, source, "ping\n"),
            "```ruby\nRadio#ping\n```\n\n*Matched on the method name alone — the receiver is the \
             class object `Class.new`, which has no such method.*"
        );
    }

    #[test]
    fn a_guessed_card_says_which_of_the_two_guesses_it_is() {
        // **The receiver having no type and the receiver having no such member are different
        // facts, and this card printed the first for both.** Measured over six corpora as
        // cards saying the type was unknown while `completion` at the identical cursor
        // answered from a class: 207 of 1,506 at an instance variable and 180 of 209 at a
        // class object. That second number is what makes it a contradiction rather than
        // merely a thin sentence — nearly every class-object card that said *unknown* sat
        // over a list of that class's own members.
        let source = "class Radio\n  def tune; end\n  def dial; end\n  def amp; end\n  \
                      def hum; end\nend\n\n\
                      person = Person.new(\"x\")\nperson.tune\nPerson.dial\n@person.amp\n\
                      gadget = Unknown.new\ngadget.hum\n";
        let (mut harness, uri) = with_signatures(source);

        // Typed outright: the class is named, because a reader can open it and see for
        // themselves that it has no `tune`.
        assert_eq!(
            card(&mut harness, &uri, source, "tune\nPerson"),
            "```ruby\nRadio#tune\n```\n\n*Matched on the method name alone — the receiver is a \
             `Person`, which has no such method.*"
        );

        // A class object is a type Ruby cannot spell, so the sentence says what it is rather
        // than printing rubydex's `Person::<Person>` at somebody who cannot go and look at it.
        assert_eq!(
            card(&mut harness, &uri, source, "dial\n@person"),
            "```ruby\nRadio#dial\n```\n\n*Matched on the method name alone — the receiver is the \
             class object `Person`, which has no such method.*"
        );

        // And the guess, which is the one the corpora are full of: two weak claims, and the
        // card has to make both of them rather than the stronger one.
        assert_eq!(
            card(&mut harness, &uri, source, "amp\n"),
            "```ruby\nRadio#amp\n```\n\n*Matched on the method name alone — the receiver was \
             guessed from the name `@person` to be a `Person`, which has no such method.*"
        );

        // And the one type that is not a name: `Unknown` is a constant no file defines, so
        // rubydex promotes it to a `Namespace::Todo` whose every lookup misses by construction.
        // Naming it would put a class on the card that nothing declares, so the sentence falls
        // back to the one that was always true.
        assert_eq!(
            card(&mut harness, &uri, source, "hum\n"),
            "```ruby\nRadio#hum\n```\n\n*Matched on the method name alone — the receiver's type \
             is unknown.*"
        );

        // The other half of the sentence, which is the contradiction it was measured by: the
        // list at that same cursor is `Person`'s members and always was.
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
        // `attr_reader :name` declares a method whose definition is not a `Definition::Method`,
        // so the signature lookup finds nothing to read parameters or visibility from. It still
        // has to name the method rather than fall through to a bare string — and `attr_reader`
        // is how a large share of a Rails app's methods are declared.
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
