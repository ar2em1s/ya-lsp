//! `textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes` and `typeHierarchy/subtypes`.
//!
//! # Both directions are the whole chain, not one step
//!
//! Supertypes are `Module#ancestors` minus the class itself: rubydex has already linearized them,
//! in the order Ruby's own method lookup walks. Modules are therefore in it — `Comparable` is a
//! supertype of `String`, and a *prepended* module sits above the class that prepends it — which
//! is correct Ruby and will surprise anyone expecting single inheritance. Filtering it to look
//! familiar would make the answer wrong.
//!
//! Subtypes are the mirror of that rather than one generation of it: a linearized chain one way
//! and a single generation the other would be a tree whose two directions disagree about what a
//! level means. So expanding `Base` lists `Leaf` as well as `Middle`, and expanding `Middle`
//! lists `Leaf` again — the price of a flat list that reads like `ancestors` in either direction.
//!
//! # Subtypes are a lookup, not a scan
//!
//! rubydex maintains the reverse index as it linearizes: every namespace carries the set of
//! declarations that resolved *through* it, kept current incrementally as documents are indexed
//! and dropped. So there is no walk of the graph here and no cap for latency — the cap bounds the
//! *response*, because `Object` has one subtype per class in the project and its bundle.
//!
//! **The set can name a declaration the graph no longer holds.** Deleting a document removes a
//! class from the descendant set of each of its own ancestors, but the ancestors are already
//! cleared for some of them by the time that runs, so a stale id survives — deleting a file that
//! defined `Leaf < Middle < Base` leaves `Leaf` in `Object`'s set and removes it from `Base`'s.
//! Every id here is therefore looked up rather than trusted.
//!
//! # What is not offered a hierarchy
//!
//! Singleton classes and the placeholders rubydex invents for a namespace it never saw, on the
//! same two tests `search::is_listable` uses: `class << self` is not a type anyone asked about,
//! and neither is `<uri>:<offset><anonymous>`. Methods are not either — `prepare` answers `null`
//! on one, which makes the editor say "no results" rather than show a tree of the wrong thing.

use std::collections::HashSet;

use lsp_types::SymbolKind;
use rubydex::model::{
    declaration::{Ancestor, Declaration, Namespace},
    definitions::Definition,
    graph::Graph,
    ids::{ConstantReferenceId, DeclarationId, NameId, UriId},
    name::ParentScope,
};

use super::{
    locator::{self, Site},
    render, symbols,
    synthesized::Synthesized,
};

/// One node of the hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The fully qualified name, as Ruby spells it. rubydex spells a namespace the same way it
    /// is written, so unlike a method there is nothing to render here.
    pub name: String,
    pub kind: SymbolKind,
    /// The file the row is written in, or that the name could not be found. Shown beside the
    /// name, which is where it earns its keep: a ten-deep chain is mostly gems and Ruby's own
    /// signatures, and this is what separates them from the two entries that are the project's.
    pub detail: String,
    /// `None` for an ancestor rubydex could not resolve — there is nothing to expand, and the
    /// row exists so that a superclass in a gem that did not install is *visible* rather than
    /// quietly missing from the chain.
    pub declaration: Option<DeclarationId>,
    pub site: Site,
}

/// What `typeHierarchy/subtypes` found, and how much of it fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtypes {
    pub items: Vec<Item>,
    /// How many subtypes there were before the cap. Above `limit` the answer is truncated, and
    /// a truncated list of subtypes looks exactly like a complete one.
    pub found: usize,
}

/// What the detail column says when the name in the chain resolved to nothing.
const NOT_FOUND: &str = "not found";

/// The type the cursor is on, ready for the client to expand.
///
/// `None` — an empty list — for everything that is not a class or a module, which includes every
/// method: a name-based method match resolves to `Declaration::Method`, so nothing has to gate on
/// [`locator::Resolution::precise`] here. The rejection falls out of asking for a namespace.
#[must_use]
pub fn prepare(
    graph: &Graph,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    own: &HashSet<UriId>,
) -> Vec<Item> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        // As in goto-definition and references: several targets can share the narrowest span, so
        // take the first that has something to say rather than the first that exists.
        .find_map(|located| {
            let items: Vec<Item> = locator::resolve(graph, &located)
                .declarations
                .into_iter()
                .filter_map(|id| item(graph, synthesized, id, own))
                .collect();
            (!items.is_empty()).then_some(items)
        })
        .unwrap_or_default()
}

