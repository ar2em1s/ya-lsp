//! `textDocument/documentSymbol` — the outline of one file.

use std::collections::HashMap;

use lsp_types::{DocumentSymbol, Location, SymbolInformation, SymbolKind, SymbolTag};
use rubydex::model::{
    definitions::{Definition, Receiver},
    graph::Graph,
    ids::{DefinitionId, UriId},
};

use super::{locator, position::TextDocument, render};

/// The outline of `uri_id`, nested the way the file is nested.
///
/// rubydex hands back a *flat* list of every definition in the document, each carrying the
/// `DefinitionId` of the construct it is lexically inside. Rebuilding the tree from those
/// links is what turns it into an outline.
#[must_use]
pub fn document_symbols(
    graph: &Graph,
    modifiers: &locator::Modifiers<'_>,
    uri_id: UriId,
    text: &TextDocument,
) -> Vec<DocumentSymbol> {
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };

    let mut listed: Vec<(DefinitionId, &Definition)> = document
        .definitions()
        .iter()
        .filter_map(|id| Some((*id, graph.definitions().get(id)?)))
        .filter(|(_, definition)| is_outline_worthy(graph, definition))
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
        .map(|(_, definition)| Some(symbol(graph, modifiers, definition, text)))
        .collect();

    // Back to front, so a node is complete — children and all — before it is moved into its
    // own parent.
    let mut roots = Vec::new();
    for index in (0..nodes.len()).rev() {
        // Each index is visited once and only a *parent* is ever borrowed, never taken, so this
        // is `Some` for the same reason the arm below is — stated the same way, rather than as
        // a silent skip that would drop a symbol if it ever stopped being true.
        let node = nodes[index]
            .take()
            .expect("each index is taken exactly once");
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

fn symbol(
    graph: &Graph,
    modifiers: &locator::Modifiers<'_>,
    definition: &Definition,
    text: &TextDocument,
) -> DocumentSymbol {
    // `selectionRange` has to sit inside `range` or VS Code throws away the whole outline;
    // `locator::spans` is the one place that guarantees it.
    let (full, selection) = locator::spans(definition);

    #[allow(deprecated)] // Same: required field, superseded by `tags`.
    DocumentSymbol {
        name: name_of(graph, definition),
        detail: detail_of(graph, modifiers, definition),
        kind: kind_of(definition),
        tags: definition
            .is_deprecated()
            .then(|| vec![SymbolTag::DEPRECATED]),
        deprecated: None,
        range: text.range_at(full.0, full.1),
        selection_range: text.range_at(selection.0, selection.1),
        children: None,
    }
}

fn name_of(graph: &Graph, definition: &Definition) -> String {
    let raw = graph
        .strings()
        .get(&graph.definition_string_id(definition))
        .map_or_else(String::new, |string| string.as_str().to_owned());
    // Through `qualified_name` for the anonymous `Class.new`, which rubydex keys by number: a
    // row in the outline reads the same as the hover card over the line it points at, and both
    // read as the call the source wrote. It leaves every other name exactly as it found it.
    let raw = render::qualified_name(graph, &raw);
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

fn detail_of(
    graph: &Graph,
    modifiers: &locator::Modifiers<'_>,
    definition: &Definition,
) -> Option<String> {
    let detail = match definition {
        Definition::Method(method) => {
            let parameters = render::parameter_list(graph, method.signatures());
            match method.visibility() {
                rubydex::model::visibility::Visibility::Public => parameters,
                // **An outline row is one `def`, so the reread is asked of that `def` and not of
                // the name.** rubydex records a bare `private` written inside a block against
                // every `def` below the block, and the word would otherwise be printed beside a
                // public method in the tree a reader navigates by. See `locator::Modifiers`.
                rubydex::model::visibility::Visibility::Private
                | rubydex::model::visibility::Visibility::ModuleFunction
                    if modifiers.escaped(graph, definition) =>
                {
                    parameters
                }
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
/// The exclusions by kind are all things rubydex records for resolution rather than for reading:
/// `private :foo` is a visibility *statement*, not a definition of `foo`, and instance and class
/// variables would list `@name` once per assignment.
///
/// The name is checked because Prism recovers a half-typed `def` into a node whose name span is
/// the whitespace after the keyword, and a row with nothing written in it is not an outline
/// entry. Dropping one is safe for the tree: `parent_of` walks past a definition it cannot find,
/// so anything nested inside reparents outwards rather than disappearing.
fn is_outline_worthy(graph: &Graph, definition: &Definition) -> bool {
    !matches!(
        definition,
        Definition::ConstantVisibility(_)
            | Definition::MethodVisibility(_)
            | Definition::GlobalVariable(_)
            | Definition::GlobalVariableAlias(_)
            | Definition::InstanceVariable(_)
            | Definition::ClassVariable(_)
    ) && !name_of(graph, definition).trim().is_empty()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::testing::*;

    #[test]
    fn the_outline_nests_the_way_the_file_nests() {
        let (mut harness, uri) = library();
        let outline = harness.outline(&uri);

        let names: Vec<&str> = outline
            .as_array()
            .expect("nested symbols")
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["Person", "Person"],
            "reopening is two top-level symbols"
        );

        let members: Vec<&str> = outline[0]["children"]
            .as_array()
            .expect("children")
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        // `self.build` keeps its receiver, and `private` is a statement rather than a symbol.
        assert_eq!(
            members,
            vec!["MAX_AGE", "name", "self.build", "shout", "secret"]
        );

        let shout = &outline[0]["children"][3];
        assert_eq!(shout["kind"], 6, "SymbolKind::METHOD");
        assert_eq!(shout["detail"], "(volume = ..., *rest, sep:, &block)");
        assert_eq!(outline[0]["children"][4]["detail"], "private");
        assert_eq!(outline[0]["children"][1]["detail"], "attr_reader");

        // The selection range has to sit inside the range, or clients reject the symbol.
        assert_eq!(shout["selectionRange"]["start"]["line"], 14);
        assert_eq!(shout["range"]["start"]["line"], 14);
        assert_eq!(shout["range"]["end"]["line"], 16);
    }

    #[test]
    fn a_public_method_below_a_block_holding_private_is_not_labelled_private() {
        // rubydex records a bare `private` written inside a block against every `def` below the
        // *block*, so the outline printed `private` beside a public method — the tree a reader
        // navigates by, stating the one thing about the method that is not true. The same
        // reread the jump and the list use decides the word. See `locator::Modifiers`.
        let mut harness = Harness::new();
        let uri = harness.write(
            "app/models/concerns/has_custom_fields.rb",
            "\
module HasCustomFields
  class_methods do
    private

    def custom_field_meta_data
      @custom_field_meta_data
    end
  end

  def upsert_custom_fields(fields)
    fields
  end
end
",
        );
        harness.index();

        let outline = harness.outline(&uri);
        let members = &outline[0]["children"];
        assert_eq!(members[0]["name"], "custom_field_meta_data");
        assert_eq!(
            members[0]["detail"], "private",
            "inside the block and under its `private`: {members}"
        );
        assert_eq!(members[1]["name"], "upsert_custom_fields");
        assert_eq!(
            members[1]["detail"], "(fields)",
            "below the block, so the `private` inside it never reached this one: {members}"
        );
    }

    /// Every construct the outline spells differently from its bare name.
    ///
    /// `class << self` has no name of its own, a `Class.new` has none either, a method can
    /// carry a receiver, and six kinds have a `detail` that is a keyword rather than a
    /// parameter list. Each was reachable only through a fixture nothing had written.
    const SHAPES: &str = "\
module Outer
  class Widget
    attr_writer :width
    attr_accessor :height
    attr_reader :depth

    LIMIT = 10
    CAP = LIMIT

    class << self
      def registry
      end
    end

    def self.build
    end

    def resize
    end
    alias grow resize
    alias_method :enlarge, :resize
  end
end

def Outer.configure
end

builder = Class.new do
  def make
  end
end

traits = Module.new do
  def trait
  end
end
";

    #[test]
    fn the_outline_spells_each_construct_the_way_a_reader_would() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/shapes.rb", SHAPES);
        harness.index();
        harness.open(&uri, SHAPES);
        let outline = harness.outline(&uri);

        /// `name | kind | detail` for every symbol, depth first, indented by nesting.
        fn rows(symbols: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
            for symbol in symbols.as_array().into_iter().flatten() {
                out.push(format!(
                    "{:indent$}{} | {} | {}",
                    "",
                    symbol["name"].as_str().unwrap_or("?"),
                    symbol["kind"],
                    symbol["detail"].as_str().unwrap_or("-"),
                    indent = depth * 2,
                ));
                rows(&symbol["children"], depth + 1, out);
            }
        }

        let mut listed = Vec::new();
        rows(&outline, 0, &mut listed);
        assert_eq!(
            listed,
            vec![
                "Outer | 2 | -",
                "  Widget | 5 | -",
                // The three `attr_*` kinds are PROPERTY, and their detail is the keyword: there
                // is no parameter list to show and "width" alone says nothing.
                "    width | 7 | attr_writer",
                "    height | 7 | attr_accessor",
                "    depth | 7 | attr_reader",
                "    LIMIT | 14 | -",
                // `CAP = LIMIT` is a constant *alias*, not a second constant.
                "    CAP | 14 | alias",
                // `class << self` has no name of its own; rubydex calls it `<Widget>`.
                "    << Widget | 5 | -",
                "      registry | 6 | -",
                "    self.build | 6 | -",
                "    resize | 6 | -",
                "    grow | 6 | alias",
                "    enlarge | 6 | alias",
                // A method written on a constant receiver keeps it, which is the only thing
                // telling `def Outer.configure` apart from a top-level `def configure`.
                "Outer.configure | 6 | -",
                // A `Class.new` nothing bound to a constant has no name of its own either;
                // rubydex keys it by document and offset, and the outline was printing that
                // key. The kind is what tells the two apart at a glance, and the spelling has
                // to agree with it — rubydex writes both the same, so the declaration decides.
                "Class.new | 5 | -",
                "  make | 6 | -",
                "Module.new | 2 | -",
                "  trait | 6 | -",
            ],
        );
    }

    #[test]
    fn the_outline_lists_definitions_and_not_the_statements_around_them() {
        // Each of these is something rubydex records as a definition for resolution's sake and
        // nobody would want in a file's structure: `private :shout` is a visibility statement,
        // not a second declaration of `shout`, and a variable would appear once per assignment.
        let mut harness = Harness::new();
        let source = "\
$LOG = nil
alias $log $LOG

class Widget
  @@count = 0
  @name = nil

  def shout
    @volume = 1
  end
  private :shout

  SECRET = 1
  private_constant :SECRET
end
";
        let uri = harness.write("lib/widget.rb", source);
        harness.index();
        harness.open(&uri, source);

        let listed: Vec<String> = all_symbols(&harness.outline(&uri))
            .into_iter()
            .map(|symbol| symbol["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        for statement in ["$LOG", "$log", "@@count", "@name", "@volume"] {
            assert!(
                !listed.contains(&statement.to_owned()),
                "{statement} is not an outline entry: {listed:?}"
            );
        }
        // Not vacuous: the definitions those statements are about are all still there.
        for wanted in ["Widget", "shout", "SECRET"] {
            assert!(listed.contains(&wanted.to_owned()), "{listed:?}");
        }
    }

    #[test]
    fn an_outline_names_a_singleton_method_on_a_constant_it_cannot_resolve() {
        // `def Nowhere.thing` parses and is indexed; the receiver names a constant the graph
        // never saw. The outline still has to carry a row for it, and the honest name is the
        // bare method — inventing `Nowhere.thing` from an unresolved reference would put a
        // name in the picker that leads nowhere.
        let mut harness = Harness::new();
        let source = "def Nowhere.thing\nend\n";
        let uri = harness.write("app/patch.rb", source);
        harness.index();
        harness.open(&uri, source);

        let outline = harness.ask(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );
        let names: Vec<&str> = all_symbols(&outline)
            .into_iter()
            .filter_map(|symbol| symbol["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec!["Nowhere.thing"],
            "the receiver is named from the reference, declared or not: {outline}"
        );
    }

    #[test]
    fn a_half_typed_def_does_not_take_the_outline_down_with_it() {
        // Reported from a real editor: typing `def` inside a class made VS Code throw
        // `selectionRange must be contained in fullRange` and drop the *entire* outline, so the
        // file's structure vanished mid-keystroke. Prism recovers a bare `def` into a node whose
        // location is the three keyword bytes and whose name location is the whitespace *after*
        // them, so `range` ended where `selectionRange` began.
        //
        // Half-typed code is the normal state of a buffer, not an edge case, so the containment
        // rule has to hold for whatever the parser recovered.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();
        harness.open(&uri, "");

        for source in [
            "class A\n def\n",
            "class A\n def \n",
            "def \n",
            "class A\n  private def \n",
            "module M\n  class B\n    def\n",
        ] {
            harness.change(&uri, source);
            let outline = harness.outline(&uri);

            assert!(
                uncontained(&outline).is_empty(),
                "{source:?}: {:?}\n{outline}",
                uncontained(&outline)
            );
            // And nothing with no name in it: the recovered node is not a symbol yet, and a
            // blank row in the outline is the visible half of the same bug.
            for symbol in all_symbols(&outline) {
                let name = symbol["name"].as_str().unwrap_or_default();
                assert!(
                    !name.trim().is_empty(),
                    "{source:?}: blank symbol\n{outline}"
                );
            }
        }

        // Not vacuous by way of an empty answer: the enclosing class is still outlined while
        // the method inside it is being typed.
        harness.change(&uri, "class A\n def\n");
        assert_eq!(harness.outline(&uri)[0]["name"], "A");
    }
}
