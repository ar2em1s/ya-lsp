//! `textDocument/documentSymbol` — the outline of one file.

use std::collections::HashMap;

use lsp_types::{DocumentSymbol, Location, SymbolInformation, SymbolKind, SymbolTag};
use rubydex::model::{
    definitions::{Definition, Receiver},
    graph::Graph,
    ids::{DefinitionId, UriId},
};

use super::{position::TextDocument, render};

/// The outline of `uri_id`, nested the way the file is nested.
///
/// rubydex hands back a *flat* list of every definition in the document, each carrying the
/// `DefinitionId` of the construct it is lexically inside. Rebuilding the tree from those
/// links is what turns it into an outline.
#[must_use]
pub fn document_symbols(graph: &Graph, uri_id: UriId, text: &TextDocument) -> Vec<DocumentSymbol> {
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };

    let mut listed: Vec<(DefinitionId, &Definition)> = document
        .definitions()
        .iter()
        .filter_map(|id| Some((*id, graph.definitions().get(id)?)))
        .filter(|(_, definition)| is_outline_worthy(definition))
        .collect();

    // rubydex emits definitions in source order already, but nothing in its API promises that,
    // and the tree assembly below depends on a parent always preceding its children.
    listed.sort_by_key(|(_, definition)| (definition.offset().start(), definition.offset().end()));

    let position: HashMap<DefinitionId, usize> = listed
        .iter()
        .enumerate()
        .map(|(index, (id, _))| (*id, index))
        .collect();

    let mut nodes: Vec<Option<DocumentSymbol>> = listed
        .iter()
        .map(|(_, definition)| Some(symbol(graph, definition, text)))
        .collect();

    // Back to front, so a node is complete — children and all — before it is moved into its
    // own parent.
    let mut roots = Vec::new();
    for index in (0..nodes.len()).rev() {
        let Some(node) = nodes[index].take() else {
            continue;
        };
        match parent_of(graph, listed[index].1, &position) {
            Some(parent) if parent < index => nodes[parent]
                .as_mut()
                .expect("a parent precedes its children and is taken last")
                .children
                .get_or_insert_with(Vec::new)
                .insert(0, node),
            _ => roots.push(node),
        }
    }
    roots.reverse();
    roots
}

/// The same outline for a client that never learned about nesting.
///
/// `hierarchicalDocumentSymbolSupport` arrived in LSP 3.10; a client without it expects a flat
/// list of `SymbolInformation` and will not render — or may not even parse — the nested form.
#[must_use]
pub fn flatten(symbols: &[DocumentSymbol], uri: &lsp_types::Uri) -> Vec<SymbolInformation> {
    let mut flat = Vec::new();
    push_flat(symbols, uri, None, &mut flat);
    flat
}

fn push_flat(
    symbols: &[DocumentSymbol],
    uri: &lsp_types::Uri,
    container: Option<&str>,
    out: &mut Vec<SymbolInformation>,
) {
    for symbol in symbols {
        #[allow(deprecated)] // `deprecated` is a required field; `tags` is what we actually set.
        out.push(SymbolInformation {
            name: symbol.name.clone(),
            kind: symbol.kind,
            tags: symbol.tags.clone(),
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range: symbol.range,
            },
            container_name: container.map(str::to_owned),
        });
        if let Some(children) = &symbol.children {
            push_flat(children, uri, Some(&symbol.name), out);
        }
    }
}

/// The nearest enclosing construct that is itself in the outline.
///
/// Walking up rather than giving up matters for `class << self`: were singleton classes ever
/// filtered out, their methods would still need to land under the class.
fn parent_of(
    graph: &Graph,
    definition: &Definition,
    position: &HashMap<DefinitionId, usize>,
) -> Option<usize> {
    let mut nesting = *definition.lexical_nesting_id();
    // Bounded rather than `while let`: nothing in rubydex's API promises the nesting chain is
    // acyclic, and a cycle here would hang the analysis thread, which is indistinguishable
    // from a dead editor. Real Ruby does not nest anywhere near this deep.
    for _ in 0..MAX_NESTING {
        let id = nesting?;
        if let Some(index) = position.get(&id) {
            return Some(*index);
        }
        nesting = *graph.definitions().get(&id)?.lexical_nesting_id();
    }
    tracing::warn!("giving up on a lexical nesting chain deeper than {MAX_NESTING}");
    None
}

