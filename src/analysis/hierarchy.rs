//! The two hierarchies: `prepareTypeHierarchy` with `supertypes` and `subtypes`, and
//! `prepareCallHierarchy` with `incomingCalls` and `outgoingCalls`.
//!
//! They share [`Item`] and nothing else. A type hierarchy is a fact about declarations that
//! rubydex has already computed; a call hierarchy is two different questions about *call sites*,
//! and the section at the end of this comment is about how differently they can be answered.
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
//! [`prepare_calls`] is its mirror and turns everything that is *not* a method away.
//!
//! # The two directions of the call hierarchy are not each other's mirror
//!
//! **Incoming is a work list.** `references::calls_to` matches a name, because rubydex links no
//! method reference to a declaration — so a caller of `call` is a caller of something spelled
//! `call`, exactly as `textDocument/references` is, and the row says so in its detail column. A
//! tree implies an exactness a name match does not have, and the footnote is what keeps the two
//! apart.
//!
//! **Outgoing is a claim that an edge exists**, which is a stronger thing to say than "this name
//! appears here", so it is drawn only from a resolution that named the receiver —
//! `locator::precise_call`, the same gate `signatureHelp` uses. A call on a receiver nothing
//! typed would otherwise fan out into every method spelled that way, and forty edges out of one
//! call site is not a hierarchy. The asymmetry is deliberate: a work list may be over-broad and
//! still be useful, an edge may not.
//!
//! **The bucket is a definition, never a declaration.** A class reopened in two files has two
//! bodies, and a row's `fromRanges` are drawn against the row's own file — so bucketing callers
//! by declaration would put one file's ranges on another file's text. It is also the reason both
//! directions ask the same question: which `def`'s body contains this offset, innermost first.
//! Incoming asks it of a call site to find its caller; outgoing asks it to *exclude* the calls
//! that belong to a nested `def` rather than to the method being expanded.
//!
//! **A call inside no `def` at all is attributed to the file.** A model's `has_many`, a
//! `Rakefile`'s top level, a class body's `include` — dropping those hides most of what a Rails
//! model is made of, and attributing them to the enclosing class would say a class called
//! something, which is not what an edge in a call graph means.

use std::collections::{HashMap, HashSet};

use lsp_types::SymbolKind;
use rubydex::model::{
    declaration::{Ancestor, Declaration, Namespace},
    definitions::Definition,
    document::Document,
    graph::Graph,
    ids::{ConstantReferenceId, DeclarationId, DefinitionId, NameId, UriId},
    name::ParentScope,
};

use super::{
    environment::{self, Trees},
    locator::{self, Site},
    references, render, symbols,
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

/// One edge of the call graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The caller, for `incomingCalls`; the callee, for `outgoingCalls`.
    pub item: Item,
    /// Every call site, as offsets into the file the calls are *written* in — the row's own file
    /// for an incoming call, the expanded method's file for an outgoing one. The protocol calls
    /// these `fromRanges`, and they are why the answer is a bucket per method rather than a row
    /// per call: a method that calls another four times is one row carrying four ranges.
    pub ranges: Vec<(u32, u32)>,
}

/// What one direction found, and how much of it fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calls {
    pub calls: Vec<Call>,
    /// How many rows there were before the cap, for the sentence that says so.
    pub found: usize,
}

/// What the detail column says when the name in the chain resolved to nothing.
const NOT_FOUND: &str = "not found";

/// What the detail column says on a row found by matching a name.
///
/// Every incoming row carries it, because every incoming row is a name match. It is two words
/// rather than hover's full sentence for the same reason `NOT_FOUND` is: this is a column in a
/// tree, and a tree row has no room for prose.
const BY_NAME: &str = "by name";

/// What the detail column says on the file a call outside any method was written in.
const OUTSIDE_A_METHOD: &str = "outside any method";

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
    names: environment::Names<'_>,
) -> Vec<Item> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        // As in goto-definition and references: several targets can share the narrowest span, so
        // take the first that has something to say rather than the first that exists.
        .find_map(|located| {
            let items: Vec<Item> = locator::resolve(graph, &located)
                .declarations
                .into_iter()
                .filter_map(|id| item(graph, synthesized, id, own, names))
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
    names: environment::Names<'_>,
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
            Ancestor::Complete(ancestor) => item(graph, synthesized, *ancestor, own, names),
            Ancestor::Partial(name) => unresolved(graph, &chain, *name),
        })
        .collect()
}

