//! The two hierarchies: `prepareTypeHierarchy` with `supertypes` and `subtypes`, and
//! `prepareCallHierarchy` with `incomingCalls` and `outgoingCalls`.
//!
//! They share [`Item`] and nothing else. A type hierarchy is a fact about declarations rubydex has
//! already computed. A call hierarchy is two different questions about *call sites*, answered very
//! differently (last section).
//!
//! # Both directions show the whole chain
//!
//! Supertypes are `Module#ancestors` minus the class itself, already linearized by rubydex in
//! Ruby's method-lookup order. So modules are in it: `Comparable` is a supertype of `String`, and a
//! *prepended* module sits above the class that prepends it. That is correct Ruby, and filtering it
//! to look like single inheritance would make it wrong.
//!
//! Subtypes mirror that instead of listing one generation: a linearized chain one way and one
//! generation the other would disagree about what a level means. So expanding `Base` lists `Leaf`
//! as well as `Middle`, and expanding `Middle` lists `Leaf` again.
//!
//! # Subtypes are a lookup, not a scan
//!
//! rubydex keeps the reverse index as it linearizes: every namespace carries the set of
//! declarations that resolved *through* it, updated as documents are indexed and dropped. So
//! nothing walks the graph here. The cap bounds the *response*, because `Object` has one subtype
//! per class in the project and its bundle.
//!
//! **The set can name a declaration the graph no longer holds.** Deleting a document removes a
//! class from each ancestor's descendant set, but some ancestors are already cleared when that
//! runs, so a stale id survives: deleting the file that defined `Leaf < Middle < Base` leaves
//! `Leaf` in `Object`'s set. So every id is looked up, never trusted.
//!
//! # What gets no hierarchy
//!
//! Singleton classes and the placeholders rubydex invents for a namespace it never saw, by the same
//! two tests as `search::is_listable`: nobody asked about `class << self` or
//! `<uri>:<offset><anonymous>`. Methods neither: `prepare` answers `null` on one, so the editor
//! says "no results" instead of drawing a tree of the wrong thing. [`prepare_calls`] is its mirror
//! and turns away everything that is *not* a method.
//!
//! # The two call directions are not mirrors
//!
//! **Incoming is a work list.** `references::calls_to` matches a name, because rubydex links no
//! method reference to a declaration. So a caller of `call` is a caller of anything spelled `call`,
//! as in `textDocument/references`, and the row says so in its detail column. A tree implies an
//! exactness a name match lacks, and the footnote keeps them apart.
//!
//! **It lists *calls*, which is narrower than references.** `alias reject! destroy!` writes the
//! name and calls nothing, so the line is a reference, not a caller, and rubydex's two spellings
//! are the only thing in the graph that tells them apart. `references::Spellings` has the argument:
//! this direction takes the bare spelling, `textDocument/references` takes both.
//!
//! **Outgoing claims an edge exists**, which is stronger than "this name appears here". So it is
//! drawn only from a resolution that named the receiver: `locator::precise_call`, the same gate
//! `signatureHelp` uses. Otherwise a call on an untyped receiver would fan out to every method
//! spelled that way. The asymmetry is deliberate: a work list may be over-broad and still useful;
//! an edge may not.
//!
//! **The bucket is a definition, never a declaration.** A class reopened in two files has two
//! bodies, and a row's `fromRanges` are drawn against the row's own file, so bucketing by
//! declaration would put one file's ranges on another's text. It is also why both directions ask
//! the same question: which `def`'s body contains this offset, innermost first. Incoming asks it to
//! find a call's caller; outgoing asks it to *exclude* calls belonging to a nested `def`.
//!
//! **A call inside no `def` is attributed to the file.** A model's `has_many`, a `Rakefile`'s top
//! level, a class body's `include`: dropping those hides most of what a Rails model is made of, and
//! attributing them to the class would claim the class called something, which is not what a
//! call-graph edge means.

use std::collections::{HashMap, HashSet};

use lsp_types::SymbolKind;
use rubydex::model::{
    declaration::{Ancestor, Declaration, Namespace},
    definitions::Definition,
    document::Document,
    graph::Graph,
    ids::{ConstantReferenceId, DeclarationId, DefinitionId, NameId, StringId, UriId},
    name::ParentScope,
};

use super::{
    environment::{self, Trees},
    indexed::{Indexed, Placed},
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
    /// The file the row is written in, or that the name could not be found. Shown beside the name:
    /// a ten-deep chain is mostly gems and Ruby's own signatures, and this separates them from the
    /// project's own entries.
    pub detail: String,
    /// `None` for an ancestor rubydex could not resolve. There is nothing to expand; the row exists
    /// so a superclass from an uninstalled gem is *visible* instead of silently missing from the
    /// chain.
    pub declaration: Option<DeclarationId>,
    pub site: Site,
}

/// What `typeHierarchy/subtypes` found, and how much of it fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtypes {
    pub items: Vec<Item>,
    /// How many subtypes there were before the cap. Above `limit` the answer is truncated, and a
    /// truncated list looks exactly like a complete one.
    pub found: usize,
}

/// One edge of the call graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The caller, for `incomingCalls`; the callee, for `outgoingCalls`.
    pub item: Item,
    /// Every call site, as offsets into the file the calls are *written* in: the row's own file for
    /// an incoming call, the expanded method's file for an outgoing one. The protocol calls these
    /// `fromRanges`, and they are why the answer is one bucket per method, not one row per call: a
    /// method calling another four times is one row with four ranges.
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
/// Every incoming row carries it, because every incoming row is a name match. Two words instead of
/// hover's full sentence, like `NOT_FOUND`: a tree row has no room for prose.
const BY_NAME: &str = "by name";

/// What the detail column says on the file a call outside any method was written in.
const OUTSIDE_A_METHOD: &str = "outside any method";

/// The one document outside the project whose rows are answers to this request.
///
/// **A hierarchy row carries the subject's uri, not the reader's**, which is why
/// [`locator::preferred_definition`] has no cursor to consult. A request rooted in a document the
/// project does not contain is a reader asking about that document, so every row written in it is
/// what they asked for. The two `prepare`s read the cursor's document, and the three expansions
/// read the row the client sent back; in every case a reader can produce, that is the same
/// document.
///
/// `None` for a request rooted inside the project: the ordinary case, where nothing outside is an
/// answer at all. See [`environment::Outward`].
fn rooted_in(uri: Option<&str>, layout: environment::Layout<'_>) -> Option<UriId> {
    environment::fenced_from(uri, layout)
        .outward
        .own_document()
        .map(UriId::from)
}