/// Everything `declaration` inherits from, in the order Ruby's method lookup walks it.
#[must_use]
pub fn supertypes(
    graph: &Graph,
    synthesized: &Synthesized,
    declaration: DeclarationId,
    own: &HashSet<UriId>,
) -> Vec<Item> {
    let Some(subject) = graph.declarations().get(&declaration).and_then(namespace) else {
        return Vec::new();
    };
    let chain: Vec<Ancestor> = subject.ancestors().iter().copied().collect();

    chain
        .iter()
        // A class is in its own ancestors, and not always first: `prepend`ing a module puts that
        // module above the class in the chain, exactly as Ruby reports it. So this drops the one
        // entry that is this declaration rather than the head of the list.
        .filter(|ancestor| **ancestor != Ancestor::Complete(declaration))
        .filter_map(|ancestor| match ancestor {
            Ancestor::Complete(ancestor) => item(graph, synthesized, *ancestor, own),
            Ancestor::Partial(name) => unresolved(graph, &chain, *name),
        })
        .collect()
}

/// Everything that inherits from `declaration`, the user's own code first.
///
/// Ranked before it is capped, because the reverse index is a hash set: taking the first `limit`
/// of it in the order it iterates would hand back a different arbitrary slice on every run, and
/// at the top of the object model the slice is what the user sees.
#[must_use]
pub fn subtypes(
    graph: &Graph,
    synthesized: &Synthesized,
    declaration: DeclarationId,
    limit: usize,
    own: &HashSet<UriId>,
) -> Subtypes {
    let Some(subject) = graph.declarations().get(&declaration).and_then(namespace) else {
        return Subtypes {
            items: Vec::new(),
            found: 0,
        };
    };

    // Ranked on what is cheap to read — a flag and the name rubydex already holds — and turned
    // into items only for the survivors, the same split `search::search` makes and for the same
    // reason: a class near the root of the object model has tens of thousands of descendants and
    // finding the file each is written in means reaching into every definition of every one.
    let mut ranked: Vec<(bool, &str, DeclarationId)> = subject
        .descendants()
        .iter()
        // A class is in its own descendants too, for the same reason it is in its own ancestors.
        .filter(|descendant| **descendant != declaration)
        .filter_map(|descendant| {
            let subtype = graph.declarations().get(descendant)?;
            namespace(subtype)?;
            Some((
                locator::declared_in(graph, subtype, own),
                subtype.name(),
                *descendant,
            ))
        })
        .collect();
    let found = ranked.len();

    // The project ahead of its bundle, for `search::rank`'s reason: asking `StandardError` for
    // its subtypes in a Rails app finds hundreds in gems and a handful that are the user's, and
    // an alphabetical list would bury the handful. Never by `DeclarationId`, which is a hash and
    // would shuffle between runs.
    ranked.sort_unstable_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(right.1)));
    ranked.truncate(limit);

    Subtypes {
        items: ranked
            .into_iter()
            .filter_map(|(_, _, id)| item(graph, synthesized, id, own))
            .collect(),
        found,
    }
}

/// The namespace a declaration is, when it is one a person asked about.
///
/// Both exclusions are `search::is_listable`'s, and they are two tests rather than one because
/// they catch different things: `SingletonClass` is how rubydex spells `class << self`, and the
/// name test also turns away an anonymous `Class.new`, which is a real class with no name to put
/// in a tree. `Todo` is the placeholder for a namespace rubydex never saw a definition of.
fn namespace(declaration: &Declaration) -> Option<&Namespace> {
    let namespace = declaration.as_namespace()?;
    (!matches!(namespace, Namespace::SingletonClass(_) | Namespace::Todo(_))
        && render::is_nameable(declaration.name()))
    .then_some(namespace)
}

/// One row for a declaration the graph holds.
///
/// `None` when there is nowhere to point: a declaration with no definitions behind it, or one
/// whose only definition is in rubydex's synthetic built-in document. The second is why `Object`,
/// `Kernel` and `BasicObject` are absent from a chain when signatures are turned off and present
/// when they are on — `DocUri::from_uri_str` rejects `rubydex:built-in` for every request alike,
/// and a row an editor cannot open is worse than a row that is not there.
fn item(
    graph: &Graph,
    synthesized: &Synthesized,
    id: DeclarationId,
    own: &HashSet<UriId>,
) -> Option<Item> {
    let declaration = graph.declarations().get(&id)?;
    namespace(declaration)?;
    let definition = locator::preferred_definition(graph, id, own)?;
    let site = locator::site(graph, synthesized, definition)?;
    Some(Item {
        name: declaration.name().to_owned(),
        kind: symbols::kind_of(definition),
        detail: file_name(&site.uri),
        declaration: Some(id),
        site,
    })
}