/// Everything that inherits from `declaration`, the user's own code first and the application's
/// own subclasses ahead of the suite's doubles.
///
/// Ranked before it is capped, because the reverse index is a hash set: taking the first `limit`
/// of it in the order it iterates would hand back a different arbitrary slice on every run, and
/// at the top of the object model the slice is what the user sees. The second term is a **rank
/// and never a drop** — see [`environment`](super::environment) for the table of which surfaces
/// may do which, and why this one may not.
#[must_use]
pub fn subtypes(
    graph: &Graph,
    synthesized: &Synthesized,
    declaration: DeclarationId,
    limit: usize,
    own: &HashSet<UriId>,
    names: environment::Names<'_>,
) -> Subtypes {
    let Some(subject) = graph.declarations().get(&declaration).and_then(namespace) else {
        return Subtypes {
            items: Vec::new(),
            found: 0,
        };
    };

    // Ranked on what is cheap to read — two flags and the name rubydex already holds — and
    // turned into items only for the survivors, the same split `search::search` makes and for
    // the same reason: a class near the root of the object model has tens of thousands of
    // descendants and finding the file each is written in means reaching into every definition
    // of every one.
    let trees = Trees::of(graph, own, names);
    let mut ranked: Vec<(bool, bool, &str, DeclarationId)> = subject
        .descendants()
        .iter()
        // A class is in its own descendants too, for the same reason it is in its own ancestors.
        .filter(|descendant| **descendant != declaration)
        .filter_map(|descendant| {
            let subtype = graph.declarations().get(descendant)?;
            namespace(subtype)?;
            let placement = environment::placement(graph, subtype, own, &trees);
            Some((
                placement.own,
                placement.loadable,
                subtype.name(),
                *descendant,
            ))
        })
        .collect();
    let found = ranked.len();

    // The project ahead of its bundle, for `search::rank`'s reason: asking `StandardError` for
    // its subtypes in a Rails app finds hundreds in gems and a handful that are the user's, and
    // an alphabetical list would bury the handful. Then the application ahead of its suite,
    // which is the same argument one level in: a base class the project actually subclasses
    // twice is subclassed twenty times by doubles under `spec/`, and alphabetical order puts
    // `FakeStore` above `Store::Admin`. It sinks and never drops — the double really does
    // inherit, and this list is read to find out what does. Never by `DeclarationId`, which is a
    // hash and would shuffle between runs.
    ranked.sort_unstable_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(right.1.cmp(&left.1))
            .then(left.2.cmp(right.2))
    });
    ranked.truncate(limit);

    Subtypes {
        items: ranked
            .into_iter()
            .filter_map(|(_, _, _, id)| item(graph, synthesized, id, own, names))
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
    names: environment::Names<'_>,
) -> Option<Item> {
    let declaration = graph.declarations().get(&id)?;
    namespace(declaration)?;
    let definition = locator::preferred_definition(graph, id, own, names)?;
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

/// The method the cursor is on, ready for the client to expand.
///
/// A call site answers as readily as a `def` does, because pressing "show call hierarchy" on a
/// call is how the feature is usually reached and `locate` answers both without being asked
/// differently. Everything that is not a method answers `null` — the mirror of [`prepare`], which
/// turns methods away, and the same reason: a tree of the wrong thing is worse than "no results".
#[must_use]
pub fn prepare_calls(
    graph: &Graph,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    own: &HashSet<UriId>,
    names: environment::Names<'_>,
) -> Vec<Item> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        // As in goto-definition and `prepare`: several targets can share the narrowest span.
        .find_map(|located| {
            let items: Vec<Item> = locator::resolve(graph, &located)
                .declarations
                .into_iter()
                .filter_map(|id| method_item(graph, synthesized, id, own, names))
                .collect();
            (!items.is_empty()).then_some(items)
        })
        .unwrap_or_default()
}

/// Every call of `declaration`, bucketed by the method each call is written in.
///
/// Ranked by nothing, deliberately: the rows come out in the order the calls are read, file by
/// file and then down each file, which is `references`' order and the only one that does not need
/// defending. The cap is on rows for `MAX_SUBTYPES`' reason — finding them is a scan either way,
/// *drawing* one reads the file it lives in.
#[must_use]
pub fn incoming(
    graph: &Graph,
    synthesized: &Synthesized,
    declaration: DeclarationId,
    limit: usize,
    scope: &HashSet<UriId>,
) -> Calls {
    let references = references::calls_to(graph, declaration, scope);

    // A `HashMap` alone would arrange the same rows differently on every run, and with a cap in
    // front of them a different arrangement is a different *answer*. So the map holds positions
    // into a list that keeps the order the calls arrived in.
    let mut rows: Vec<(Caller, Vec<(u32, u32)>)> = Vec::new();
    let mut seen: HashMap<Caller, usize> = HashMap::new();

    // Grouped by file so the document's `def`s are collected once per file rather than once per
    // call site: `calls_to` has already sorted by uri and then by offset.
    for group in references.chunk_by(|left, right| left.uri == right.uri) {
        let uri_id = UriId::from(group[0].uri.as_str());
        // The uri came out of `graph.documents()` by way of `calls_to`, so a miss here is a
        // lookup that yields nothing rather than a case with anything to do about it — the same
        // shape, and the same reason, as the two loops in `references`.
        let Some(document) = graph.documents().get(&uri_id) else {
            continue;
        };
        let bodies = method_bodies(graph, document);
        for reference in group {
            let caller = match containing(&bodies, reference.start) {
                Some(body) => Caller::Method(body.id()),
                None => Caller::File(uri_id),
            };
            let row = *seen.entry(caller).or_insert_with(|| {
                rows.push((caller, Vec::new()));
                rows.len() - 1
            });
            rows[row].1.push((reference.start, reference.end));
        }
    }

    let found = rows.len();
    rows.truncate(limit);

    Calls {
        calls: rows
            .into_iter()
            .filter_map(|(caller, ranges)| {
                let item = match caller {
                    Caller::Method(id) => {
                        caller_item(graph, synthesized, graph.definitions().get(&id)?)?
                    }
                    Caller::File(uri_id) => file_item(graph, uri_id)?,
                };
                Some(Call { item, ranges })
            })
            .collect(),
        found,
    }
}