/// The type the cursor is on, ready for the client to expand.
///
/// `None` (an empty list) for anything that is not a class or module, including every method: a
/// name-based method match resolves to `Declaration::Method`, so nothing here needs to gate on
/// [`locator::Resolution::precise`]. Asking for a namespace rejects it.
#[must_use]
pub fn prepare(
    graph: &Indexed,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    placed: &Placed,
    layout: environment::Layout<'_>,
) -> Vec<Item> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        // As in goto-definition and references: several targets can share the narrowest span, so
        // take the first that has something to say, not the first that exists.
        .find_map(|located| {
            let items: Vec<Item> = locator::resolve(
                graph,
                &located,
                environment::Fence::uses(locator::uri_of(graph, uri_id), layout),
            )
            .declarations
            .into_iter()
            .filter_map(|id| {
                item(
                    graph,
                    synthesized,
                    id,
                    placed,
                    layout,
                    rooted_in(locator::uri_of(graph, uri_id), layout),
                )
            })
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
    placed: &Placed,
    layout: environment::Layout<'_>,
    rooted: Option<&str>,
) -> Vec<Item> {
    let here = rooted_in(rooted, layout);
    let Some(subject) = graph.declarations().get(&declaration).and_then(namespace) else {
        return Vec::new();
    };
    let chain: Vec<Ancestor> = subject.ancestors().iter().copied().collect();

    chain
        .iter()
        // A class is in its own ancestors, and not always first: `prepend`ing a module puts that
        // module above the class, exactly as Ruby reports it. So this drops the entry that is this
        // declaration, not the head of the list.
        .filter(|ancestor| **ancestor != Ancestor::Complete(declaration))
        .filter_map(|ancestor| match ancestor {
            Ancestor::Complete(ancestor) => {
                item(graph, synthesized, *ancestor, placed, layout, here)
            }
            Ancestor::Partial(name) => unresolved(graph, &chain, *name),
        })
        .collect()
}

/// Everything that inherits from `declaration`, the user's own code first and the application's
/// own subclasses ahead of the suite's doubles.
#[must_use]
pub fn subtypes(
    graph: &Graph,
    synthesized: &Synthesized,
    declaration: DeclarationId,
    limit: usize,
    placed: &Placed,
    layout: environment::Layout<'_>,
    rooted: Option<&str>,
) -> Subtypes {
    let here = rooted_in(rooted, layout);
    let (ranked, found) = below(graph, declaration, limit, placed, layout, namespace, |_| {
        true
    });
    Subtypes {
        items: ranked
            .into_iter()
            .filter_map(|id| item(graph, synthesized, id, placed, layout, here))
            .collect(),
        found,
    }
}

/// What is under one namespace, in the order every list of it is shown in.
///
/// **One order, because two would be a disagreement nobody would notice.** `subtypes` draws the
/// whole set and `implementations` draws the part that declares one member. A reader who sees
/// `FakeStore` above `Warehouse` in one list and below it in the other has been told two different
/// things about one fact. So the ranking lives here, and neither caller has its own.
///
/// Ranked before capping, because the reverse index is a hash set: taking the first `limit` in
/// iteration order would return a different arbitrary slice each run, and near the root of the
/// object model the slice is all the user sees. The second term is a **rank, never a drop**; see
/// [`environment`](super::environment) for which surfaces may do which.
///
/// **`admits` runs before ranking, not after**, which makes the second caller affordable: asking
/// `Object` which of its descendants declare `save` is tens of thousands of hash lookups and then
/// `placement` on the few that survive. Ranking first would do both for every descendant.
///
/// `gate` and `admits` are two tests because the subject takes only the first: `Story` includes the
/// concern that declares `save` and declares nothing itself, so filtering the subject by the member
/// test would leave no descendants to ask.
fn below(
    graph: &Graph,
    declaration: DeclarationId,
    limit: usize,
    placed: &Placed,
    layout: environment::Layout<'_>,
    gate: fn(&Declaration) -> Option<&Namespace>,
    admits: impl Fn(&Namespace) -> bool,
) -> (Vec<DeclarationId>, usize) {
    let Some(subject) = graph.declarations().get(&declaration).and_then(gate) else {
        return (Vec::new(), 0);
    };

    // Ranked on what is cheap to read (two flags and the name rubydex holds), and turned into items
    // only for the survivors, as `search::search` does: a class near the root has tens of thousands
    // of descendants, and finding each one's file means reaching into every definition.
    let trees = Trees::of(graph, placed.own(), placed.outside(), layout.names);
    let mut ranked: Vec<(bool, bool, &str, DeclarationId)> = subject
        .descendants()
        .iter()
        // A class is in its own descendants too, for the same reason it is in its own ancestors.
        .filter(|descendant| **descendant != declaration)
        .filter_map(|descendant| {
            let subtype = graph.declarations().get(descendant)?;
            if !admits(gate(subtype)?) {
                return None;
            }
            let placement = environment::placement(graph, subtype, placed.own(), &trees);
            // **Dropped, not sunk: the one case in this list where the verdicts differ.** A
            // subclass under `spec/` really does inherit, and a list read to find out what inherits
            // must keep it; `placement.loadable` sinks it. A subclass declared in a document
            // outside the project is not something this project has at all, so there is nothing to
            // sink it below.
            if !placement.inside {
                return None;
            }
            Some((
                placement.own,
                placement.loadable,
                subtype.name(),
                *descendant,
            ))
        })
        .collect();
    let found = ranked.len();

    // The project ahead of its bundle, for `search::rank`'s reason: `StandardError`'s subtypes in a
    // Rails app are hundreds in gems and a handful of the user's, and an alphabetical list would
    // bury the handful. Then the application ahead of its suite, the same argument one level in: a
    // base class the project subclasses twice may have twenty doubles under `spec/`, and
    // alphabetical order puts `FakeStore` above `Store::Admin`. Sunk, never dropped: the double
    // really inherits. Never ordered by `DeclarationId`, which is a hash and would shuffle between
    // runs.
    ranked.sort_unstable_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(right.1.cmp(&left.1))
            .then(left.2.cmp(right.2))
    });
    ranked.truncate(limit);

    (ranked.into_iter().map(|(_, _, _, id)| id).collect(), found)
}