/// How far up a lexical nesting chain to walk before assuming it is broken.
const MAX_NESTING: usize = 64;

fn symbol(graph: &Graph, definition: &Definition, text: &TextDocument) -> DocumentSymbol {
    let full = definition.offset();
    let selection = definition.name_offset().unwrap_or(full);

    #[allow(deprecated)] // Same: required field, superseded by `tags`.
    DocumentSymbol {
        name: name_of(graph, definition),
        detail: detail_of(graph, definition),
        kind: kind_of(definition),
        tags: definition
            .is_deprecated()
            .then(|| vec![SymbolTag::DEPRECATED]),
        deprecated: None,
        range: text.range_at(full.start(), full.end()),
        selection_range: text.range_at(selection.start(), selection.end()),
        children: None,
    }
}

fn name_of(graph: &Graph, definition: &Definition) -> String {
    let raw = graph
        .strings()
        .get(&graph.definition_string_id(definition))
        .map_or_else(String::new, |string| string.as_str().to_owned());
    let name = render::simple_name(&raw);

    match definition {
        // `class << self` has no name of its own; rubydex calls it `<Person>`.
        Definition::SingletonClass(_) => format!("<< {}", name.trim_matches(['<', '>'])),
        Definition::Method(method) => match method.receiver() {
            Some(Receiver::SelfReceiver(_)) => format!("self.{name}"),
            Some(Receiver::ConstantReceiver(name_id)) => match constant_name(graph, *name_id) {
                Some(receiver) => format!("{receiver}.{name}"),
                None => name.to_owned(),
            },
            None => name.to_owned(),
        },
        _ => name.to_owned(),
    }
}

fn constant_name(graph: &Graph, name_id: rubydex::model::ids::NameId) -> Option<String> {
    let name = graph.names().get(&name_id)?;
    Some(graph.strings().get(name.str())?.as_str().to_owned())
}

fn detail_of(graph: &Graph, definition: &Definition) -> Option<String> {
    let detail = match definition {
        Definition::Method(method) => {
            let parameters = render::parameter_list(graph, method.signatures());
            match method.visibility() {
                rubydex::model::visibility::Visibility::Public => parameters,
                other => format!("{other} {parameters}").trim_end().to_owned(),
            }
        }
        Definition::AttrReader(_) => "attr_reader".to_owned(),
        Definition::AttrWriter(_) => "attr_writer".to_owned(),
        Definition::AttrAccessor(_) => "attr_accessor".to_owned(),
        Definition::MethodAlias(_) => "alias".to_owned(),
        Definition::ConstantAlias(_) => "alias".to_owned(),
        _ => String::new(),
    };
    (!detail.is_empty()).then_some(detail)
}

/// Shared with `search`, so a symbol has the same icon wherever it is listed.
pub(super) fn kind_of(definition: &Definition) -> SymbolKind {
    match definition {
        Definition::Class(_) | Definition::SingletonClass(_) => SymbolKind::CLASS,
        Definition::Module(_) => SymbolKind::MODULE,
        Definition::Constant(_) | Definition::ConstantAlias(_) => SymbolKind::CONSTANT,
        Definition::AttrAccessor(_) | Definition::AttrReader(_) | Definition::AttrWriter(_) => {
            SymbolKind::PROPERTY
        }
        _ => SymbolKind::METHOD,
    }
}

/// What belongs in an outline.
///
/// The exclusions are all things rubydex records for resolution rather than for reading:
/// `private :foo` is a visibility *statement*, not a definition of `foo`, and instance and
/// class variables would list `@name` once per assignment.
fn is_outline_worthy(definition: &Definition) -> bool {
    !matches!(
        definition,
        Definition::ConstantVisibility(_)
            | Definition::MethodVisibility(_)
            | Definition::GlobalVariable(_)
            | Definition::GlobalVariableAlias(_)
            | Definition::InstanceVariable(_)
            | Definition::ClassVariable(_)
    )
}