/// Every method the body written at `offset` calls.
///
/// **Addressed by a position and not by a declaration**, which is the one place this direction
/// disagrees with everything else in the file. A row the client expands names one body — the
/// file and the span it was drawn from — and a method reopened in two files has two bodies with
/// different calls in them. A declaration would have to pick one, and it would pick the wrong one
/// for every incoming-call row, which points at the body the calls were *found* in rather than at
/// whichever body `preferred_definition` prefers.
///
/// No cap, and that is an argument rather than an omission: the population is the distinct calls
/// inside one `def`, which a person wrote by hand. Incoming calls are bounded by the workspace.
///
/// **Only a call whose receiver was named produces a row** — [`locator::precise_call`], the gate
/// `signatureHelp` fires from. The alternative is the name rung, which would answer `person.name`
/// with every method called `name` in the graph and draw forty edges out of one call site. The
/// redirect is kept, so `Foo.new` is an edge to `Foo#initialize`: that is the method that runs.
#[must_use]
pub fn outgoing(
    graph: &Graph,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    own: &HashSet<UriId>,
    names: environment::Names<'_>,
    layout: environment::Layout<'_>,
) -> Vec<Call> {
    // A file the graph does not hold: written since the last settle, or excluded from the index
    // after the client got hold of this item. There is nothing to expand and no error to raise.
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };
    let bodies = method_bodies(graph, document);
    // Not inside a `def` at all — the file row an incoming call produces is exactly this, and
    // expanding one is a question with the honest answer "nothing".
    let Some(body) = containing(&bodies, offset) else {
        return Vec::new();
    };

    let mut sites: Vec<(u32, u32)> = document
        .method_references()
        .iter()
        .filter_map(|id| graph.method_references().get(id))
        .map(|reference| (reference.offset().start(), reference.offset().end()))
        // The cheap bound first, because most of a file's calls are outside any one body. Only
        // the start is compared: a reference that starts inside a body ends inside it, so testing
        // the end as well would be an arm no input can take.
        .filter(|(start, _)| *start >= body.offset().start() && *start < body.offset().end())
        // A call inside a nested `def` was made by that `def`. The containment question
        // `incoming` asks to *find* a caller, asked here to exclude one.
        .filter(|(start, _)| containing(&bodies, *start).map(Definition::id) == Some(body.id()))
        .collect();
    // Source order, so the rows read the way the body does. rubydex's reference list is per
    // document and nothing promises an order within it.
    sites.sort_unstable();

    let mut rows: Vec<(DeclarationId, Vec<(u32, u32)>)> = Vec::new();
    let mut seen: HashMap<DeclarationId, usize> = HashMap::new();
    for (start, end) in sites {
        // **No cursor, so no syntax to read.** This walks every call site in a body rather than
        // answering at one, and a row saying a body calls a private method is true whether or
        // not the interpreter would permit the spelling. See `locator::Privacy`.
        let Some(callee) =
            locator::precise_call(graph, uri_id, start, layout, locator::Privacy::Allowed)
        else {
            continue;
        };
        let row = *seen.entry(callee).or_insert_with(|| {
            rows.push((callee, Vec::new()));
            rows.len() - 1
        });
        rows[row].1.push((start, end));
    }

    rows.into_iter()
        .filter_map(|(callee, ranges)| {
            Some(Call {
                item: method_item(graph, synthesized, callee, own, names)?,
                ranges,
            })
        })
        .collect()
}

/// Who a call site is attributed to.
///
/// A definition rather than a declaration for the reason in this module's comment: a row's ranges
/// are drawn against the row's own file, and a reopened class has a body in more than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Caller {
    Method(DefinitionId),
    File(UriId),
}

/// One row for a method declaration, pointing at the definition the rest of the server prefers.
///
/// `None` for everything that is not a method, which is what makes `prepare_calls` decline a
/// class without a second test for it.
fn method_item(
    graph: &Graph,
    synthesized: &Synthesized,
    id: DeclarationId,
    own: &HashSet<UriId>,
    names: environment::Names<'_>,
) -> Option<Item> {
    let declaration = graph.declarations().get(&id)?;
    matches!(declaration, Declaration::Method(_)).then_some(())?;
    let definition = locator::preferred_definition(graph, id, own, names)?;
    row(graph, synthesized, declaration.name(), definition, Some(id))
}

/// One row for the method a call was written inside.
///
/// Keyed off the definition rather than looked up again, so the row points at *this* body — the
/// one the ranges are in — and not at whichever body `preferred_definition` would have chosen.
/// The declaration still rides along in `data`, because expanding this row is a question about
/// the method rather than about one of its bodies.
fn caller_item(graph: &Graph, synthesized: &Synthesized, definition: &Definition) -> Option<Item> {
    let id = *graph.definition_to_declaration_id(definition)?;
    let declaration = graph.declarations().get(&id)?;
    let mut item = row(graph, synthesized, declaration.name(), definition, Some(id))?;
    item.detail = format!("{} — {BY_NAME}", item.detail);
    Some(item)
}

/// One row for a file, for a call written outside every `def` in it.
///
/// The span is the start of the file: there is no construct to point at, and a range covering the
/// whole file would make an editor select all of it on click. `declaration` is `None`, so the
/// client can expand it and get nothing — which is the truth, a file has no callers.
fn file_item(graph: &Graph, uri_id: UriId) -> Option<Item> {
    let uri = graph.documents().get(&uri_id)?.uri().to_owned();
    Some(Item {
        name: file_name(&uri),
        kind: SymbolKind::FILE,
        detail: OUTSIDE_A_METHOD.to_owned(),
        declaration: None,
        site: Site {
            uri,
            full: (0, 0),
            selection: (0, 0),
        },
    })
}

/// The shape both hierarchies' rows share, given the definition the row points at.
fn row(
    graph: &Graph,
    synthesized: &Synthesized,
    name: &str,
    definition: &Definition,
    declaration: Option<DeclarationId>,
) -> Option<Item> {
    let site = locator::site(graph, synthesized, definition)?;
    Some(Item {
        // Qualified — `Person#shout` rather than `shout` — because a call tree is read across
        // files, and `render::simple_name` only takes rubydex's parentheses off the end.
        name: render::simple_name(name).to_owned(),
        kind: symbols::kind_of(definition),
        detail: file_name(&site.uri),
        declaration,
        site,
    })
}