/// `textDocument/implementation`: where the thing under the cursor is written, and everything below
/// it that writes its own.
///
/// **The definition comes first and is always listed.** Claude Code prints an implementation answer
/// with its *definition* formatter, so an empty answer meaning "nothing overrides this" reaches an
/// agent as *No definition found*: a claim about the server, not the code. TypeScript answers a
/// concrete method with that method for the same reason. It also settles what an un-overridden
/// concern method answers: the concern's own `def`.
///
/// **Below the receiver, never below the declaration.** `story.save` is declared by
/// `ActiveRecord::Persistence`, whose descendants are every model, so an override list taken from
/// the declaration answers a `Story` with `User#save`. [`locator::Resolution::receiver`] carries
/// the class the call was really about. Where it carries nothing, the declaration's owner is asked
/// instead: the right question at a `def`, and the widest honest one at an untyped bare call.
///
/// **A constant is the same question one level up**: the namespace's descendants, as places instead
/// of a tree. In rubydex a module's includers and a class's subclasses are one set, so they are one
/// answer here.
///
/// The caller decides whether the cursor may ask at all (a guessed receiver is refused in
/// [`requests`](super::requests), where the tier is known); this draws what it is handed.
///
/// The tree names come from the `layout`, not beside it, for `Fence`'s reason in
/// [`environment`](super::environment): a list that knew where the project is but not what its
/// trees are called would rank a double as application code. The two hierarchy entry points take
/// the names separately because they take no layout.
#[must_use]
pub fn implementations(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    resolution: &locator::Resolution,
    cursor: Option<&str>,
    limit: usize,
    placed: &Placed,
) -> Vec<Site> {
    let mut places = locator::all_places(
        graph,
        synthesized,
        layout,
        resolution.declarations.iter().copied(),
        cursor,
    );

    let mut overrides: Vec<DeclarationId> = Vec::new();
    for id in &resolution.declarations {
        let Some(declaration) = graph.declarations().get(id) else {
            continue;
        };
        match declaration {
            Declaration::Namespace(_) => {
                overrides.extend(below(graph, *id, limit, placed, layout, reachable, |_| true).0);
            }
            Declaration::Method(_) => {
                let member = StringId::from(member_key(declaration));
                let subject = resolution.receiver.unwrap_or(*declaration.owner_id());
                // **The member test runs twice, and neither copy is dead.** Inside `below`, it runs
                // before ranking, which is where the cap is decided: rank first, and `Object`'s
                // descendants would fill every row with classes that declare nothing, pushing the
                // real overrides off the end. Here, it projects a descendant to the member *it*
                // declares, which is the id a place is taken from. Each also filters, so disabling
                // either one still keeps a non-overriding subclass out; only disabling both lets
                // one in.
                let (overriding, _) = below(
                    graph,
                    subject,
                    limit,
                    placed,
                    layout,
                    reachable,
                    |descendant| descendant.member(&member).is_some(),
                );
                overrides.extend(overriding.iter().filter_map(|id| {
                    reachable(graph.declarations().get(id)?)?
                        .member(&member)
                        .copied()
                }));
            }
            _ => {}
        }
    }

    // Deduplicated against the definition's own places, not appended blind. A method whose owner is
    // one of its own descendants (a module that includes something that includes it) would
    // otherwise be listed twice, and `Found 2 definitions` of one `def` is an overstatement. Keyed
    // on the name span, like `locator::sites`: one `def` that two generators both named comes back
    // with two different `full` spans.
    let seen: HashSet<(String, (u32, u32))> = places
        .iter()
        .map(|place| (place.uri.clone(), place.selection))
        .collect();
    places.extend(
        locator::all_places(graph, synthesized, layout, overrides, cursor)
            .into_iter()
            .filter(|place| !seen.contains(&(place.uri.clone(), place.selection))),
    );
    places
}

/// rubydex's key for a method member, read off a declaration's own name.
///
/// The parenthesised spelling (`shout()`), because that is what a namespace files its members
/// under; see [`locator`]'s `member_name`, which reaches the same string from the other side.
///
/// The whole name where there is no `#`, which no `Declaration::Method` lacks: a key nothing is
/// filed under finds no members and produces no rows, the same answer an unreachable branch would
/// give. `references::method_names` splits the name the same way.
fn member_key(declaration: &Declaration) -> &str {
    let name = declaration.name();
    name.rsplit_once('#').map_or(name, |(_, member)| member)
}

/// The namespace a declaration is, when it is one a person asked about.
///
/// Both exclusions are `search::is_listable`'s, and they are two tests because they catch different
/// things: `SingletonClass` is rubydex's `class << self`, and the name test also rejects an
/// anonymous `Class.new`, a real class with no name to show. `Todo` is the placeholder for a
/// namespace rubydex never saw a definition of.
fn namespace(declaration: &Declaration) -> Option<&Namespace> {
    let namespace = declaration.as_namespace()?;
    (!matches!(namespace, Namespace::SingletonClass(_) | Namespace::Todo(_))
        && render::is_nameable(declaration.name()))
    .then_some(namespace)
}

/// Every namespace a *place* can be found under: wider than [`namespace`] by exactly the two it
/// turns away.
///
/// [`namespace`] is the hierarchy's gate, about rows in a tree: nobody wrote `class << self` as a
/// type, and an anonymous `Class.new` has no name for a column. `implementation` shows no names,
/// only places, and both of those are real receivers. `Shape.build` is a call on `Shape::<Shape>`,
/// and rubydex linearizes the singleton chain beside the instance one, so a subclass's
/// `def self.build` is simply a descendant's member.
///
/// `Todo` stays out, by kind, not spelling: it stands for a namespace no file defines, so it has no
/// members and no descendants.
fn reachable(declaration: &Declaration) -> Option<&Namespace> {
    let namespace = declaration.as_namespace()?;
    (!matches!(namespace, Namespace::Todo(_))).then_some(namespace)
}

/// One row for a declaration the graph holds.
///
/// `None` when there is nowhere to point: no definitions, or only one in rubydex's synthetic
/// built-in document. The second is why `Object`, `Kernel` and `BasicObject` vanish from a chain
/// when signatures are off and appear when they are on: `DocUri::from_graph_uri` rejects
/// `rubydex:built-in` for every request, and a row an editor cannot open is worse than no row.
fn item(
    graph: &Graph,
    synthesized: &Synthesized,
    id: DeclarationId,
    placed: &Placed,
    layout: environment::Layout<'_>,
    here: Option<UriId>,
) -> Option<Item> {
    let declaration = graph.declarations().get(&id)?;
    namespace(declaration)?;
    let definition = locator::preferred_definition(
        graph,
        id,
        placed.own(),
        placed.outside(),
        layout.names,
        here,
    )?;
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
/// Shown, not dropped: a superclass in a gem the bundle did not install would otherwise leave a
/// chain that reads as complete but is missing everything above the gap.
///
/// It is placed where the name is *written*, which may not be the class being expanded: an
/// unresolved superclass propagates down, so `Leaf`'s chain carries the `Missing::Thing` that
/// `Middle` inherits from. So the whole chain is searched for the mention, and the kind comes from
/// how it was written: a superclass is a class, a mixin is a module.
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
/// Every partial in a linearized chain came from one of the chain's own members (nothing beyond an
/// unresolved name is reachable through it), so this searches only the members.
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
                // Unreachable today, but required by the enum's other fourteen kinds, which are
                // members, not namespace bodies. Everything in a linearized chain is a class or
                // module, and rubydex promotes even `Wrapper = Class.new` to a class before it can
                // be anybody's ancestor.
                _ => return None,
            };
            // The superclass first and separately, because its kind is a class. One unresolved name
            // written both ways (`class A < Foo` in one file, `include Foo` in another) cannot
            // happen: the two spell different `Name`s only when their nesting differs, and then
            // they are two rows.
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
                            // The name as written is the whole row, so both spans are one span,
                            // which satisfies the protocol's rule that the selection sits inside
                            // the range.
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
/// Built by walking parent scopes instead of recursing, since a constant path can be as deep as the
/// file wrote it. The explicit root is kept because `::Foo` failing where `Foo` would resolve is
/// often the reason, and this row exists to be read.
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

/// The last segment of a document URI: the file name.
///
/// Taken from the URI, not a path: this runs once per row of a list at most a few hundred long, and
/// going through `DocUri` would parse a `Url` per row for a string that is only read. Percent
/// escapes are left alone: the column is a hint beside a name, not a link.
fn file_name(uri: &str) -> String {
    uri.rsplit('/').next().unwrap_or(uri).to_owned()
}

