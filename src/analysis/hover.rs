//! `textDocument/hover` — what the thing under the cursor is, as markdown.

use rubydex::model::{
    declaration::Declaration, definitions::Definition, graph::Graph, ids::DeclarationId,
    visibility::Visibility,
};

use super::{
    locator::{self, Resolution},
    render,
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
/// receiver's type was not known, or the class is reopened somewhere this card cannot show.
/// That is the test for whether a new line belongs in one.
#[must_use]
pub fn markdown(graph: &Graph, resolution: &Resolution) -> Option<String> {
    match resolution.declarations.as_slice() {
        [] => None,
        [only] => {
            let mut card = card(graph, *only)?;
            if !resolution.precise {
                card.push_str(&footnote(GUESS));
            }
            Some(card)
        }
        // Only the name-based fallback can produce more than one, and picking one of them
        // arbitrarily would be presenting a coin flip as an answer.
        many => Some(candidate_list(graph, many)),
    }
}

const GUESS: &str = "Matched on the method name alone — the receiver's type is unknown.";

/// What ya-lsp knows about an answer, as one italic line under it.
///
/// The single place the convention lives, because it was two before: a guessed single match
/// carried it as a trailing italic and a guessed *list* carried the same sentence inline after
/// an em dash, in bold, at the top. Same fact, same uncertainty, two shapes.
fn footnote(note: &str) -> String {
    format!("\n\n*{note}*")
}

fn card(graph: &Graph, declaration_id: DeclarationId) -> Option<String> {
    let declaration = graph.declarations().get(&declaration_id)?;
    let definitions = locator::definitions_of(graph, declaration_id);

    let mut card = format!(
        "```ruby\n{}\n```",
        signature(graph, declaration, &definitions)
    );

    if let Some(documentation) = definitions
        .iter()
        .find_map(|definition| render::documentation(definition.comments()))
    {
        card.push_str("\n\n---\n\n");
        card.push_str(&documentation);
    }

    // Reopened classes and monkey-patched methods are the norm in Ruby, and the one thing a
    // hover cannot show is the code that is somewhere else.
    if definitions.len() > 1 {
        card.push_str(&footnote(&format!(
            "Defined in {} places.",
            definitions.len()
        )));
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
