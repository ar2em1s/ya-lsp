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
    resolution: &Resolution,
    assignment_line: Option<u32>,
) -> Option<String> {
    match resolution.declarations.as_slice() {
        [] => None,
        [only] => {
            let mut card = card(graph, synthesized, *only)?;
            if !resolution.precise {
                card.push_str(&footnote(GUESS));
            }
            for note in provenance(&resolution.derivation, assignment_line) {
                card.push_str(&footnote(&note));
            }
            Some(card)
        }
        // Only the name-based fallback can produce more than one, and picking one of them
        // arbitrarily would be presenting a coin flip as an answer.
        many => Some(candidate_list(graph, many)),
    }
}

const GUESS: &str = "Matched on the method name alone — the receiver's type is unknown.";

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
fn provenance(derivation: &types::Derivation, assignment_line: Option<u32>) -> Vec<String> {
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
    if let Some(line) = assignment_line {
        notes.push(format!(
            "Type taken from the assignment on line {line}, which may not be the one that ran."
        ));
    }
    // A convention rather than a fact about this file, so the line it names is in another one.
    // Naming both is the whole of what makes the convention shippable: a reader who thinks Rails
    // renders this template from somewhere else can go and look.
    if let Some(from) = &derivation.controller {
        notes.push(format!(
            "Type taken from `{}`, line {} — the controller Rails renders this template from.",
            from.controller, from.line
        ));
    }
    // The view context, and it is a convention for the same reason the line above is: the template
    // does not say which class renders it, and it does not say that `app/helpers` is in scope
    // either. Both name what a reader would have to go and check.
    if let Some(reached) = &derivation.view {
        notes.push(match reached {
            views::InView::Helper => "Reached through the view context — Rails includes every \
                 `app/helpers` module in every template."
                .to_owned(),
            views::InView::Exported(renderer) => format!(
                "Reached through `helper_method` in `{renderer}` — the class Rails renders this \
                 template from."
            ),
        });
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

fn card(graph: &Graph, synthesized: &Synthesized, declaration_id: DeclarationId) -> Option<String> {
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
        signature(graph, declaration, &definitions)
    );

    if let Some(documentation) = written
        .iter()
        .find_map(|definition| render::documentation(definition.comments()))
    {
        card.push_str("\n\n---\n\n");
        card.push_str(&documentation);
    }

    // Reopened classes and monkey-patched methods are the norm in Ruby, and the one thing a
    // hover cannot show is the code that is somewhere else. **Places**, not definitions: a
    // generated declaration that maps to a line is one, a generated declaration that maps to
    // nothing is not, and an annotation that types a method the user already wrote is the same
    // place twice rather than two of them.
    let places = written.len()
        + generated
            .iter()
            .filter(|definition| locator::site(graph, synthesized, definition).is_some())
            .count();
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

fn signature(graph: &Graph, declaration: &Declaration, definitions: &[&Definition]) -> String {
    let name = declaration.name();
    match declaration {
        Declaration::Namespace(namespace) => match namespace {
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
                Some(other) => format!("{other} "),
            };
            format!("{visibility}{}{parameters}", render::qualified_name(name))
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

fn candidate_list(graph: &Graph, declarations: &[DeclarationId]) -> String {
    let mut names: Vec<String> = declarations
        .iter()
        .filter_map(|id| graph.declarations().get(id))
        .map(|declaration| render::qualified_name(declaration.name()))
        .collect();
    names.sort();
    names.dedup();

    let total = names.len();
    let mut listing = format!("**{total} possible definitions**\n");
    for name in names.iter().take(MAX_CANDIDATES) {
        listing.push_str(&format!("\n- `{name}`"));
    }
    if total > MAX_CANDIDATES {
        listing.push_str(&format!("\n- …and {} more", total - MAX_CANDIDATES));
    }
    // The list is the answer; why it is a list is the footnote, in the place every other card
    // puts one.
    listing.push_str(&footnote(GUESS));
    listing
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_singleton_class_hovers_as_the_source_wrote_it() {
        assert_eq!(attached_name("Person::<Person>"), "Person");
        assert_eq!(attached_name("<Person>"), "Person");
        assert_eq!(attached_name("Person"), "Person");
    }
}