/// The method the cursor is on, ready for the client to expand.
///
/// A call site answers as readily as a `def`, because pressing "show call hierarchy" on a call is
/// the usual way in, and `locate` handles both. Everything that is not a method answers `null`, the
/// mirror of [`prepare`], for the same reason: a tree of the wrong thing is worse than "no
/// results".
#[must_use]
pub fn prepare_calls(
    graph: &Indexed,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    placed: &Placed,
    layout: environment::Layout<'_>,
) -> Vec<Item> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        // As in goto-definition and `prepare`: several targets can share the narrowest span.
        .find_map(|located| {
            let items: Vec<Item> = locator::resolve(
                graph,
                &located,
                environment::Fence::uses(locator::uri_of(graph, uri_id), layout),
            )
            .declarations
            .into_iter()
            .filter_map(|id| {
                method_item(
                    graph,
                    synthesized,
                    id,
                    placed,
                    layout,
                    rooted_in(locator::uri_of(graph, uri_id), layout),
                )
            })
            .collect();
            (!items.is_empty()).then_some(items)
        })
        .unwrap_or_default()
}

/// Every call of `declaration`, bucketed by the method each call is written in.
///
/// Deliberately unranked: rows come in the order the calls are read, file by file and then down
/// each file, which is `references`' order and needs no defending. The cap is on rows for
/// `MAX_SUBTYPES`' reason: finding them is a scan either way, but *drawing* one reads its file.
///
/// **An `alias` line is not a call.** [`references::calls_to`] asks for the one spelling a call is
/// recorded under. The names a rename must visit and the names something actually called are two
/// lists, and this is the narrower.
#[must_use]
pub fn incoming(
    graph: &Graph,
    synthesized: &Synthesized,
    declaration: DeclarationId,
    limit: usize,
    scope: &HashSet<UriId>,
) -> Calls {
    let references = references::calls_to(graph, declaration, scope);

    // A `HashMap` alone would order the same rows differently each run, and behind a cap a
    // different order is a different *answer*. So the map holds positions into a list that keeps
    // arrival order.
    let mut rows: Vec<(Caller, Vec<(u32, u32)>)> = Vec::new();
    let mut seen: HashMap<Caller, usize> = HashMap::new();

    // Grouped by file, so each document's `def`s are collected once per file, not once per call
    // site. `calls_to` has already sorted by uri and offset.
    for group in references.chunk_by(|left, right| left.uri == right.uri) {
        let uri_id = UriId::from(group[0].uri.as_str());
        // The uri came from `graph.documents()` via `calls_to`, so a miss here just yields nothing;
        // there is nothing to handle. The same shape, for the same reason, as the two loops in
        // `references`.
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
/// **Addressed by position, not declaration**, the one place this direction differs from the rest
/// of the file. A row the client expands names one body (the file and span it was drawn from), and
/// a method reopened in two files has two bodies with different calls. A declaration would have to
/// pick one, and would pick wrong for every incoming-call row, which points at the body the calls
/// were *found* in, not whichever body `preferred_definition` prefers.
///
/// No cap, on purpose: the population is the distinct calls inside one hand-written `def`. Incoming
/// calls are bounded only by the workspace.
///
/// **Only a call whose receiver was named produces a row**: [`locator::precise_call`], the gate
/// `signatureHelp` fires from. The alternative, the name rung, would answer `person.name` with
/// every `name` method in the graph. The redirect is kept, so `Foo.new` is an edge to
/// `Foo#initialize`: that is the method that runs.
#[must_use]
pub fn outgoing(
    graph: &Indexed,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    placed: &Placed,
    // **The names ride on the layout, not as a second argument**, as `environment::Layout`
    // documents at its `names` field: a fencing surface holds one value, so its halves cannot come
    // apart.
    layout: environment::Layout<'_>,
    blocks: &locator::Blocks<'_>,
) -> Vec<Call> {
    // A file the graph does not hold: written since the last settle, or excluded from the index
    // after the client got this item. Nothing to expand and no error to raise.
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };
    let bodies = method_bodies(graph, document);
    // Not inside any `def`. The file row an incoming call produces is exactly this, and expanding
    // it honestly answers "nothing".
    let Some(body) = containing(&bodies, offset) else {
        return Vec::new();
    };

    let mut sites: Vec<(u32, u32)> = document
        .method_references()
        .iter()
        .filter_map(|id| graph.method_references().get(id))
        .map(|reference| (reference.offset().start(), reference.offset().end()))
        // The cheap bound first, because most of a file's calls are outside any one body. Only the
        // start is compared: a reference starting inside a body ends inside it, so testing the end
        // would be an arm no input can take.
        .filter(|(start, _)| *start >= body.offset().start() && *start < body.offset().end())
        // A call inside a nested `def` was made by that `def`. The containment question
        // `incoming` asks to *find* a caller, asked here to exclude one.
        .filter(|(start, _)| containing(&bodies, *start).map(Definition::id) == Some(body.id()))
        .collect();
    // Source order, so the rows read the way the body does. rubydex's reference list is per
    // document, with no promised order inside it.
    sites.sort_unstable();

    let mut rows: Vec<(DeclarationId, Vec<(u32, u32)>)> = Vec::new();
    let mut seen: HashMap<DeclarationId, usize> = HashMap::new();
    for (start, end) in sites {
        // **No cursor, so no syntax to read.** This walks every call site in a body instead of
        // answering at one, and a row saying a body calls a private method is true whether or not
        // Ruby allows that spelling. See `locator::Privacy`.
        let Some(callee) = locator::precise_call(
            graph,
            uri_id,
            start,
            layout,
            locator::Privacy::Allowed,
            blocks,
        ) else {
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
                item: method_item(
                    graph,
                    synthesized,
                    callee,
                    placed,
                    layout,
                    rooted_in(locator::uri_of(graph, uri_id), layout),
                )?,
                ranges,
            })
        })
        .collect()
}

/// Who a call site is attributed to.
///
/// A definition, not a declaration, for the reason in this module's comment: a row's ranges are
/// drawn against the row's own file, and a reopened class has bodies in more than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Caller {
    Method(DefinitionId),
    File(UriId),
}

/// One row for a method declaration, pointing at the definition the rest of the server prefers.
///
/// `None` for anything that is not a method, which lets `prepare_calls` decline a class without a
/// second test.
fn method_item(
    graph: &Graph,
    synthesized: &Synthesized,
    id: DeclarationId,
    placed: &Placed,
    layout: environment::Layout<'_>,
    here: Option<UriId>,
) -> Option<Item> {
    let declaration = graph.declarations().get(&id)?;
    matches!(declaration, Declaration::Method(_)).then_some(())?;
    let definition = locator::preferred_definition(
        graph,
        id,
        placed.own(),
        placed.outside(),
        layout.names,
        here,
    )?;
    row(graph, synthesized, declaration.name(), definition, Some(id))
}

/// One row for the method a call was written inside.
///
/// Built from the definition, not looked up again, so the row points at *this* body (the one the
/// ranges are in), not whichever body `preferred_definition` would pick. The declaration still
/// rides in `data`, because expanding the row asks about the method, not one of its bodies.
fn caller_item(graph: &Graph, synthesized: &Synthesized, definition: &Definition) -> Option<Item> {
    let id = *graph.definition_to_declaration_id(definition)?;
    let declaration = graph.declarations().get(&id)?;
    let mut item = row(graph, synthesized, declaration.name(), definition, Some(id))?;
    item.detail = format!("{} — {BY_NAME}", item.detail);
    Some(item)
}