/// One row for a name in the chain that resolved to nothing.
///
/// Shown rather than dropped, which is the whole point of the arm: a superclass that lives in a
/// gem the bundle did not install would otherwise leave a chain that reads as complete and is
/// short by everything above the gap.
///
/// It is placed where the name is *written*, which is not necessarily in the class being
/// expanded — an unresolved superclass propagates down, so `Leaf`'s chain carries the
/// `Missing::Thing` that `Middle` inherits from. So the whole chain is searched for the mention,
/// and the kind comes from how it was written: a superclass is a class and a mixin is a module.
fn unresolved(graph: &Graph, chain: &[Ancestor], name: NameId) -> Option<Item> {
    let (kind, site) = mention(graph, chain, name)?;
    Some(Item {
        name: spell(graph, name)?,
        kind,
        detail: NOT_FOUND.to_owned(),
        declaration: None,
        site,
    })
}

/// Where `name` is written as a superclass or a mixin somewhere in `chain`, and which of the two.
///
/// Every partial in a linearized chain was contributed by one of the chain's own members — a name
/// that did not resolve is a name nothing beyond it is reachable through — so this looks no
/// further than the members themselves.
fn mention(graph: &Graph, chain: &[Ancestor], name: NameId) -> Option<(SymbolKind, Site)> {
    chain
        .iter()
        .filter_map(|ancestor| match ancestor {
            Ancestor::Complete(id) => graph.declarations().get(id),
            Ancestor::Partial(_) => None,
        })
        .flat_map(Declaration::definitions)
        .filter_map(|id| graph.definitions().get(id))
        .find_map(|definition| {
            let (superclass, mixins) = match definition {
                Definition::Class(class) => (class.superclass_ref().copied(), class.mixins()),
                Definition::Module(module) => (None, module.mixins()),
                // Not reachable today and required by the enum's other fourteen kinds, which are
                // members rather than namespace bodies: everything in a linearized chain is a
                // class or a module, and rubydex promotes even `Wrapper = Class.new` to a class
                // before it can be anybody's ancestor. It is the file's one uncovered line.
                _ => return None,
            };
            // The superclass first and separately, because it is the one whose kind is a class.
            // A `Foo` written as both — `class A < Foo` in one file and `include Foo` in another
            // — cannot happen for one name that resolved to nothing, since the two spell
            // different `Name`s only when their nesting differs, and then they are two rows.
            superclass
                .filter(|reference| name_of(graph, *reference) == Some(name))
                .map(|reference| (SymbolKind::CLASS, reference))
                .or_else(|| {
                    mixins
                        .iter()
                        .map(|mixin| *mixin.constant_reference_id())
                        .find(|reference| name_of(graph, *reference) == Some(name))
                        .map(|reference| (SymbolKind::MODULE, reference))
                })
                .and_then(|(kind, reference)| {
                    let reference = graph.constant_references().get(&reference)?;
                    let document = graph.documents().get(definition.uri_id())?;
                    let span = (reference.offset().start(), reference.offset().end());
                    Some((
                        kind,
                        Site {
                            uri: document.uri().to_owned(),
                            // The name as written is the whole of what this row is, so the two
                            // spans are one span — which satisfies the protocol's rule that the
                            // selection sit inside the range without having to be reconciled.
                            full: span,
                            selection: span,
                        },
                    ))
                })
        })
}

/// The name a constant reference names.
fn name_of(graph: &Graph, reference: ConstantReferenceId) -> Option<NameId> {
    graph
        .constant_references()
        .get(&reference)
        .map(|reference| *reference.name_id())
}

/// A name as it was written: `Missing::Thing`, `::Missing::Thing`.
///
/// Built by walking the parent scopes rather than recursing through them, since a constant path
/// is as deep as the file that wrote it. The explicit root is kept because `::Foo` failing to
/// resolve where `Foo` would have resolved is often the reason, and this row exists to be read.
fn spell(graph: &Graph, name: NameId) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    let mut current = graph.names().get(&name)?;
    loop {
        segments.push(graph.strings().get(current.str())?.as_str());
        match current.parent_scope() {
            ParentScope::Some(parent) | ParentScope::Attached(parent) => {
                current = graph.names().get(parent)?;
            }
            ParentScope::TopLevel => {
                segments.push("");
                break;
            }
            ParentScope::None => break,
        }
    }
    segments.reverse();
    Some(segments.join("::"))
}