/// Every `def` in one document, which is every span a call site can be attributed to.
///
/// Methods only. An `attr_reader` declares one but has no body for a call to be inside, and a
/// class or module body is deliberately not a caller — see this module's comment.
fn method_bodies<'g>(graph: &'g Graph, document: &Document) -> Vec<&'g Definition> {
    document
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
        .filter(|definition| matches!(definition, Definition::Method(_)))
        .collect()
}

/// The innermost `def` whose body contains `offset`.
///
/// Innermost is the greatest start offset among those that contain it: a definition nested inside
/// another starts after it and ends before it, so nothing else has to be compared. Linear in the
/// file's `def`s, which is why the caller collects them once per file and not once per call.
fn containing<'g>(bodies: &[&'g Definition], offset: u32) -> Option<&'g Definition> {
    bodies
        .iter()
        .filter(|definition| {
            definition.offset().start() <= offset && offset < definition.offset().end()
        })
        .max_by_key(|definition| definition.offset().start())
        .copied()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;
    use crate::analysis::{MAX_INCOMING_CALLS, MAX_SUBTYPES};

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

        assert!(
            supertypes(
                &graph,
                &synthesized,
                nowhere,
                &own,
                environment::Names::default()
            )
            .is_empty()
        );
        assert_eq!(
            subtypes(
                &graph,
                &synthesized,
                nowhere,
                10,
                &own,
                environment::Names::default()
            ),
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

    /// A three-deep chain, a module included halfway up it, a module prepended at the bottom, a
    /// sibling, and a superclass nothing can resolve.
    ///
    /// Written as one file so the fixture and the answers can be read against each other. The
    /// prepend is not decoration: it is the one shape where the class is *not* the first entry
    /// of its own ancestor chain, so dropping "the head of the list" instead of "the entry that
    /// is this class" would pass every other test here.
    const HIERARCHY: &str = "\
module Greet
end

module Loud
end

class Base
end

class Middle < Base
  include Greet
end

class Leaf < Middle
  prepend Loud
end

class Other < Base
end

class Orphan < Missing::Thing
end
";

    /// The fixture indexed, with signatures and gems off — so the rows are the project's own.
    fn hierarchy_harness() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("lib/hierarchy.rb", HIERARCHY);
        harness.index();
        (harness, uri)
    }

    #[test]
    fn the_supertypes_of_a_class_are_its_ruby_ancestors_in_ruby_order() {
        // The whole list, not a `contains`: this is `Module#ancestors` and the interesting thing
        // about it is its *composition*. `Loud` above `Leaf` because a prepended module wins
        // method lookup, `Greet` between `Middle` and `Base` because that is where it was
        // included, and `Leaf` itself nowhere — a class is in its own ancestors and is not its
        // own supertype.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, HIERARCHY, "Leaf <"),
            "module Loud — hierarchy.rb\n\
             class Middle — hierarchy.rb\n\
             module Greet — hierarchy.rb\n\
             class Base — hierarchy.rb"
        );
    }

    #[test]
    fn object_and_kernel_are_in_the_chain_only_when_there_is_a_file_to_point_at() {
        // Every Ruby class inherits from `Object`, `Kernel` and `BasicObject`, and rubydex knows
        // it without any signatures — it carries the five of them as a built-in document called
        // `rubydex:built-in`, which has no file behind it. `DocUri` rejects that URI for every
        // request alike, so the rows are dropped here rather than sent as somewhere an editor
        // cannot open. With signatures indexed they come back, out of `core/*.rbs`, which is the
        // test below.
        let (mut harness, uri) = hierarchy_harness();
        let rows = harness.hierarchy_rows("typeHierarchy/supertypes", &uri, HIERARCHY, "Base\nend");
        assert!(!rows.contains("Object"), "{rows}");
        assert!(!rows.contains("Kernel"), "{rows}");
    }

    #[test]
    fn the_subtypes_of_a_class_are_every_class_below_it_and_not_just_the_next_one() {
        // The mirror of the supertypes above, and deliberately transitive to match them: `Leaf`
        // is two levels under `Base` and is listed, because a chain answered one way and a
        // single generation answered the other would be a tree whose two directions disagree
        // about what a level means. `Base` itself is not in it, and neither is `Orphan`, which
        // inherits from something else.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, HIERARCHY, "Base\nend"),
            "class Leaf — hierarchy.rb\n\
             class Middle — hierarchy.rb\n\
             class Other — hierarchy.rb"
        );
    }

    #[test]
    fn a_double_declared_only_in_a_spec_is_listed_under_the_real_subclasses() {
        // Both halves. The double is **listed** — it really does inherit, and this list is read
        // to find out what does — and it is listed **last**, under every subclass the
        // application loads, because `MAX_SUBTYPES` is spent from the top and a base class with
        // two real subclasses routinely has twenty doubles. Alphabetical order is what used to
        // decide it, and `FakeStore` sorts above `Warehouse`.
        let mut harness = Harness::new();
        let app = harness.write(
            "app/models/store.rb",
            "class Store\nend\n\nclass Warehouse < Store\nend\n",
        );
        harness.write("spec/support/doubles.rb", "class FakeStore < Store\nend\n");
        harness.index();

        assert_eq!(
            harness.hierarchy_rows(
                "typeHierarchy/subtypes",
                &app,
                "class Store\nend\n\nclass Warehouse < Store\nend\n",
                "Store\nend"
            ),
            "class Warehouse — store.rb\n\
             class FakeStore — doubles.rb"
        );
    }

    #[test]
    fn a_call_from_a_spec_is_an_incoming_call_and_this_list_is_never_fenced() {
        // The other side of the same rule, and the half with teeth: `incomingCalls` answers
        // *where is this used*, and a use under `spec/` is a use. A list that quietly omitted
        // the suite would be a refactor that breaks it — which is `references`' argument, and
        // the reason `environment` names this surface as one that must never ask.
        let mut harness = Harness::new();
        let app = harness.write(
            "app/models/store.rb",
            "class Store\n  def ship\n  end\nend\n",
        );
        harness.write(
            "spec/models/store_spec.rb",
            "class StoreSpec\n  def test_ship\n    Store.new.ship\n  end\nend\n",
        );
        harness.index();

        let rows = harness.call_rows(
            "callHierarchy/incomingCalls",
            &app,
            "class Store\n  def ship\n  end\nend\n",
            "ship",
        );
        assert!(rows.contains("test_ship"), "{rows}");
    }

    #[test]
    fn a_module_lists_the_classes_that_mix_it_in() {
        // `include` and `prepend` both put a class into a module's descendants, which is what
        // makes "who uses this concern" a question the hierarchy answers. `Greet` is included by
        // `Middle` and reaches `Leaf` through it.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, HIERARCHY, "Greet\nend"),
            "class Leaf — hierarchy.rb\n\
             class Middle — hierarchy.rb"
        );
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, HIERARCHY, "Loud\nend"),
            "class Leaf — hierarchy.rb"
        );
    }

    #[test]
    fn an_ancestor_that_did_not_resolve_is_a_row_that_says_so() {
        // The silent-degradation case, and the reason the partial arm is not simply dropped: a
        // superclass in a gem that did not install would otherwise leave a chain that reads as
        // complete and is short by everything above the gap. The row is spelled as it was
        // written, its kind comes from having been written as a superclass rather than a mixin,
        // and it carries no `data` — there is nothing to expand.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, HIERARCHY, "Orphan <"),
            "class Missing::Thing — not found"
        );

        let expanded = harness.expand("typeHierarchy/supertypes", &uri, HIERARCHY, "Orphan <");
        assert!(
            expanded[0]["data"].is_null(),
            "a row for a name that resolved to nothing has nothing behind it"
        );
    }

    #[test]
    fn an_unresolved_ancestor_is_placed_where_it_is_written_even_when_that_is_another_class() {
        // A partial propagates down the chain: `Cursed`'s ancestors carry the `Missing::Thing`
        // that `Orphan` inherits from, and it is written in `Orphan`. So the whole chain is
        // searched for the mention rather than only the class being expanded — otherwise the row
        // is either missing or pointing at the wrong line, and the row exists to be clicked.
        //
        // `Cursed` carries an unresolved mixin of its own so that the chain holds *two* names
        // that resolved to nothing. That is what makes the search step over one partial on its
        // way to the mention of another, which is the ordinary case in a project with a gem
        // missing and the one a single unresolved name never reaches.
        let source = "class Orphan < Missing::Thing\nend\n\nclass Cursed < Orphan\n  \
                      include AlsoMissing\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/cursed.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Cursed <"),
            "module AlsoMissing — not found\n\
             class Orphan — cursed.rb\n\
             class Missing::Thing — not found"
        );
        let expanded = harness.expand("typeHierarchy/supertypes", &uri, source, "Cursed <");
        assert_eq!(
            expanded[2]["selectionRange"]["start"],
            serde_json::json!({ "line": 0, "character": 15 }),
            "the span of `Missing::Thing` on `class Orphan`'s own line"
        );
        assert_eq!(
            expanded[0]["selectionRange"]["start"],
            serde_json::json!({ "line": 4, "character": 10 }),
            "and `AlsoMissing` where `Cursed` writes it"
        );
    }

    #[test]
    fn a_module_says_which_of_its_own_mixins_did_not_resolve() {
        // A module's ancestors are its mixins, and rubydex keeps `include`d names on the module
        // definition rather than on the enum — so a module in the chain is a case of its own,
        // and the concern that includes a missing concern is a shape Rails code has.
        let source = "module Bag\n  include Gone::Bits\nend\n\nclass Holder\n  \
                      include Bag\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/bag.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Bag\n  include"),
            "module Gone::Bits — not found"
        );
        // And through the class that includes it, which is where the propagation and the module
        // case meet.
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Holder"),
            "module Bag — bag.rb\n\
             module Gone::Bits — not found"
        );
    }

    #[test]
    fn a_mixin_that_did_not_resolve_is_a_module_and_a_superclass_is_a_class() {
        // Nothing in the graph says what a name that resolved to nothing *was*, but the source
        // does: `include` takes a module and `<` takes a class. Both rows would otherwise have to
        // guess, and a guess here shows the user the wrong icon on the only row on the screen
        // that is about something being missing.
        let source = "class Odd < Gone::Parent\n  include Gone::Concern\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/odd.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Odd <"),
            "module Gone::Concern — not found\n\
             class Gone::Parent — not found"
        );
    }

    #[test]
    fn an_explicit_root_is_kept_in_the_name_of_a_row_that_could_not_be_found() {
        // `::Foo` failing where `Foo` would have resolved is frequently the reason, and this row
        // is read rather than clicked, so the name is the whole of what it has to offer.
        let source = "class Rooted < ::Nowhere\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/rooted.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Rooted <"),
            "class ::Nowhere — not found"
        );
    }

    #[test]
    fn the_hierarchy_is_prepared_from_a_use_of_a_name_as_well_as_from_its_definition() {
        // Two of `locator`'s three targets reach here: the `class Leaf` definition and the
        // `Middle` written after the `<`, which is a constant reference. Both are things a user
        // right-clicks, and both have to answer with the class rather than with nothing.
        let (mut harness, uri) = hierarchy_harness();
        let from_definition = harness.prepare_hierarchy(&uri, HIERARCHY, "Leaf <");
        assert_eq!(from_definition[0]["name"], serde_json::json!("Leaf"));

        let from_reference = harness.prepare_hierarchy(&uri, HIERARCHY, "Middle\n  prepend");
        assert_eq!(from_reference[0]["name"], serde_json::json!("Middle"));
        assert_eq!(
            from_reference[0]["selectionRange"]["start"]["line"],
            serde_json::json!(9),
            "the `class Middle` line, not the line the reference is on"
        );
    }

    #[test]
    fn nothing_that_is_not_a_class_or_a_module_is_offered_a_hierarchy() {
        // `null`, not an empty list, which is what makes the editor say there are no results
        // rather than open an empty tree. A method is the case that matters: `references` matches
        // a method by name, so a cursor on `shout` resolves to declarations — they are simply
        // not types, and the rejection falls out of asking the resolution for a namespace.
        let source = "class Person\n  MAX = 3\n  def shout\n    total = MAX\n  end\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", source);
        harness.index();

        for needle in ["shout", "MAX = 3", "total"] {
            assert_eq!(
                harness.prepare_hierarchy(&uri, source, needle),
                serde_json::Value::Null,
                "{needle:?} is not a type"
            );
        }
    }

    #[test]
    fn a_singleton_class_is_not_a_type_anybody_asked_about() {
        // rubydex models `class << self` as a namespace called `Person::<Person>`, and it has a
        // real ancestor chain — `Class`, `Module`, `Object`. It is not a name a person wrote, so
        // it is turned away on the same two tests the symbol picker uses, and a cursor on
        // `self` there answers `null` rather than opening a tree over ya-lsp's own spelling.
        let source = "class Person\n  class << self\n    def build; end\n  end\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", source);
        harness.index();

        assert_eq!(
            harness.prepare_hierarchy(&uri, source, "<< self"),
            serde_json::Value::Null
        );
    }

    #[test]
    fn an_item_the_client_hands_back_without_a_usable_declaration_answers_nothing() {
        // Both follow-ups take their subject from the client, which echoes back whatever the row
        // it is expanding carried. A row for an unresolved name has no `data` at all, a client
        // may send one that is not a number, and a rubydex id is a 64-bit hash — so a stale one
        // is indistinguishable from a live one and has to answer "nothing" rather than resolve
        // against whatever it collides with.
        let (mut harness, _) = hierarchy_harness();
        for data in [
            serde_json::Value::Null,
            serde_json::json!("not a number"),
            serde_json::json!("0"),
            serde_json::json!(1_234_567_890_123_456_789_u64),
            serde_json::json!("1234567890123456789"),
        ] {
            let item = serde_json::json!({
                "name": "Ghost",
                "kind": 5,
                "uri": "file:///nowhere.rb",
                "range": { "start": { "line": 0, "character": 0 },
                           "end": { "line": 0, "character": 0 } },
                "selectionRange": { "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 0 } },
                "data": data,
            });
            for method in ["typeHierarchy/supertypes", "typeHierarchy/subtypes"] {
                assert_eq!(
                    harness.ask(method, serde_json::json!({ "item": item })),
                    serde_json::Value::Null,
                    "{method} on {:?}",
                    item["data"]
                );
            }
        }
    }

    /// One file, two classes and a class body, for the call hierarchy.
    ///
    /// `name + name` is deliberate: two calls of one method from one body is what makes a row a
    /// bucket rather than a list, and the `+` beside them is a call whose receiver nothing typed.
    /// `Siren#greet` is spelled like `Person#greet` and has nothing to do with it, which is the
    /// name match this feature rests on, written down where the tests can see it.
    const CALL_SITES: &str = "\
class Person
  def name
    \"Ada\"
  end

  def greet
    name + name
  end

  def label
    greet
  end
end

class Siren
  def greet
    \"wee\"
  end
end

class Report
  greet
end
";

    /// The fixture indexed, with signatures and gems off.
    fn calls_harness() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("lib/calls.rb", CALL_SITES);
        harness.index();
        (harness, uri)
    }

    #[test]
    fn a_call_hierarchy_prepares_from_the_def_and_from_a_call_site_alike() {
        // Both are how the command is reached — the cursor is as often on a call as on the `def`
        // — and `locate` answers both without being asked differently. The item has to be the
        // same one either way, because the client sends it back and the follow-ups read it.
        let (mut harness, uri) = calls_harness();

        let from_def = harness.prepare_calls(&uri, CALL_SITES, "greet\n    name");
        let from_call = harness.prepare_calls(&uri, CALL_SITES, "greet\n  end\nend\n\nclass Siren");
        assert_eq!(from_def[0]["name"], "Person#greet");
        assert_eq!(from_def, from_call, "the same method, prepared two ways");
    }

    #[test]
    fn incoming_calls_are_bucketed_by_the_method_each_call_is_written_in() {
        // The whole answer, drawn: one row per caller with every call site it holds, in the
        // order the file reads. `Person#label` is a name match and says so — `Siren#greet` is
        // spelled the same and this list cannot tell them apart, which is the trade `references`
        // makes and the one a *tree* hides unless the row admits it.
        //
        // The second row is the call written in `class Report`'s body, which is inside no `def`
        // at all: attributed to the file rather than dropped, because a Rails model's macros are
        // all of that shape and dropping them would empty the most useful answer this gives.
        let (mut harness, uri) = calls_harness();

        assert_eq!(
            harness.call_rows(
                "callHierarchy/incomingCalls",
                &uri,
                CALL_SITES,
                "greet\n    name"
            ),
            "Person#label — calls.rb — by name [10:4-9]\n\
             calls.rb — outside any method [21:2-7]"
        );
    }

    #[test]
    fn outgoing_calls_are_the_calls_whose_receiver_was_named() {
        // `name` twice from one body is one row with two ranges; `+` is a call on whatever `name`
        // returned, which nothing typed, so it is not an edge. A name-based fallback would have
        // drawn `+` to every `+` in the graph, which is why this direction does not have one.
        let (mut harness, uri) = calls_harness();

        assert_eq!(
            harness.call_rows(
                "callHierarchy/outgoingCalls",
                &uri,
                CALL_SITES,
                "greet\n    name"
            ),
            "Person#name — calls.rb [6:4-8 6:11-15]"
        );

        // The body *after* the calls that are not its own, which is the ordinary shape and the
        // one that says the span test is a test rather than a formality: `name` is written twice
        // above `label` and belongs to `greet`, and `greet` resolves to `Person`'s rather than to
        // the `Siren#greet` spelled the same way further down the file.
        assert_eq!(
            harness.call_rows("callHierarchy/outgoingCalls", &uri, CALL_SITES, "label"),
            "Person#greet — calls.rb [10:4-9]"
        );
    }

    #[test]
    fn a_call_in_a_nested_def_belongs_to_the_nested_def_in_both_directions() {
        // `def` inside `def` is legal Ruby and the one shape where "the innermost body that
        // contains this offset" is not the same as "the body this is written in". Incoming has to
        // attribute the call to `inner`; outgoing has to leave it out of `wrapper`, which is the
        // same question asked from the other side and answered by the same function.
        let source = "\
class Outer
  def wrapper
    def inner
      name
    end
  end

  def name
    \"x\"
  end
end
";
        let mut harness = Harness::new();
        let uri = harness.write("lib/nested.rb", source);
        harness.index();

        assert_eq!(
            harness.call_rows("callHierarchy/incomingCalls", &uri, source, "name\n    end"),
            "Outer#inner — nested.rb — by name [3:6-10]"
        );
        assert_eq!(
            harness.call_rows("callHierarchy/outgoingCalls", &uri, source, "wrapper"),
            "null",
            "the nested def's call is not the outer def's"
        );
    }

    #[test]
    fn a_call_hierarchy_is_offered_on_methods_and_on_nothing_else() {
        // The mirror of what `prepareTypeHierarchy` declines. A class answers `null` so the
        // editor says "no results" rather than drawing a tree of the wrong thing, and a cursor on
        // a string literal is the ordinary way the command is pressed by accident.
        let (mut harness, uri) = calls_harness();

        for needle in ["Person\n  def", "\"Ada\""] {
            assert_eq!(
                harness.prepare_calls(&uri, CALL_SITES, needle),
                serde_json::Value::Null,
                "prepared a call hierarchy at {needle:?}"
            );
        }
    }

    #[test]
    fn a_truncated_list_of_callers_says_so() {
        // The cap is reached by an ordinary method name rather than by an unusual question — a
        // `call` or a `name` has callers in every file — so a short list of callers is both
        // likely and indistinguishable from a complete one. Said out loud, like the other two.
        let mut source = String::from("class Big\n  def ping\n  end\n");
        for index in 0..=MAX_INCOMING_CALLS {
            source.push_str(&format!("  def c{index}\n    ping\n  end\n"));
        }
        source.push_str("end\n");

        let mut harness = Harness::new();
        let uri = harness.write("lib/big.rb", &source);
        harness.index();

        let prepared = harness.prepare_calls(&uri, &source, "ping\n  end");
        let found = harness.ask(
            "callHierarchy/incomingCalls",
            serde_json::json!({ "item": prepared[0].clone() }),
        );
        assert_eq!(found.as_array().map(Vec::len), Some(MAX_INCOMING_CALLS));
        assert_eq!(
            harness.messages(),
            vec![messages::incoming_calls_truncated(
                MAX_INCOMING_CALLS + 1,
                MAX_INCOMING_CALLS
            )]
        );
    }

    #[test]
    fn expanding_an_item_with_no_body_behind_it_answers_null() {
        // Two items a client can legitimately send back and neither has calls to list. The file
        // row is one this server produced itself — every incoming answer over a Rails model has
        // one — and expanding it asks "what does a file call", which has no answer. The second is
        // a file written since the last settle: the text is on disk for `with_text` to read and
        // the graph has never seen it, which is what a stale item looks like after a rebuild.
        let (mut harness, uri) = calls_harness();

        let prepared = harness.prepare_calls(&uri, CALL_SITES, "greet\n    name")[0].clone();
        let callers = harness.ask(
            "callHierarchy/incomingCalls",
            serde_json::json!({ "item": prepared }),
        );
        let file_row = callers[1]["from"].clone();
        assert_eq!(file_row["name"], "calls.rb", "the file row moved");
        assert_eq!(
            harness.ask(
                "callHierarchy/outgoingCalls",
                serde_json::json!({ "item": file_row })
            ),
            serde_json::Value::Null
        );

        let source = "class Late\n  def ring\n    ring\n  end\nend\n";
        let unindexed = harness.write("lib/late.rb", source);
        assert_eq!(
            harness.ask(
                "callHierarchy/outgoingCalls",
                serde_json::json!({ "item": {
                    "name": "Late#ring",
                    "kind": 6,
                    "uri": unindexed.as_str(),
                    "range": { "start": position_of(source, "def ring"),
                               "end": position_of(source, "def ring") },
                    "selectionRange": { "start": position_of(source, "ring\n    ring"),
                                        "end": position_of(source, "ring\n    ring") },
                } })
            ),
            serde_json::Value::Null
        );
    }

    #[test]
    fn malformed_call_hierarchy_params_are_answered_with_null_rather_than_a_panic() {
        // Client input, like every other handler's. The two follow-ups take an item rather than a
        // position, and `outgoingCalls` reads a uri and a range out of it that `incomingCalls`
        // never touches — so a garbage item has to be survivable in two different ways.
        let (mut harness, _) = calls_harness();
        for method in [
            "textDocument/prepareCallHierarchy",
            "callHierarchy/incomingCalls",
            "callHierarchy/outgoingCalls",
        ] {
            assert_eq!(
                harness.ask(method, serde_json::json!({ "nonsense": true })),
                serde_json::Value::Null,
                "{method}"
            );
        }

        let ghost = serde_json::json!({
            "name": "Ghost#vanish",
            "kind": 6,
            "uri": "file:///nowhere.rb",
            "range": { "start": { "line": 0, "character": 0 },
                       "end": { "line": 0, "character": 0 } },
            "selectionRange": { "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 0 } },
            "data": "1234567890123456789",
        });
        for method in ["callHierarchy/incomingCalls", "callHierarchy/outgoingCalls"] {
            assert_eq!(
                harness.ask(method, serde_json::json!({ "item": ghost })),
                serde_json::Value::Null,
                "{method} on an item the graph does not hold"
            );
        }
    }

    #[test]
    fn malformed_hierarchy_params_are_answered_with_null_rather_than_a_panic() {
        // Client input, like every other handler's params. All three arms, because each parses a
        // different shape and the two follow-ups do not take a position at all.
        let (mut harness, _) = hierarchy_harness();
        for method in [
            "textDocument/prepareTypeHierarchy",
            "typeHierarchy/supertypes",
            "typeHierarchy/subtypes",
        ] {
            assert_eq!(
                harness.ask(method, serde_json::json!({ "nonsense": true })),
                serde_json::Value::Null
            );
        }
    }

    #[test]
    fn a_truncated_list_of_subtypes_says_so() {
        // A short list of subtypes is indistinguishable from a complete one, so reaching the cap
        // is said out loud rather than only logged. Reached here by asking about a class with
        // more subtypes than the answer holds, which in a real project takes asking about
        // something near the root of the object model.
        let mut source = String::from("class Root\nend\n");
        for index in 0..(MAX_SUBTYPES + 3) {
            source.push_str(&format!("class Sub{index} < Root\nend\n"));
        }
        let mut harness = Harness::new();
        let uri = harness.write("lib/many.rb", &source);
        harness.index();

        let found = harness.expand("typeHierarchy/subtypes", &uri, &source, "Root\nend");
        assert_eq!(found.as_array().map(Vec::len), Some(MAX_SUBTYPES));
        assert_eq!(
            harness.messages(),
            vec![format!(
                "{} subtypes found: only the first {MAX_SUBTYPES} are shown.",
                MAX_SUBTYPES + 3
            )]
        );
    }

    #[test]
    fn the_users_own_code_is_ranked_above_the_bundle_and_the_rest_alphabetically() {
        // `search::rank`'s decision, for `search::rank`'s reason: asking a widely-subclassed
        // class for its subtypes in a real project finds hundreds in gems and a handful that are
        // the user's, and an alphabetical list would bury the handful below whatever the bundle
        // happens to spell with an `A`. Within each half the order is the name, never the
        // `DeclarationId` — that is a hash, and a tree that reshuffles between runs is
        // unreadable.
        let (dir, _gem_home, env) = project_with_gem(
            "module Shouty\n  class Base\n  end\n\n  class Middle < Base\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "class ZLocal < Shouty::Base\nend\n";
        let uri = harness.write("lib/z_local.rb", source);
        harness.index();
        harness.index_gems();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, source, "Base\nend"),
            "class ZLocal — z_local.rb\n\
             class Shouty::Middle — shouty.rb",
            "the project's own class first, though it sorts last"
        );
    }

    #[test]
    fn signatures_put_rubys_own_classes_and_modules_in_the_chain() {
        // The "a superclass in a gem" case, met with the mechanism that actually
        // delivers it: with signatures indexed, `Object` and `Comparable` come out of real
        // `.rbs` files and the chain reaches all the way up. `module Comparable` in a list of
        // supertypes is the answer's most surprising claim and its most correct one — a
        // linearized chain is what Ruby means by `ancestors`, and filtering modules out to make
        // it look like single inheritance would make it wrong.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/object.rbs"),
            "class BasicObject\nend\n\nmodule Kernel\nend\n\nclass Object < BasicObject\n  \
             include Kernel\nend\n\nmodule Comparable\nend\n\nclass Numeric < Object\n  \
             include Comparable\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let source = "class Money < Numeric\nend\n";
        let uri = harness.write("lib/money.rb", source);
        harness.index();
        harness.index_gems();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Money <"),
            "class Numeric — object.rbs\n\
             module Comparable — object.rbs\n\
             class Object — object.rbs\n\
             module Kernel — object.rbs\n\
             class BasicObject — object.rbs"
        );
        // And the other direction across the same boundary: Ruby's own class knows about the
        // project's, because the reverse index is filled as the chain is linearized.
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, source, "Numeric"),
            "class Money — money.rb"
        );
    }

    #[test]
    fn a_class_reopened_in_two_files_is_one_row_pointing_at_the_users_own_copy() {
        // One row per declaration, not one per definition: `ActiveRecord::Base` is reopened
        // hundreds of times and a chain listing each would be unreadable. Which of them the row
        // points at is `locator::preferred_definition`, shared with the symbol picker so a class
        // cannot open in one file from the outline and in another from the hierarchy.
        let (dir, _gem_home, env) = project_with_gem("module Shouty\n  class Base\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "module Shouty\n  class Base\n    def extra; end\n  end\nend\n";
        let uri = harness.write("lib/reopen.rb", source);
        harness.index();
        harness.index_gems();

        let prepared = harness.prepare_hierarchy(&uri, source, "Base");
        assert_eq!(
            prepared.as_array().map(Vec::len),
            Some(1),
            "reopened in two files, listed once: {prepared}"
        );
        assert!(
            prepared[0]["uri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("/lib/reopen.rb"),
            "the project's copy, not the gem's: {}",
            prepared[0]["uri"]
        );
    }
}
