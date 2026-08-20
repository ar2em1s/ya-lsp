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
#[must_use]
pub fn markdown(graph: &Graph, resolution: &Resolution) -> Option<String> {
    match resolution.declarations.as_slice() {
        [] => None,
        [only] => {
            let card = card(graph, *only)?;
            if resolution.precise {
                Some(card)
            } else {
                Some(format!("{card}\n\n*{GUESS}*"))
            }
        }
        // Only the name-based fallback can produce more than one, and picking one of them
        // arbitrarily would be presenting a coin flip as an answer.
        many => Some(candidate_list(graph, many)),
    }
}

const GUESS: &str = "Matched on the method name alone — the receiver's type is unknown.";

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
        card.push_str(&format!("\n\n*Defined in {} places.*", definitions.len()));
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

/// `Person::<Person>` -> `Person`, `<Person>` -> `Person`.
fn attached_name(name: &str) -> &str {
    name.rsplit_once("::<")
        .map_or(name, |(_, singleton)| singleton)
        .trim_end_matches('>')
        .trim_start_matches('<')
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
    let mut listing = format!("**{total} possible definitions** — {GUESS}\n");
    for name in names.iter().take(MAX_CANDIDATES) {
        listing.push_str(&format!("\n- `{name}`"));
    }
    if total > MAX_CANDIDATES {
        listing.push_str(&format!("\n- …and {} more", total - MAX_CANDIDATES));
    }
    listing
}

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