/// One row for a file, for a call written outside every `def` in it.
///
/// The span is the start of the file: there is no construct to point at, and a range over the whole
/// file would make an editor select all of it on click. `declaration` is `None`, so expanding it
/// yields nothing, which is true: a file has no callers.
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
        // Qualified (`Person#shout`, not `shout`), because a call tree is read across files.
        // `render::simple_name` only strips rubydex's parentheses.
        name: render::simple_name(name).to_owned(),
        kind: symbols::kind_of(definition),
        detail: file_name(&site.uri),
        declaration,
        site,
    })
}

/// Every `def` in one document: every span a call site can be attributed to.
///
/// Methods only. An `attr_reader` declares one but has no body for a call to be inside, and a class
/// or module body is deliberately not a caller (see this module's comment).
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
/// Innermost is the greatest start offset among those containing it: a nested definition starts
/// after its parent and ends before it, so nothing else needs comparing. Linear in the file's
/// `def`s, which is why the caller collects them once per file, not once per call.
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
        // Not hypothetical, and not the case `locator` tests: the id comes from the *client*, which
        // echoes back whatever the expanded item carried. A rubydex id is a 64-bit hash of the
        // name, so it survives a config reload, but the declaration it names may not, and
        // descendant sets keep ids of deleted documents too.
        let graph = Graph::new();
        let nowhere = DeclarationId::new(1_234_567_890_123_456_789);
        let placed = Placed::of(&graph, |_| false, environment::Layout::default());

        let synthesized = Synthesized::new();

        assert!(
            supertypes(
                &graph,
                &synthesized,
                nowhere,
                &placed,
                environment::Layout::default(),
                None
            )
            .is_empty()
        );
        assert_eq!(
            subtypes(
                &graph,
                &synthesized,
                nowhere,
                10,
                &placed,
                environment::Layout::default(),
                None
            ),
            Subtypes {
                items: Vec::new(),
                found: 0
            }
        );
    }

    #[test]
    fn the_namespaces_a_person_never_wrote_are_not_types() {
        // Two tests, because they catch different things, asserted here and not only through a
        // cursor: which declarations get a hierarchy is a rule, and a rule reached by accident
        // through whatever `locate` returns at a `~` can silently stop being tested.
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

        // The placeholder for a namespace rubydex never saw (`Foo::Bar` mentioned with no `Foo`),
        // and a `Class.new` with nothing to call it, a real class with no name to show. The second
        // is why the name is tested as well as the kind.
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
        // `spell` walks interned strings the resolver filed, so every step can miss. Its row is the
        // one saying a superclass could not be found, and a row with no name says less than no row.
        let graph = Graph::new();
        assert_eq!(spell(&graph, NameId::new(987_654_321)), None);
    }

    #[test]
    fn the_detail_column_is_the_file_name_however_the_uri_is_spelled() {
        // The file name is what separates a ten-deep chain's two project rows from its eight
        // others, so it must be right for the shapes rubydex's document keys really take.
        assert_eq!(file_name("file:///w/app/models/user.rb"), "user.rb");
        assert_eq!(file_name("file:///user.rb"), "user.rb");
        // rubydex's synthetic document never reaches a client (`DocUri` rejects it), but it reaches
        // here first and must not show as an empty column.
        assert_eq!(file_name("rubydex:built-in"), "rubydex:built-in");
    }

    /// A three-deep chain, a module included halfway up, a module prepended at the bottom, a
    /// sibling, and a superclass nothing can resolve.
    ///
    /// One file, so fixture and answers can be read side by side. The prepend matters: it is the
    /// one shape where the class is *not* first in its own ancestor chain, so dropping "the head of
    /// the list" instead of "the entry that is this class" would pass every other test here.
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
        // The whole list, not a `contains`: this is `Module#ancestors`, and what matters is its
        // *composition*. `Loud` above `Leaf` because a prepended module wins method lookup, `Greet`
        // between `Middle` and `Base` because that is where it was included, and `Leaf` nowhere: a
        // class is in its own ancestors but is not its own supertype.
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
        // Every Ruby class inherits from `Object`, `Kernel` and `BasicObject`, and rubydex knows it
        // without signatures, via a built-in document called `rubydex:built-in` with no file behind
        // it. `DocUri` rejects that URI for every request, so the rows are dropped instead of sent
        // somewhere an editor cannot open. With signatures indexed they come back from `core/*.rbs`
        // (next test).
        let (mut harness, uri) = hierarchy_harness();
        let rows = harness.hierarchy_rows("typeHierarchy/supertypes", &uri, HIERARCHY, "Base\nend");
        assert!(!rows.contains("Object"), "{rows}");
        assert!(!rows.contains("Kernel"), "{rows}");
    }

    #[test]
    fn the_subtypes_of_a_class_are_every_class_below_it_and_not_just_the_next_one() {
        // The mirror of the supertypes above, and transitive to match: `Leaf` is two levels under
        // `Base` and is listed, because a chain one way and a single generation the other would
        // disagree about what a level means. `Base` itself is not listed, nor is `Orphan`, which
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
        // Both halves. The double is **listed** (it really inherits, and this list is read to find
        // out what does) and listed **last**, under every subclass the application loads:
        // `MAX_SUBTYPES` is spent from the top, and a base class with two real subclasses often has
        // twenty doubles. Alphabetically, `FakeStore` would sort above `Warehouse`.
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
        // The other side of the rule, and the half with teeth: `incomingCalls` answers *where is
        // this used*, and a use under `spec/` is a use. Omitting the suite would make a refactor
        // that breaks it; that is `references`' argument, and why `environment` names this surface
        // as one that must never fence.
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
        // `include` and `prepend` both put a class into a module's descendants, which makes "who
        // uses this concern" a hierarchy question. `Greet` is included by `Middle` and reaches
        // `Leaf` through it.
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
        // The silent-degradation case, and why the partial arm is not just dropped: a superclass in
        // an uninstalled gem would otherwise leave a chain that reads as complete but is short by
        // everything above the gap. The row is spelled as written, its kind comes from being
        // written as a superclass, and it carries no `data`: there is nothing to expand.
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
        // A partial propagates down the chain: `Cursed`'s ancestors carry the `Missing::Thing` that
        // `Orphan` inherits from, written in `Orphan`. So the whole chain is searched for the
        // mention, not only the class being expanded; otherwise the row is missing or points at the
        // wrong line, and the row exists to be clicked.
        //
        // `Cursed` has an unresolved mixin of its own, so the chain holds *two* unresolved names.
        // That makes the search step over one partial on its way to another's mention: the ordinary
        // case when a gem is missing, which a single unresolved name never reaches.
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
        // definition, not the enum, so a module in the chain is its own case. A concern including a
        // missing concern is a real Rails shape.
        let source = "module Bag\n  include Gone::Bits\nend\n\nclass Holder\n  \
                      include Bag\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/bag.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Bag\n  include"),
            "module Gone::Bits — not found"
        );
        // And through the class that includes it, where propagation and the module case meet.
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Holder"),
            "module Bag — bag.rb\n\
             module Gone::Bits — not found"
        );
    }

    #[test]
    fn a_mixin_that_did_not_resolve_is_a_module_and_a_superclass_is_a_class() {
        // The graph says nothing about what an unresolved name *was*, but the source does:
        // `include` takes a module and `<` takes a class. Otherwise both rows would have to guess,
        // and a wrong guess puts the wrong icon on the one row that is about something missing.
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
        // `::Foo` failing where `Foo` would have resolved is often the reason, and this row is read
        // rather than clicked, so the name is all it offers.
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
        // Two of `locator`'s three targets reach here: the `class Leaf` definition and the `Middle`
        // after the `<`, a constant reference. Users right-click both, and both must answer with
        // the class.
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
        // `null`, not an empty list, so the editor says there are no results instead of opening an
        // empty tree. A method is the case that matters: `references` matches a method by name, so
        // a cursor on `shout` resolves to declarations. They are just not types, and asking the
        // resolution for a namespace rejects them.
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
        // rubydex models `class << self` as a namespace called `Person::<Person>`, with a real
        // ancestor chain (`Class`, `Module`, `Object`). Nobody wrote that name, so it is rejected
        // by the symbol picker's two tests, and a cursor on `self` there answers `null` instead of
        // opening a tree over ya-lsp's own spelling.
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
        // carried. A row for an unresolved name has no `data`, a client may send a non-number, and
        // a rubydex id is a 64-bit hash, so a stale id looks like a live one and must answer
        // "nothing" instead of resolving against whatever it collides with.
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
    /// `name + name` is deliberate: two calls of one method from one body make a row a bucket, not
    /// a list, and the `+` between them is a call on an untyped receiver. `Siren#greet` is spelled
    /// like `Person#greet` and unrelated to it: the name match this feature rests on, written where
    /// the tests can see it.
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
        // Both are how the command is reached (the cursor is as often on a call as on the `def`),
        // and `locate` handles both. The item must be the same either way, because the client sends
        // it back and the follow-ups read it.
        let (mut harness, uri) = calls_harness();

        let from_def = harness.prepare_calls(&uri, CALL_SITES, "greet\n    name");
        let from_call = harness.prepare_calls(&uri, CALL_SITES, "greet\n  end\nend\n\nclass Siren");
        assert_eq!(from_def[0]["name"], "Person#greet");
        assert_eq!(from_def, from_call, "the same method, prepared two ways");
    }

    #[test]
    fn incoming_calls_are_bucketed_by_the_method_each_call_is_written_in() {
        // The whole answer, drawn: one row per caller with every call site it holds, in file order.
        // `Person#label` is a name match and says so: `Siren#greet` is spelled the same and this
        // list cannot tell them apart. `references` makes the same trade; a *tree* hides it unless
        // the row admits it.
        //
        // The second row is the call in `class Report`'s body, inside no `def`: attributed to the
        // file, not dropped, because a Rails model's macros all have that shape and dropping them
        // would empty the most useful answer.
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
        // `name` twice from one body is one row with two ranges. `+` is a call on whatever `name`
        // returned, which nothing typed, so it is not an edge. A name-based fallback would link `+`
        // to every `+` in the graph, which is why this direction has none.
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

        // The body *after* calls that are not its own: the ordinary shape, and proof the span test
        // does real work. `name` is written twice above `label` and belongs to `greet`, and `greet`
        // resolves to `Person`'s, not the same-spelled `Siren#greet` further down.
        assert_eq!(
            harness.call_rows("callHierarchy/outgoingCalls", &uri, CALL_SITES, "label"),
            "Person#greet — calls.rb [10:4-9]"
        );
    }

    #[test]
    fn an_alias_writes_the_name_down_and_is_not_an_incoming_call() {
        // The other half of `references`' own test: `alias yell shout` names `shout` and calls
        // nothing, so it belongs in a work list, not a tree of callers. rubydex records it under
        // the parenthesised spelling, the only thing in the graph that tells the two apart.
        // `references::Spellings` makes the choice, and this list takes the narrower half.
        let source = "\
class Person
  def shout
    \"!\"
  end

  def announce
    shout
  end

  alias yell shout
  alias_method :holler, :shout
end
";
        let mut harness = Harness::new();
        let uri = harness.write("lib/hr.rb", source);
        harness.index();

        assert_eq!(
            harness.call_rows("callHierarchy/incomingCalls", &uri, source, "shout\n  end"),
            "Person#announce — hr.rb — by name [6:4-9]"
        );
    }

    #[test]
    fn a_call_in_a_nested_def_belongs_to_the_nested_def_in_both_directions() {
        // `def` inside `def` is legal Ruby, and the one shape where "the innermost body containing
        // this offset" differs from "the body this is written in". Incoming must attribute the call
        // to `inner`; outgoing must leave it out of `wrapper`. Same question from the other side,
        // answered by the same function.
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
        // The mirror of what `prepareTypeHierarchy` declines. A class answers `null` so the editor
        // says "no results" instead of drawing a tree of the wrong thing, and a cursor on a string
        // literal is the usual accidental press.
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
        // An ordinary method name reaches the cap (a `call` or a `name` has callers in every file),
        // so a short caller list is both likely and indistinguishable from a complete one. Said out
        // loud, like the other two.
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
        // Two items a client can legitimately send back, neither with calls to list. The file row
        // is one this server produced (every incoming answer over a Rails model has one), and "what
        // does a file call" has no answer. The second is a file written since the last settle: on
        // disk for `with_text` to read, unseen by the graph, which is what a stale item looks like
        // after a rebuild.
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
        // Client input, like every handler's. The follow-ups take an item, not a position, and
        // `outgoingCalls` reads a uri and range from it that `incomingCalls` never touches, so a
        // garbage item must be survivable in two different ways.
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
        // Client input, like every handler's params. All three arms, because each parses a
        // different shape and the two follow-ups take no position.
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
        // A short subtype list is indistinguishable from a complete one, so hitting the cap is said
        // out loud, not only logged. Reached here with a class that has more subtypes than the
        // answer holds; in a real project that means asking near the root of the object model.
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
        // `search::rank`'s decision, for its reason: a widely subclassed class in a real project
        // has hundreds of subtypes in gems and a handful of the user's, and alphabetical order
        // would bury the handful under whatever the bundle spells with an `A`. Within each half the
        // order is by name, never `DeclarationId`, which is a hash: a tree that reshuffles between
        // runs is unreadable.
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
        // The "superclass in a gem" case, with the mechanism that really delivers it: with
        // signatures indexed, `Object` and `Comparable` come from real `.rbs` files and the chain
        // reaches the top. `module Comparable` in a supertype list is the answer's most surprising
        // claim and its most correct: a linearized chain is what Ruby means by `ancestors`.
        let mut harness = signed(
            &[(
                "core/object.rbs",
                "class BasicObject\nend\n\nmodule Kernel\nend\n\nclass Object < BasicObject\n  \
             include Kernel\nend\n\nmodule Comparable\nend\n\nclass Numeric < Object\n  \
             include Comparable\nend\n",
            )],
            "",
        );
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
        // project's, because the reverse index fills as the chain is linearized.
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, source, "Numeric"),
            "class Money — money.rb"
        );
    }

    #[test]
    fn a_class_reopened_in_two_files_is_one_row_pointing_at_the_users_own_copy() {
        // One row per declaration, not per definition: `ActiveRecord::Base` is reopened hundreds of
        // times, and a row each would be unreadable. `locator::preferred_definition` picks which
        // one the row points at, shared with the symbol picker so a class cannot open in one file
        // from the outline and another from the hierarchy.
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

    // -- `textDocument/implementation` -------------------------------------------------------
    //
    // The fixture: one base class, two subclasses that override its method, one that does not, and
    // a fourth override under `spec/`. Every rule of the answer can be read off it, and each test
    // below checks one.

    const SHAPE: &str =
        "class Shape\n  def area\n    0\n  end\n\n  def self.build\n    new\n  end\nend\n";
    const SQUARE: &str =
        "class Square < Shape\n  def area\n    1\n  end\n\n  def self.build\n    new\n  end\nend\n";
    const CIRCLE: &str = "class Circle < Shape\n  def area\n    3\n  end\nend\n";
    const POINT: &str = "class Point < Shape\nend\n";

    fn shapes() -> Harness {
        let harness = Harness::new();
        harness.write("lib/shape.rb", SHAPE);
        harness.write("lib/square.rb", SQUARE);
        harness.write("lib/circle.rb", CIRCLE);
        harness.write("lib/point.rb", POINT);
        harness
    }

    #[test]
    fn the_definition_comes_first_and_every_override_below_the_receiver_follows_it() {
        let mut harness = shapes();
        let source = "shape = Shape.new\nshape.area\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        // `shape.rb` first because it is what `definition` answers, then the two overrides in
        // `subtypes`' order. `point.rb` is a subclass that declares nothing, so it implements
        // nothing.
        assert_eq!(
            harness.implementation_list(&caller, source, "area"),
            ["shape.rb:1:6", "circle.rb:1:6", "square.rb:1:6"]
        );
    }

    #[test]
    fn a_method_nothing_below_it_overrides_answers_with_its_own_def_rather_than_nothing() {
        // The decision the whole answer hangs off. Claude Code prints an implementation answer with
        // its *definition* formatter, so `null` here reaches an agent as "No definition found. This
        // may occur if ... the definition is in an external library not indexed by the LSP server":
        // a claim about the server, from a server that knows exactly where the method is.
        let mut harness = shapes();
        let source = "point = Point.new\npoint.area\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.implementation_list(&caller, source, "area"),
            ["shape.rb:1:6"]
        );
    }

    #[test]
    fn the_overrides_are_below_the_receiver_and_not_below_whatever_declared_the_method() {
        // `story.save` is declared by `ActiveRecord::Persistence`, whose descendants are every
        // model, so a list taken from the *declaration* answers a `Story` with `User#save`. Written
        // small here: `Persist` for the concern, `User` for the model that must not appear.
        let mut harness = Harness::new();
        harness.write(
            "lib/persist.rb",
            "module Persist\n  def save\n    0\n  end\nend\n",
        );
        harness.write("lib/story.rb", "class Story\n  include Persist\nend\n");
        harness.write(
            "lib/user.rb",
            "class User\n  include Persist\n  def save\n    1\n  end\nend\n",
        );
        harness.write(
            "lib/draft.rb",
            "class Draft < Story\n  def save\n    2\n  end\nend\n",
        );
        let source = "story = Story.new\nstory.save\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        // `User#save` really does override `Persist#save`, and it is not an implementation of
        // anything a `Story` could reach.
        assert_eq!(
            harness.implementation_list(&caller, source, "save"),
            ["persist.rb:1:6", "draft.rb:1:6"]
        );
    }

    #[test]
    fn a_guessed_receiver_is_offered_no_implementations_at_all() {
        // The name rung answers a list of same-spelled declarations, and every override below each
        // is the same guess multiplied. A jump has no room for the footnote that would say so
        // (`hints`' margin argument, on a second surface). `hover` at this cursor still answers,
        // and still says it is guessing.
        let mut harness = shapes();
        let source = "thing = something\nthing.area\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert!(
            harness.implementation_at(&caller, source, "area").is_null(),
            "a guess is not an implementation"
        );
        let markdown = harness.hover_at(&caller, source, "area")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Guessed from name alone"), "{markdown}");
        assert!(
            harness
                .candidates_at(&caller, source, "area")
                .contains(&"Shape#area".to_owned()),
            "{markdown}"
        );
    }

    #[test]
    fn a_def_is_asked_below_its_own_owner() {
        // No receiver to read, so the declaration's owner is the subject, which at a `def` is the
        // class the cursor is in.
        let mut harness = shapes();
        let shape = harness.write("lib/shape.rb", SHAPE);
        harness.index();

        assert_eq!(
            harness.implementation_list(&shape, SHAPE, "area"),
            ["shape.rb:1:6", "circle.rb:1:6", "square.rb:1:6"]
        );
    }

    #[test]
    fn a_singleton_method_is_asked_below_the_singleton_and_finds_the_subclasses_own() {
        // rubydex attributes `def self.build` to `Shape::<Shape>`, so the subject is a singleton
        // class and its descendants are the subclasses' singletons. Nothing in the answer needs to
        // know: it is the same lookup, one namespace over.
        let mut harness = shapes();
        let source = "Shape.build\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.implementation_list(&caller, source, "build"),
            ["shape.rb:5:11", "square.rb:5:11"]
        );
    }

    #[test]
    fn a_constructor_is_asked_below_the_class_being_constructed() {
        // `Vault.new` answers with `Vault#initialize`, an *instance* method, so what is below it is
        // below `Vault`. Not below `Vault::<Vault>`, the singleton the call was written on, where a
        // subclass's `initialize` is not a member; and not below the declaration's owner, which for
        // a class without its own `initialize` is `Object`.
        let mut harness = Harness::new();
        harness.write(
            "lib/vault.rb",
            "class Vault\n  def initialize(name)\n    @name = name\n  end\nend\n",
        );
        harness.write(
            "lib/safe.rb",
            "class Safe < Vault\n  def initialize(name)\n    super\n  end\nend\n",
        );
        let source = "Vault.new(\"x\")\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.implementation_list(&caller, source, "new(\"x\")"),
            ["vault.rb:1:6", "safe.rb:1:6"]
        );
    }

    #[test]
    fn a_subclass_that_overrides_nothing_is_a_subclass_and_not_an_implementation() {
        // The other half of the first test, on its own: `Point` inherits `area` and declares none,
        // so it is in `typeHierarchy/subtypes` and in no list here.
        let mut harness = shapes();
        let source = "shape = Shape.new\nshape.area\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let found = harness.implementation_list(&caller, source, "area");
        assert!(
            !found.iter().any(|place| place.starts_with("point.rb")),
            "{found:?}"
        );
    }

    #[test]
    fn a_constant_answers_with_what_is_below_the_namespace() {
        // The same question one level up, and the one place this answers what `subtypes` would: in
        // rubydex a module's includers and a class's subclasses are one set, and this list gives
        // places instead of a tree.
        let mut harness = shapes();
        let source = "Shape.new\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.implementation_list(&caller, source, "Shape"),
            [
                "shape.rb:0:6",
                "circle.rb:0:6",
                "point.rb:0:6",
                "square.rb:0:6"
            ]
        );
    }

    #[test]
    fn the_suites_own_subclass_is_ranked_under_the_applications_and_never_dropped() {
        // `subtypes`' rule, because both lists read one ranking. A double really does override the
        // method, and this list is read to find out what does.
        let mut harness = shapes();
        harness.write(
            "spec/support/fake_shape.rb",
            "class FakeShape < Shape\n  def area\n    9\n  end\nend\n",
        );
        let source = "shape = Shape.new\nshape.area\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.implementation_list(&caller, source, "area"),
            [
                "shape.rb:1:6",
                "circle.rb:1:6",
                "square.rb:1:6",
                "fake_shape.rb:1:6"
            ]
        );
    }

    #[test]
    fn a_subclass_written_outside_the_project_is_dropped_where_the_suites_is_only_sunk() {
        // The two verdicts this list carries, side by side. A double under `spec/` really inherits,
        // so it sinks. A class in a document open **beside** the project is not the project's at
        // all, so there is nothing to sink it below.
        let mut harness = shapes();
        harness.write(
            "spec/support/fake_shape.rb",
            "class FakeShape < Shape\n  def area\n    9\n  end\nend\n",
        );
        let source = "shape = Shape.new\nshape.area\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let beside = tempfile::tempdir().unwrap();
        let path = beside.path().join("scratch_shape.rb");
        let loose_source = "class ScratchShape < Shape\n  def area\n    7\n  end\nend\n";
        std::fs::write(&path, loose_source).unwrap();
        let loose = crate::workspace::DocUri::from_path(&path).unwrap();
        harness.open(&loose, loose_source);

        assert_eq!(
            harness.implementation_list(&caller, source, "area"),
            [
                "shape.rb:1:6",
                "circle.rb:1:6",
                "square.rb:1:6",
                "fake_shape.rb:1:6"
            ],
            "the suite's double is last and the scratch file's is nowhere"
        );
    }

    #[test]
    fn the_answer_is_locations_for_a_client_that_asked_for_nothing_and_links_for_one_that_did() {
        // Claude Code sends `textDocument/implementation` without declaring any
        // `textDocument.implementation` capability, so the negotiated shape is `Location[]` (`uri`
        // and `range`, not `targetUri`). Reusing `definition`'s flag, which that client *does* set,
        // would send links it never asked for.
        let mut harness = shapes();
        let source = "shape = Shape.new\nshape.area\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let flat = harness.implementation_at(&caller, source, "area");
        assert!(flat[0]["uri"].is_string(), "{flat}");
        assert!(flat[0]["targetUri"].is_null(), "{flat}");

        harness.takes_implementation_links();
        let linked = harness.implementation_at(&caller, source, "area");
        assert!(linked[0]["targetUri"].is_string(), "{linked}");
        assert!(
            linked[0]["originSelectionRange"].is_object(),
            "a link carries the span it was asked at: {linked}"
        );
    }

    #[test]
    fn a_receiver_the_name_rung_guessed_is_refused_even_though_the_member_was_found_on_it() {
        // The tier gate is not the same test as `precise`, and this cursor shows it: `story` names
        // no type, the name rung guesses `Story` from the six letters, and the member really is on
        // it. So the answer is **precise** yet `Guessed`. Refusing only the imprecise list would
        // answer here, and every override below `Story` would be the same guess multiplied.
        let mut harness = Harness::new();
        harness.write(
            "lib/story.rb",
            "class Story\n  def title\n    \"x\"\n  end\nend\n",
        );
        harness.write(
            "lib/draft.rb",
            "class Draft < Story\n  def title\n    \"y\"\n  end\nend\n",
        );
        let source = "story = fetch\nstory.title\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let markdown = harness.hover_at(&caller, source, "title")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(
            markdown.contains("Guessed from name alone"),
            "the card says so: {markdown}"
        );
        assert!(
            harness
                .implementation_at(&caller, source, "title")
                .is_null(),
            "and the jump does not"
        );
    }

    #[test]
    fn a_constant_nothing_defines_has_nowhere_to_point_and_nothing_below_it() {
        // rubydex invents a `Todo` namespace for a constant the workspace mentions and no file
        // defines. It resolves, precisely and at the resolved tier, but has no definitions to place
        // and no descendants to search, so the honest answer is `null`, not a row pointing nowhere.
        let mut harness = Harness::new();
        let source = "Missing::Thing\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert!(
            harness
                .implementation_at(&caller, source, "Missing")
                .is_null()
        );
    }

    #[test]
    fn an_id_the_graph_no_longer_holds_is_looked_up_rather_than_trusted() {
        // The hierarchies' own rule, on the third list that reads declaration ids: a descendant set
        // keeps ids of deleted documents, and a resolution is only as current as its graph. Asked
        // directly, because no cursor produces a dead id on purpose.
        let graph = Graph::new();
        let synthesized = Synthesized::new();
        let nowhere = locator::Resolution {
            declarations: vec![DeclarationId::new(1_234_567_890_123_456_789)],
            precise: true,
            redirected: false,
            derivation: crate::analysis::types::Derivation::default(),
            receiver: None,
        };

        assert!(
            implementations(
                &graph,
                &synthesized,
                environment::Layout::default(),
                &nowhere,
                None,
                10,
                &Placed::of(&graph, |_| false, environment::Layout::default()),
            )
            .is_empty()
        );
    }

    #[test]
    fn a_cursor_on_nothing_the_graph_holds_has_no_implementations() {
        let mut harness = shapes();
        let source = "# just a comment\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert!(
            harness
                .implementation_at(&caller, source, "comment")
                .is_null()
        );
    }

    #[test]
    fn a_class_rubydex_resolved_as_its_own_superclass_sits_where_ruby_puts_it() {
        // A class rubydex resolved as its own superclass, repaired by
        // `Indexed::repair_superclasses`: both lists read
        // the chain Ruby builds, and so does the search for overrides.
        let mut harness = Harness::new();
        let base = "class ApplicationController\n  def authenticate\n  end\nend\n";
        let base_uri = harness.write("app/controllers/application_controller.rb", base);
        let admin = "\
module Admin
  class ApplicationController < ApplicationController
    def authenticate
    end
  end

  class UsersController < ApplicationController
  end
end
";
        let admin_uri = harness.write("app/controllers/admin/base.rb", admin);
        harness.index();
        assert_eq!(
            harness.hierarchy_rows(
                "typeHierarchy/supertypes",
                &admin_uri,
                admin,
                "UsersController"
            ),
            "class Admin::ApplicationController — base.rb\n\
             class ApplicationController — application_controller.rb"
        );
        assert_eq!(
            harness.hierarchy_rows(
                "typeHierarchy/subtypes",
                &base_uri,
                base,
                "ApplicationController"
            ),
            "class Admin::ApplicationController — base.rb\n\
             class Admin::UsersController — base.rb"
        );
        assert_eq!(
            harness.implementation_list(&base_uri, base, "authenticate"),
            ["application_controller.rb:1:6", "base.rb:2:8"]
        );
    }
}