/// The last segment of a document URI, which is the file name.
///
/// Taken off the URI rather than off a path: this runs once per row of a list that is at most a
/// few hundred long, and going through `DocUri` would mean a `Url` parse per row for a string
/// that is only ever read. Percent escapes are left as they are — the column is a hint next to a
/// name, not somewhere anybody clicks.
fn file_name(uri: &str) -> String {
    uri.rsplit('/').next().unwrap_or(uri).to_owned()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_the_graph_does_not_hold_has_no_hierarchy_either_way() {
        // Not a hypothetical, and not the same case `locator` tests: the id arrives from the
        // *client*, which echoes back whatever the item it is expanding carried. A rubydex id is
        // a 64-bit hash of the name, so it survives a config reload — but the declaration it
        // names does not have to, and the descendant sets keep ids of deleted documents besides.
        let graph = Graph::new();
        let nowhere = DeclarationId::new(1_234_567_890_123_456_789);
        let own = HashSet::new();

        let synthesized = Synthesized::new();

        assert!(supertypes(&graph, &synthesized, nowhere, &own).is_empty());
        assert_eq!(
            subtypes(&graph, &synthesized, nowhere, 10, &own),
            Subtypes {
                items: Vec::new(),
                found: 0
            }
        );
    }

    #[test]
    fn the_namespaces_a_person_never_wrote_are_not_types() {
        // Two tests rather than one, because they catch different things, and asserted here
        // rather than only through a cursor position: which declarations are *offered* a
        // hierarchy is a rule, and reaching a rule by accident through whatever `locate` happens
        // to return at a `~` is how one stops being tested without anybody noticing.
        use rubydex::model::declaration::{
            ClassDeclaration, MethodDeclaration, ModuleDeclaration, SingletonClassDeclaration,
            TodoDeclaration,
        };

        let class = Declaration::Namespace(Namespace::Class(Box::new(ClassDeclaration::new(
            "Person".to_owned(),
            DeclarationId::from("Object"),
        ))));
        let module = Declaration::Namespace(Namespace::Module(Box::new(ModuleDeclaration::new(
            "Greet".to_owned(),
            DeclarationId::from("Object"),
        ))));
        assert!(namespace(&class).is_some());
        assert!(namespace(&module).is_some());

        // `class << self`, which rubydex files as a namespace with a real ancestor chain and
        // which nobody spelled that way.
        let singleton = Declaration::Namespace(Namespace::SingletonClass(Box::new(
            SingletonClassDeclaration::new(
                "Person::<Person>".to_owned(),
                DeclarationId::from("Person"),
            ),
        )));
        assert!(namespace(&singleton).is_none());

        // The placeholder for a namespace rubydex never saw — `Foo::Bar` mentioned where there
        // is no `Foo` — and `Class.new` with nothing to call it, which is a real class with no
        // name to put in a tree. The second is why the name is tested as well as the kind.
        let todo = Declaration::Namespace(Namespace::Todo(Box::new(TodoDeclaration::new(
            "Foo".to_owned(),
            DeclarationId::from("Object"),
        ))));
        let anonymous = Declaration::Namespace(Namespace::Class(Box::new(ClassDeclaration::new(
            "12345:678<anonymous>".to_owned(),
            DeclarationId::from("Object"),
        ))));
        assert!(namespace(&todo).is_none());
        assert!(namespace(&anonymous).is_none());

        // And a method, which is what a cursor lands on far more often than any of the above.
        let method = Declaration::Method(Box::new(MethodDeclaration::new(
            "Person#shout()".to_owned(),
            DeclarationId::from("Person"),
        )));
        assert!(namespace(&method).is_none());
    }

    #[test]
    fn a_name_the_graph_does_not_hold_is_spelled_as_nothing() {
        // `spell` walks interned strings the resolver filed, so every step of it can miss. The
        // row it feeds is the one that says a superclass could not be found, and a row with no
        // name in it says less than no row at all.
        let graph = Graph::new();
        assert_eq!(spell(&graph, NameId::new(987_654_321)), None);
    }

    #[test]
    fn the_detail_column_is_the_file_name_however_the_uri_is_spelled() {
        // A file name is the one thing that separates the two rows of a ten-deep chain that are
        // the project's from the eight that are not, so it is worth it being right for the
        // shapes rubydex's document keys actually take.
        assert_eq!(file_name("file:///w/app/models/user.rb"), "user.rb");
        assert_eq!(file_name("file:///user.rb"), "user.rb");
        // rubydex's synthetic document, which never reaches a client — `DocUri` rejects it — but
        // which reaches here first, and must not read as an empty column.
        assert_eq!(file_name("rubydex:built-in"), "rubydex:built-in");
    }
}
