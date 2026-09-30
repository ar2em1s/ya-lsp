//! The graph, the two things indexed beside it, and the single door a write goes through.
//!
//! - [`Members`]: parenthesised member name -> every declaration holding it, for the name rung.
//! - [`Placed`]: where every document sits (the user's own code, how far each of its directories is
//!   from anywhere, and which trees the application never loads), for completion ranking and every
//!   surface that fences.
//!
//! Both are pure functions of the graph's contents, both are built by the first question that needs
//! them, and both are dropped by [`Indexed::graph_mut`]. That is the design, explained below.
//!
//! rubydex answers *is there a declaration called `Person#shout()`* in constant time, because a
//! `DeclarationId` is a hash of the name. It answers *which declarations anywhere are called
//! `#shout()`* by scanning every declaration it holds: [`rubydex::query::declaration_search`]
//! collects every id into a fresh `Vec`, chunks it across `available_parallelism()` newly spawned
//! threads, and joins them. [`locator`](super::locator)'s name rung asks that at every call the
//! receiver path could not answer, and it was a large share of the analysis thread's time.
//!
//! A single-threaded scan here was slower still (the fan-out pays for itself; most of its cost is
//! setup, not matching), so the answer was not to scan at all: this index, built once per graph
//! change, cut the name rung's cost by well over a third. Building it is now that rung's whole
//! cost; the lookups are negligible. Every write drops the index (that is [`Indexed::graph_mut`],
//! by design), so the question to ask next is how many writes a session really needs, not how fast
//! [`Members::of`] scans.
//!
//! # Why the graph is private to this file
//!
//! An index of the graph is wrong as soon as the graph moves, and a stale name rung answers with a
//! `def` that no longer exists: silently, at the tier a reader can least check. Rust privacy is by
//! module *descendant*, so a private field on `Analysis` is visible to every file under
//! `analysis/`, and "remember to invalidate" would be the whole guarantee. Here the field is
//! private to **this** module and there is no `DerefMut`: [`Indexed::graph_mut`] is the only route
//! to a `&mut Graph` in the crate, and it drops the index on the way. A mutation that forgets to
//! invalidate does not compile.
//!
//! **So a write nobody needed is expensive**, and `didOpen` was one: the editor opens a file the
//! startup walk already indexed, `index_buffer` hands rubydex text it already has,
//! `Graph::consume_document_changes` compares `Document::content_hash` and returns, and the index
//! is gone anyway, because taking the `&mut Graph` drops it.
//! [`Analysis::graph_holds`](super::Analysis::graph_holds) asks the same hash one step earlier,
//! where both the parse and the invalidation can still be skipped.

use std::{
    cell::OnceCell,
    collections::{HashMap, HashSet, VecDeque},
    ops::Deref,
    rc::Rc,
};

use rubydex::{
    indexing::{self, LanguageId},
    model::{
        declaration::{Ancestor, Ancestors, Declaration, Namespace},
        definitions::{Definition, Mixin},
        graph::Graph,
        identity_maps::{IdentityHashBuilder, IdentityHashMap},
        ids::{DeclarationId, NameId, StringId, UriId},
        name::ParentScope,
    },
};

use super::{
    completion::directory_of,
    environment,
    locator::Spans,
    synthesized::{GENERATED_SCHEME, source_of},
    types,
};
use crate::workspace::rails;

/// The graph, and what is indexed beside it.
///
/// Reads go through [`Deref`], so this is a `&Graph` wherever one is needed. Writes do not; the
/// module docs explain why that asymmetry is the whole design.
pub struct Indexed {
    graph: Graph,
    /// Built by the first question that needs it, dropped by the next write.
    ///
    /// Lazy, not built with the settle, because a settle nobody navigates after should cost
    /// nothing, and `documentHighlight` fires on every cursor move, so any session that navigates
    /// pays it back within seconds.
    members: OnceCell<Members>,
    /// The second thing indexed beside the graph, and lazy for the same reason. See [`Placed`].
    placed: OnceCell<Placed>,
    /// Every namespace whose linearization rubydex found cyclic, by the last segment of its name.
    /// Lazy for [`Self::members`]' reason. See [`Self::cyclic_named`].
    cycles: OnceCell<HashMap<String, Vec<DeclarationId>>>,
    /// Every document that calls `instance_variable_set` or `remove_instance_variable`. Lazy for
    /// [`Self::members`]' reason. See [`Self::reflective_documents`].
    reflective: OnceCell<Vec<UriId>>,
    /// Every call of a method, by the name it is called by: filled one name at a time, as asked.
    /// See [`Self::calls_named`].
    calls: std::cell::RefCell<HashMap<StringId, Rc<[Call]>>>,
    /// Every class a view can run on. Lazy for [`Self::members`]' reason. See [`Self::renderers`].
    renderers: OnceCell<Rc<[DeclarationId]>>,
    /// Each of those and the layouts its views are rendered in. See [`Self::layouts`].
    layouts: OnceCell<Rc<[(DeclarationId, rails::Layouts)]>>,
    /// A large document's spans, by the document: filled one document at a time, as asked. See
    /// [`Self::spans`].
    spans: std::cell::RefCell<HashMap<UriId, Rc<Spans>>>,
    /// Each object's classes and its writers' documents: filled one object at a time, as asked.
    /// See [`Self::hierarchy`].
    hierarchies: std::cell::RefCell<Hierarchies>,
    /// What the fences read off each document's path: filled one document at a time, as asked.
    /// See [`Self::paths`].
    paths: environment::HeldPaths,
}

/// One call site: its document and the span rubydex filed it under.
pub type Call = (UriId, u32, u32);

/// [`Indexed::hierarchy`]'s: by the namespace, whether it is the class object's side, and the fence
/// it was built under.
type Hierarchies =
    HashMap<(DeclarationId, bool, environment::FenceKey), Option<Rc<types::Hierarchy>>>;

impl Default for Indexed {
    /// `Graph::new`, not `Graph::default`, which is a different graph: `new` installs `Object`,
    /// `BasicObject`, `Module` and `Class`, and nothing resolves without them.
    ///
    /// Plus one document of our own on top: [`OBJECT_MIXINS`].
    fn default() -> Self {
        let mut graph = Graph::new();
        indexing::index_source(
            &mut graph,
            OBJECT_MIXINS_URI.into(),
            OBJECT_MIXINS,
            &LanguageId::Rbs,
        );
        Self {
            graph,
            members: OnceCell::new(),
            placed: OnceCell::new(),
            cycles: OnceCell::new(),
            reflective: OnceCell::new(),
            calls: std::cell::RefCell::default(),
            renderers: OnceCell::new(),
            layouts: OnceCell::new(),
            spans: std::cell::RefCell::default(),
            hierarchies: std::cell::RefCell::default(),
            paths: environment::HeldPaths::default(),
        }
    }
}

/// The URI the document below is filed under, and **every character matters.**
///
/// It must sort *before* every `file:` URI, which `c` < `f` guarantees and
/// `the_seed_uri_sorts_before_every_file_uri` pins. See [`OBJECT_MIXINS`] for why.
///
/// It must also be a URI [`DocUri::from_graph_uri`](crate::workspace::DocUri::from_graph_uri)
/// refuses, so no request can ever hand it to a client: the same fence rubydex's own
/// `rubydex:built-in` sits behind, and `locator::named_after`'s `unopenable` tier reads it off the
/// same `file:` prefix. Anything not `file:` satisfies that half.
const OBJECT_MIXINS_URI: &str = "core:ya-lsp/object.rbs";

/// `Kernel` must be the *last* thing included into `Object`, and rubydex puts it first.
///
/// # What Ruby means
///
/// `Kernel` is included into `Object` at boot, so every later `Object.include(M)` sits **above**
/// it: `Object.ancestors` is `[Object, M, Kernel, BasicObject]`. The order matters: `Kernel`'s
/// methods are private, so a `Kernel` in front of `M` *shadows* `M`'s public method of the same
/// name, and the member disappears from every list drawn from the chain. Bundles routinely include
/// into `Object`: `debug` installs `DEBUGGER__::ForkInterceptor#fork`, and `pp`, `json`, `minitest`
/// and `activesupport` do the same.
///
/// # What rubydex does
///
/// `resolution.rs`'s `linearize_mixins` walks `declaration.definitions()` and `push_front`s each
/// mixin, so the last one processed ends up **first**, which is Ruby's rule. What is not Ruby's
/// rule is the walk order: `prepare_units` sorts each resolve's pending definitions by *(name
/// depth, URI rank, offset)*, the URI rank being lexicographic. rubydex declares its own
/// `class Object < BasicObject; include Kernel; end` in a synthetic document called
/// `rubydex:built-in`, and `r` > `f`, so that document sorts after every gem, its `include Kernel`
/// is processed last, and `Kernel` lands at the head of the chain with every gem's module behind
/// it.
///
/// # What this is
///
/// The same `include Kernel`, from a document that sorts *first*. Processed first, `Kernel` goes to
/// the back of the include list and stays: `linearize_mixins` deduplicates a repeated include
/// against what it already has, so rubydex's own copy (still processed last) is dropped, not moved,
/// and this one's position survives.
///
/// **A repair, not a model.** URI order is not Ruby's load order, and nothing here makes it so, so
/// the order *among* a bundle's own modules is still arbitrary. `Kernel` is the one chain entry
/// with a rule that holds in every project (included at boot, so always last), and it is the only
/// one this fixes.
///
/// It costs one document, three lines and no resolve. The alternative, a resolve inside
/// `index_workspace`, is only sound paired with a generator pass that would then run against a
/// fraction of the bundle.
///
/// # Why `::Kernel` and not `Kernel`
///
/// **Because a rubydex name is keyed by spelling *and* nesting, so `Kernel` written inside
/// `class Object` would be the same `NameId` as the one in `rubydex:built-in`.** Two documents, two
/// constant references, one shared name entry, and rubydex's incremental invalidation works on
/// names: unresolving one re-queues *every* reference sharing it, without un-recording what the
/// last resolve filed on the target declaration. The next resolve then calls
/// `record_resolved_reference` twice for this document's reference and trips `declaration.rs:160`:
/// *"Cannot add the same exact reference to a declaration twice"*. That is a `debug_assert`, so a
/// debug build crash-loops (the contained panic rebuilds, the rebuild resolves, and round it goes)
/// and a release build gets silent duplicate references.
///
/// `::Kernel` is a different spelling, hence a different name, so this document shares nothing with
/// rubydex's. It resolves to the same declaration and linearizes identically.
/// `lifecycle::the_config_file_reloads_without_a_restart` fails without it, so: **do not "simplify"
/// the `::` away.** The superclass is omitted for the same reason: `BasicObject` inside
/// `class Object` was the second shared name.
const OBJECT_MIXINS: &str = "class Object
  include ::Kernel
end
";

impl Deref for Indexed {
    type Target = Graph;

    fn deref(&self) -> &Graph {
        &self.graph
    }
}

impl Indexed {
    /// The only `&mut Graph` in the crate, and the only thing that invalidates the indexes.
    ///
    /// **Both of them, and any third would go here too.** Everything cached in this struct is a
    /// function of the graph's contents, so the one door a write goes through is the one place that
    /// can drop them all without anyone remembering to.
    pub fn graph_mut(&mut self) -> &mut Graph {
        self.members.take();
        self.placed.take();
        self.cycles.take();
        self.reflective.take();
        self.calls.get_mut().clear();
        self.renderers.take();
        self.layouts.take();
        self.spans.get_mut().clear();
        self.hierarchies.get_mut().clear();
        self.paths.forget();
        &mut self.graph
    }

    /// Every declaration whose name contains `#member`, the one query shape ya-lsp asked
    /// `declaration_search`'s exact mode for.
    ///
    /// `member` is parenthesised (`shout()`), as [`member_name`](super::locator) produces and
    /// rubydex keys method declarations.
    pub fn members_named(&self, member: &str) -> &[DeclarationId] {
        self.members
            .get_or_init(|| Members::of(&self.graph))
            .named(member)
    }

    /// Every namespace named `unqualified` (the last segment) whose linearization rubydex found
    /// cyclic.
    ///
    /// **The shape it finds is rubydex resolving a superclass to the class being opened**
    /// (an upstream defect): `class ApplicationController < ApplicationController` inside
    /// `module Admin`. The superclass it dropped is spelled like the class, so a class of that last
    /// name may be missing a subclass from its descendants, and a question over its descendants
    /// must know.
    pub fn cyclic_named(&self, unqualified: &str) -> &[DeclarationId] {
        self.cycles
            .get_or_init(|| {
                let mut cycles: HashMap<String, Vec<DeclarationId>> = HashMap::new();
                for (id, declaration) in self.graph.declarations() {
                    if let Declaration::Namespace(namespace) = declaration
                        && matches!(namespace.ancestors(), Ancestors::Cyclic(_))
                    {
                        // Only the class whose own superclass is spelled like it: the one the
                        // upstream defect makes. A subclass inherits the short chain without
                        // dropping any link of its own.
                        let own = declaration.unqualified_name();
                        if self.superclass_spelled(declaration, &own) {
                            cycles.entry(own).or_default().push(*id);
                        }
                    }
                }
                cycles
            })
            .get(unqualified)
            .map_or(&[], Vec::as_slice)
    }

    /// Whether one of `declaration`'s `class` definitions writes a superclass whose last segment is
    /// `spelled`.
    fn superclass_spelled(&self, declaration: &Declaration, spelled: &str) -> bool {
        declaration
            .definitions()
            .iter()
            .filter_map(|id| self.graph.definitions().get(id))
            .any(|definition| {
                let Definition::Class(class) = definition else {
                    return false;
                };
                class
                    .superclass_ref()
                    .and_then(|reference| self.graph.constant_references().get(reference))
                    .and_then(|reference| self.graph.names().get(reference.name_id()))
                    .and_then(|name| self.graph.strings().get(name.str()))
                    .is_some_and(|written| written.as_str() == spelled)
            })
    }

    /// Every document with a call to `instance_variable_set` or `remove_instance_variable`, from
    /// rubydex's call index: the only documents that can write a variable no `@name =` spells on
    /// an object they do not own. Sorted, so a reader walks them in a stable order.
    pub fn reflective_documents(&self) -> &[UriId] {
        self.reflective.get_or_init(|| {
            let names = super::scopes::REFLECTIVE_WRITERS.map(StringId::from);
            let mut documents: Vec<UriId> = self
                .graph
                .method_references()
                .values()
                .filter(|reference| names.contains(reference.str()))
                .map(|reference| reference.uri_id())
                .collect();
            documents.sort_unstable();
            documents.dedup();
            documents
        })
    }

    /// Every call of a method named `name`, sorted, whatever it is called on.
    ///
    /// A scan of rubydex's whole call index, so each name is scanned once per graph, not once per
    /// request.
    pub fn calls_named(&self, name: &str) -> Rc<[Call]> {
        let wanted = StringId::from(name);
        if let Some(held) = self.calls.borrow().get(&wanted) {
            return Rc::clone(held);
        }
        let mut calls: Vec<Call> = self
            .graph
            .method_references()
            .values()
            .filter(|reference| *reference.str() == wanted)
            .map(|reference| {
                (
                    reference.uri_id(),
                    reference.offset().start(),
                    reference.offset().end(),
                )
            })
            .collect();
        calls.sort_unstable();
        let calls: Rc<[Call]> = Rc::from(calls);
        self.calls.borrow_mut().insert(wanted, Rc::clone(&calls));
        calls
    }

    /// `uri_id`'s spans, sorted for [`locator::locate_held`](super::locator::locate_held): built
    /// by the first question about that document, and held until the graph changes.
    pub fn spans(&self, uri_id: UriId, build: impl FnOnce() -> Spans) -> Rc<Spans> {
        if let Some(held) = self.spans.borrow().get(&uri_id) {
            return Rc::clone(held);
        }
        let spans = Rc::new(build());
        self.spans.borrow_mut().insert(uri_id, Rc::clone(&spans));
        spans
    }

    /// Link every class rubydex resolved as its own superclass to the class Ruby names there. Run
    /// after each resolve, by `analysis::resolve`.
    ///
    /// - **The upstream defect.** `class ApplicationController < ApplicationController` inside
    ///   `module Admin` names the top-level class, because Ruby evaluates the superclass before the
    ///   constant exists. rubydex finds `Admin::ApplicationController` itself, marks the chain
    ///   cyclic, and ends it, and the chain of every class below it, at an `Object` it estimates.
    /// - **Repaired in the graph, once, not at each reader.** About thirty places read a chain or a
    ///   set of descendants, rubydex's own member search and completion among them. A chain written
    ///   into the graph reaches every one of them.
    /// - **What is written.** The parent is [`superclass_outside`]. Each class below the cycle keeps
    ///   its own head (itself and its modules, down to the cycle) and takes the parent's whole chain
    ///   after it, less the modules the parent already has, which Ruby's `include` skips too. Each
    ///   class in the parent's chain gains it as a descendant. The class side takes the same from
    ///   the parent's singleton ([`splice`]).
    /// - **rubydex keeps it consistent.** An edit to a class takes it out of every ancestor its chain
    ///   lists and relinks every descendant it lists, so an edit on either side unlinks the repair
    ///   with the chain, and the next resolve's pass writes it again.
    /// - **Left as it was:** a parent nothing names, and a parent below the class, which Ruby cannot
    ///   load either. Both keep the cycle, and the readers that refuse one (`types::ancestry`,
    ///   [`Self::cyclic_named`]) still do.
    pub fn repair_superclasses(&mut self) {
        let mut pending = cut_short(&self.graph);
        if pending.is_empty() {
            return;
        }
        let found = pending.len();
        let graph = self.graph_mut();
        // A parent may sit below another such class (`Admin::Reports::ApplicationController <
        // ApplicationController` reaching `Admin::ApplicationController`), so a class waits until
        // its parent's chain is whole. Each round repairs one or more, or stops.
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|&(class, parent)| !splice(graph, class, parent));
            if pending.len() == before {
                break;
            }
        }
        tracing::debug!(
            "relinked {} of {found} classes resolved as their own superclass",
            found - pending.len()
        );
    }

    /// Where every document in the graph sits, built by the first request that asks.
    ///
    /// `judge` receives the graph instead of this reading it, because two of [`Placed`]'s four
    /// answers are not this file's to give: whether a document is the user's own code is
    /// [`Analysis::is_own_code`](super::Analysis) (which reads the workspace root, the configured
    /// load paths and the discovered gem roots), and which directory names fence a tree is
    /// `[trees]`. Those are the memo's two non-graph inputs, and the two reasons
    /// [`Self::forget_placement`] exists.
    pub(super) fn placed(&self, judge: impl FnOnce(&Graph) -> Placed) -> &Placed {
        self.placed.get_or_init(|| judge(&self.graph))
    }

    /// Drop the placement because the *question* changed, not the graph.
    ///
    /// Called from the two places that can change it: a `ya-lsp.toml` reload, which can move
    /// `[trees]` and the load paths, and gem discovery, where `foreign_prefixes` learns a directory
    /// inside the workspace root is somebody else's bundle. Every other input is the graph's
    /// contents, which [`Self::graph_mut`] invalidates. [`Self::renderers`] reads whose code a
    /// document is too, so it goes with it, and [`Self::hierarchy`] reads the fence, whose
    /// [`Layout`](environment::Layout) is those same inputs, as are [`Self::paths`].
    pub(super) fn forget_placement(&mut self) {
        self.placed.take();
        self.renderers.take();
        self.layouts.take();
        self.hierarchies.get_mut().clear();
        self.paths.forget();
    }

    /// What the fences have read off each document's path ([`environment::HeldPaths`]), for
    /// `Analysis::layout` to carry.
    ///
    /// **Held across requests** for [`Self::hierarchy`]'s reason: a hover asks the same few
    /// hundred paths on every request, and each answer is a function of the path and the
    /// [`Layout`](environment::Layout) alone. The layout is a non-graph input, dropped by
    /// [`Self::forget_placement`]; [`Self::graph_mut`] drops it too, for `HeldPaths`' reason.
    pub(super) fn paths(&self) -> &environment::HeldPaths {
        &self.paths
    }

    /// Every class a view can run on (`types::every_renderer`), built by the first request that
    /// asks and held until the graph changes.
    ///
    /// **Held across requests**, not one request's memo: it walks every document in the graph and
    /// parses each one's URI, and a partial or a helper asks it on every read (a tenth of two
    /// corpora's audits). `find` receives nothing, for [`Self::placed`]'s reason: its answer
    /// reads the view convention and whose code a document is. Those are its non-graph inputs,
    /// dropped by [`Self::forget_placement`] and [`Self::forget_renderers`].
    pub(super) fn renderers(
        &self,
        find: impl FnOnce() -> Rc<[DeclarationId]>,
    ) -> Rc<[DeclarationId]> {
        Rc::clone(self.renderers.get_or_init(find))
    }

    /// Each class a view can run on and the layouts its views are rendered in
    /// (`types::layout_renderers`), built by the first layout read that asks and held with
    /// [`Self::renderers`], from which it is found: dropped wherever they are.
    pub(super) fn layouts(
        &self,
        find: impl FnOnce() -> Rc<[(DeclarationId, rails::Layouts)]>,
    ) -> Rc<[(DeclarationId, rails::Layouts)]> {
        Rc::clone(self.layouts.get_or_init(find))
    }

    /// Every class an object can be and every document its writers are written in
    /// (`types::hierarchy`), built by the first request that asks and held until the graph
    /// changes.
    ///
    /// **Held across requests**, for [`Self::renderers`]' reason: `build` walks every descendant of
    /// the object and reads each one's paths against the fence, and a template's or a helper's
    /// variable asks it once per class that renders it, on every hover. Rebuilt per request, it was
    /// most of what a slow hover cost (2026-09-29).
    ///
    /// - **A refusal is held too.** `None` is as much the graph's answer as a hierarchy is.
    /// - **The fence is in the key** ([`environment::Fence::key`]), so a spec and a loose file each
    ///   keep their own answer. Its [`Layout`](environment::Layout) is not: that is a non-graph
    ///   input, dropped by [`Self::forget_placement`].
    pub(super) fn hierarchy(
        &self,
        base: DeclarationId,
        class_side: bool,
        fence: environment::FenceKey,
        build: impl FnOnce() -> Option<types::Hierarchy>,
    ) -> Option<Rc<types::Hierarchy>> {
        let key = (base, class_side, fence);
        if let Some(held) = self.hierarchies.borrow().get(&key) {
            return held.clone();
        }
        let made = build().map(Rc::new);
        self.hierarchies.borrow_mut().insert(key, made.clone());
        made
    }

    /// Drop the renderers because the view convention was rebuilt: the generator pass rebuilds it,
    /// and need not write the graph to do so.
    pub(super) fn forget_renderers(&mut self) {
        self.renderers.take();
        self.layouts.take();
    }

    /// Whether the index is built, for tests about it surviving (or not) a route into
    /// [`Analysis`](super::Analysis) that had no reason to touch the graph.
    #[cfg(test)]
    pub(super) fn members_are_built(&self) -> bool {
        self.members.get().is_some()
    }

    /// The same, for the placement.
    #[cfg(test)]
    pub(super) fn placement_is_built(&self) -> bool {
        self.placed.get().is_some()
    }

    /// How many documents' spans are held, for the same kind of test.
    #[cfg(test)]
    pub(super) fn spans_held(&self) -> usize {
        self.spans.borrow().len()
    }
}

/// Every class whose chain rubydex ended at a superclass resolved to the class itself, with the
/// class Ruby names there. Sorted, so the repair runs in the same order on every resolve.
fn cut_short(graph: &Graph) -> Vec<(DeclarationId, DeclarationId)> {
    let mut found: Vec<(DeclarationId, DeclarationId)> = graph
        .declarations()
        .keys()
        .filter(|id| is_class(graph, **id) && is_cut(graph, **id))
        .filter_map(|id| Some((*id, superclass_outside(graph, *id)?)))
        .collect();
    found.sort_unstable();
    found
}

/// The class Ruby names as `class`'s superclass, where rubydex resolved that name to `class`
/// itself. `None` for every other class, and where Ruby would raise.
///
/// Ruby evaluates the superclass before it creates the constant, so its lookup cannot find the
/// class being opened. This is that lookup with the class skipped, in Ruby's order:
/// 1. each scope the code is lexically inside, innermost first;
/// 2. the ancestors of the innermost one, so `class Scope < Scope` inside a policy reaches its
///    parent policy's `Scope`;
/// 3. the top level.
///
/// The first constant found is the answer, and it must be a class: a module there is Ruby's
/// `TypeError`, not a reason to look further. **Only a bare name**, which is the defect's shape:
/// Ruby cannot resolve `Outer::Name` to a class that does not exist yet, so a qualified name that
/// reached the class itself is Ruby's `NameError`.
pub fn superclass_outside(graph: &Graph, class: DeclarationId) -> Option<DeclarationId> {
    superclass_names(graph, class)
        .filter(|name| graph.name_id_to_declaration_id(*name) == Some(&class))
        .filter_map(|name| graph.names().get(&name))
        .filter(|name| matches!(name.parent_scope(), ParentScope::None))
        .find_map(|name| {
            let spelled = graph.strings().get(name.str())?.as_str();
            looked_up(graph, class, *name.nesting(), spelled)
        })
}

/// The name each of `class`'s `class` definitions writes as its superclass.
fn superclass_names(graph: &Graph, class: DeclarationId) -> impl Iterator<Item = NameId> + '_ {
    graph
        .declarations()
        .get(&class)
        .into_iter()
        .flat_map(Declaration::definitions)
        .filter_map(|id| match graph.definitions().get(id)? {
            Definition::Class(written) => written.superclass_ref(),
            _ => None,
        })
        .filter_map(|reference| graph.constant_references().get(reference))
        .map(|reference| *reference.name_id())
}

/// Ruby's constant lookup for `spelled`, written inside `nesting`, skipping `class`. See
/// [`superclass_outside`].
fn looked_up(
    graph: &Graph,
    class: DeclarationId,
    nesting: Option<NameId>,
    spelled: &str,
) -> Option<DeclarationId> {
    let named = |name: String| types::declared(graph, &name).filter(|id| *id != class);
    let inside = |owner: &DeclarationId| {
        let owner = graph.declarations().get(owner)?.name();
        named(format!("{owner}::{spelled}"))
    };
    // A scope rubydex could not resolve is no place to look; the scopes around it still are.
    let scopes: Vec<DeclarationId> =
        std::iter::successors(nesting, |name| *graph.names().get(name)?.nesting())
            .filter_map(|name| graph.name_id_to_declaration_id(name).copied())
            .collect();
    let found = scopes
        .iter()
        .find_map(inside)
        .or_else(|| {
            let innermost = graph.declarations().get(scopes.first()?)?.as_namespace()?;
            innermost
                .ancestors()
                .iter()
                .find_map(|ancestor| match ancestor {
                    Ancestor::Complete(id) => inside(id),
                    Ancestor::Partial(_) => None,
                })
        })
        .or_else(|| named(spelled.to_owned()))?;
    is_class(graph, found).then_some(found)
}

/// Give `class`, and every class below it, the chain Ruby builds on `parent`, on both sides.
/// `false` while `parent`'s own chain is still cut, so [`Indexed::repair_superclasses`] asks again
/// once it is not.
///
/// - **A parent below `class` never gets here.** Its chain runs through `class`, so it is cut
///   until this very call repairs it, and [`whole_chain`] refuses it: the cycle stays, as in Ruby.
/// - **Rebuilt from each class's own mixins, not from the cut chain.** rubydex resolves a bare
///   `include Shared` in a class body through the class's own chain, so in a class it cut, those
///   names resolve only after the cut chain is cached, and never reach it. [`linearized`] reads
///   them now that they have.
/// - **Every class below a cut is cut**, since its chain runs through it, so each is written over.
fn splice(graph: &mut Graph, class: DeclarationId, parent: DeclarationId) -> bool {
    let Some(tail) = whole_chain(graph, parent) else {
        return false;
    };
    // **The class side.** rubydex gives a class a singleton only where it declares a class method
    // or extends a module, so a class without one adds nothing to the chain, and the nearest class
    // above the parent that has one stands in for it.
    let stand_in = tail
        .ancestors
        .iter()
        .filter_map(complete)
        .filter(|id| is_class(graph, *id))
        .find_map(|id| singleton_class_of(graph, id))
        .and_then(|singleton| whole_chain(graph, singleton));

    // Each class's chains, by the class: `top_down` puts a superclass before its subclasses, so the
    // chains a class builds on are here before it is.
    let mut instance: HashMap<DeclarationId, Chain> = HashMap::new();
    let mut class_side: HashMap<DeclarationId, (DeclarationId, Chain)> = HashMap::new();
    for (id, superclass) in top_down(graph, class) {
        let above = superclass.map_or(Some(&tail), |superclass| instance.get(&superclass));
        let above_class_side = superclass.map_or(stand_in.as_ref(), |superclass| {
            class_side.get(&superclass).map(|(_, chain)| chain)
        });
        let built = singleton_class_of(graph, id)
            .zip(above_class_side)
            .map(|(singleton, above)| (id, (singleton, linearized(graph, singleton, above, true))));
        class_side.extend(built);
        let built = above.map(|above| (id, linearized(graph, id, above, false)));
        instance.extend(built);
    }
    let class_side = class_side.into_values();
    for (id, chain) in instance.into_iter().chain(class_side) {
        write(graph, id, chain);
    }
    true
}

/// A chain as [`linearized`] builds it, with the two facts rubydex's [`Ancestors`] states carry.
struct Chain {
    ancestors: Vec<Ancestor>,
    /// A name in it has not resolved.
    partial: bool,
    /// A module in it is cyclic itself, which is not this repair's to undo.
    cyclic: bool,
}

/// `id`'s chain, or `None` while it is still cut (or was never linearized).
fn whole_chain(graph: &Graph, id: DeclarationId) -> Option<Chain> {
    let chain = chain_of(graph, id)?;
    (!chain.cyclic && !chain.ancestors.is_empty()).then_some(chain)
}

/// `id`'s chain as rubydex holds it, cut or not.
fn chain_of(graph: &Graph, id: DeclarationId) -> Option<Chain> {
    let (ancestors, partial, cyclic) =
        match graph.declarations().get(&id)?.as_namespace()?.ancestors() {
            Ancestors::Complete(ancestors) => (ancestors, false, false),
            Ancestors::Partial(ancestors) => (ancestors, true, false),
            Ancestors::Cyclic(ancestors) => (ancestors, false, true),
        };
    Some(Chain {
        ancestors: ancestors.clone(),
        partial,
        cyclic,
    })
}

/// `class` first, then every class rubydex lists below it, each after its superclass, with that
/// superclass. The top's is the parent, which the caller holds.
fn top_down(graph: &Graph, class: DeclarationId) -> Vec<(DeclarationId, Option<DeclarationId>)> {
    let below: Vec<DeclarationId> = graph
        .declarations()
        .get(&class)
        .and_then(Declaration::as_namespace)
        .into_iter()
        .flat_map(|namespace| namespace.descendants().iter().copied())
        .filter(|id| *id != class)
        .collect();
    // How many superclasses up `class` is: the order to build in. A class that does not reach it
    // within that many steps is not below it, whatever rubydex listed, so it is left alone.
    let depth = |id: DeclarationId| {
        std::iter::successors(Some(id), |at| superclass_of(graph, *at))
            .take(below.len() + 1)
            .position(|at| at == class)
    };
    let mut ordered: Vec<(usize, DeclarationId)> = below
        .iter()
        .filter_map(|id| Some((depth(*id)?, *id)))
        .collect();
    ordered.sort_unstable();
    std::iter::once((class, None))
        .chain(
            ordered
                .into_iter()
                .map(|(_, id)| (id, superclass_of(graph, id))),
        )
        .collect()
}

/// The superclass rubydex picks: the first `class` definition whose superclass resolved.
fn superclass_of(graph: &Graph, id: DeclarationId) -> Option<DeclarationId> {
    superclass_names(graph, id).find_map(|name| {
        graph
            .name_id_to_declaration_id(name)
            .copied()
            .filter(|id| is_namespace(graph, *id))
    })
}

/// What rubydex's `linearize_ancestors` builds for `id` on top of `above`, from the mixins `id`'s
/// definitions write: its prepends, itself, its includes, then `above`.
///
/// **rubydex's own rules** (`resolution.rs`'s `linearize_mixins`), which are Ruby's: the last
/// mixin written comes first, a prepend already prepended changes nothing, and an include is
/// skipped when the class already has the module, above or below itself. A class side reads the
/// attached class's `extend`s as includes, first, as rubydex does. A name that has not resolved
/// stays in the chain as a partial entry, and a name that resolved to something other than a class
/// or a module is skipped.
fn linearized(graph: &Graph, id: DeclarationId, above: &Chain, class_side: bool) -> Chain {
    let declaration = graph.declarations().get(&id);
    let attached = declaration
        .filter(|_| class_side)
        .and_then(|declaration| graph.declarations().get(declaration.owner_id()));
    let extends = attached
        .into_iter()
        .flat_map(|attached| mixins_of(graph, attached))
        .filter(|mixin| matches!(mixin, Mixin::Extend(_)));
    let own = declaration
        .into_iter()
        .flat_map(|declaration| mixins_of(graph, declaration))
        .filter(|mixin| !matches!(mixin, Mixin::Extend(_)));

    let (mut partial, mut cyclic) = (above.partial, above.cyclic);
    let mut prepends: VecDeque<Ancestor> = VecDeque::new();
    let mut includes: VecDeque<Ancestor> = VecDeque::new();
    for (prepend, name) in extends.chain(own).filter_map(|mixin| {
        let reference = graph
            .constant_references()
            .get(mixin.constant_reference_id())?;
        Some((matches!(mixin, Mixin::Prepend(_)), *reference.name_id()))
    }) {
        let module = match graph.name_id_to_declaration_id(name) {
            Some(module) => chain_of(graph, *module),
            None => Some(Chain {
                ancestors: vec![Ancestor::Partial(name)],
                partial: true,
                cyclic: false,
            }),
        };
        let Some(module) = module else {
            continue;
        };
        partial |= module.partial;
        cyclic |= module.cyclic;
        let mut ids = module.ancestors;
        if prepend {
            if ids.iter().any(|id| !prepends.contains(id)) {
                prepends.retain(|id| !ids.contains(id));
                for id in ids.into_iter().rev() {
                    prepends.push_front(id);
                }
            }
        } else {
            ids.retain(|id| {
                !prepends.contains(id) && !includes.contains(id) && !above.ancestors.contains(id)
            });
            for id in ids.into_iter().rev() {
                includes.push_front(id);
            }
        }
    }
    let mut ancestors: Vec<Ancestor> = prepends.into_iter().collect();
    ancestors.push(Ancestor::Complete(id));
    ancestors.extend(includes);
    ancestors.extend_from_slice(&above.ancestors);
    Chain {
        ancestors,
        partial,
        cyclic,
    }
}

/// Every mixin `declaration`'s definitions write, in the order rubydex reads them.
fn mixins_of<'g>(
    graph: &'g Graph,
    declaration: &'g Declaration,
) -> impl Iterator<Item = &'g Mixin> {
    declaration
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
        .flat_map(|definition| match definition {
            Definition::Class(class) => class.mixins(),
            Definition::Module(module) => module.mixins(),
            Definition::SingletonClass(singleton) => singleton.mixins(),
            _ => &[],
        })
}

/// Write `chain` as `id`'s, and record `id` as a descendant of every class and module in it, as
/// rubydex does for a chain it builds.
fn write(graph: &mut Graph, id: DeclarationId, chain: Chain) -> Option<()> {
    let above: Vec<DeclarationId> = chain.ancestors.iter().filter_map(complete).collect();
    let ancestors = match (chain.cyclic, chain.partial) {
        (true, _) => Ancestors::Cyclic(chain.ancestors),
        (false, true) => Ancestors::Partial(chain.ancestors),
        (false, false) => Ancestors::Complete(chain.ancestors),
    };
    graph
        .declarations_mut()
        .get_mut(&id)?
        .as_namespace_mut()?
        .set_ancestors(ancestors);
    for above in above {
        graph
            .declarations_mut()
            .get_mut(&above)?
            .as_namespace_mut()?
            .add_descendant(id);
    }
    Some(())
}

fn complete(ancestor: &Ancestor) -> Option<DeclarationId> {
    match ancestor {
        Ancestor::Complete(id) => Some(*id),
        Ancestor::Partial(_) => None,
    }
}

fn is_class(graph: &Graph, id: DeclarationId) -> bool {
    matches!(
        graph.declarations().get(&id),
        Some(Declaration::Namespace(Namespace::Class(_)))
    )
}

fn is_cut(graph: &Graph, id: DeclarationId) -> bool {
    graph
        .declarations()
        .get(&id)
        .and_then(Declaration::as_namespace)
        .is_some_and(|namespace| matches!(namespace.ancestors(), Ancestors::Cyclic(_)))
}

fn is_namespace(graph: &Graph, id: DeclarationId) -> bool {
    graph
        .declarations()
        .get(&id)
        .is_some_and(|declaration| declaration.as_namespace().is_some())
}

fn singleton_class_of(graph: &Graph, id: DeclarationId) -> Option<DeclarationId> {
    graph
        .declarations()
        .get(&id)?
        .as_namespace()?
        .singleton_class()
        .copied()
}

/// How many declarations the graph holds per distinct member name, rounded down.
///
/// Only the reservation reads it, so a wrong value costs a growth step or some slack, never an
/// answer. Real projects hold roughly four to five declarations per key. **Four is deliberately the
/// low end**: a reservation one item over a power of two pays for the whole next doubling, and four
/// reserves exactly the bucket count the true key total needs, so the map builds without a rehash
/// or a bucket array paid for twice.
const DECLARATIONS_PER_MEMBER: usize = 4;

/// `shout()` -> every declaration named `...#shout()`, over the whole graph.
///
/// **Keyed by the member's [`StringId`], rubydex's own hash of it, not the text.** It is the same
/// key `query::find_member_in_ancestors` takes and the same function `DeclarationId::from` keys
/// every declaration with, so the name rung is no more exposed to a collision here than
/// `graph.get()` is elsewhere. It buys two things: no allocation per occurrence (a `Box<str>` key
/// allocated once per `#` run and freed on the next write), and identity hashing, so growing the
/// map moves entries instead of rehashing strings. Key counts on real projects match the text-keyed
/// version exactly, which is the check that matters, since a collision would merge two members into
/// one key.
struct Members(IdentityHashMap<StringId, Vec<DeclarationId>>);

impl Members {
    /// One pass over the graph, keyed by every `#`-to-`)` run each name holds.
    ///
    /// **The list answered is exactly `name.contains("#{member}")`'s.** A member arrives
    /// parenthesised and holds no other `)`, so a name contains `#member` exactly when some `#` in
    /// it is followed by `member`, and since `member` ends at its only `)`, that run is the text
    /// from the `#` to the next `)`. Keying every such run, not just the last, covers a name with
    /// several `#`s, the only shape whose match would not be at the end. In practice none has two:
    /// rubydex spells a method `<namespace>#<member>`, and a namespace name is a constant path,
    /// which cannot contain `#`.
    ///
    /// **The ids keep the map's own iteration order**, the order `declaration_search` answered in
    /// (it chunked that same key order across its threads and concatenated the chunks). The name
    /// rung ranks its places from that order, so it is part of the answer, not a detail. Hash keys
    /// do not affect it: within a member, the order is the walk's push order, which is the graph's.
    ///
    /// **The scan left is already the fastest available.** The `find` calls dominate, and two
    /// hand-written rewrites (a byte `position` loop, and a single pass pairing each `#` with the
    /// next `)`) were both no faster than `str::find`, which is `memchr`. So the lever taken is the
    /// other one, not reading names that cannot match, and it **narrows nothing**. rubydex composes
    /// a `#` into a declaration name in exactly one place, `resolution::create_declaration`'s
    /// `format!("{}#{}", owner.name(), str)`, whose owner is always a namespace and whose callers
    /// build a method, an instance variable, a class variable or a global, nothing else.
    /// `Constant`, `ConstantAlias` and `Namespace` names hold no `#` (they are spelled
    /// `owner::name`, and a singleton `owner::<name>`), and of the four kinds that do, only a
    /// method's can hold a `)` (the others are `@x`, `@@x` and `$x`). So `Declaration::Method` is
    /// *exactly* the set of names that can carry the keyed run, and the result is the same as
    /// reading **every** declaration, which real projects confirmed byte for byte.
    ///
    /// **What remains is how often this runs, not the scan.** This build is the whole of the name
    /// rung's cost, so the next lever is rebuilding less, which is [`Indexed::graph_mut`]'s
    /// business, not a faster pass over names.
    fn of(graph: &Graph) -> Self {
        let declarations = graph.declarations();
        let mut by_member: IdentityHashMap<StringId, Vec<DeclarationId>> =
            IdentityHashMap::with_capacity_and_hasher(
                declarations.len() / DECLARATIONS_PER_MEMBER,
                IdentityHashBuilder,
            );
        for (id, declaration) in declarations {
            // Only a method's name can hold the run this keys; see the doc comment above.
            if declaration.as_method().is_none() {
                continue;
            }
            let name = declaration.name();
            let mut from = 0;
            while let Some(offset) = name[from..].find('#') {
                let at = from + offset;
                // A `#` with no `)` after it is an instance variable (`Foo#@count`), which no
                // parenthesised member can match, nor anything later in the name.
                let Some(close) = name[at..].find(')') else {
                    break;
                };
                by_member
                    .entry(StringId::from(&name[at + 1..=at + close]))
                    .or_default()
                    .push(*id);
                from = at + 1;
            }
        }
        Self(by_member)
    }

    fn named(&self, member: &str) -> &[DeclarationId] {
        self.0
            .get(&StringId::from(member))
            .map_or(&[], Vec::as_slice)
    }
}

/// Where every document in the graph sits, computed once per settle instead of once per request.
///
/// # Why
///
/// Without it, two whole-graph passes sit between a keystroke and a completion list, neither about
/// the cursor: building a `HashSet<UriId>` of the user's own documents by testing every document
/// against the workspace root, external load paths and every gem root, then walking every document
/// again for the unloadable tag and the generated scheme, and the own set for the test-tree tag. On
/// a large application that is tens of thousands of documents, and **the answer stays the same
/// until the graph moves**. Completion time would scale with workspace size instead of what the
/// user typed, mostly in three string-scanning leaves (the `split` behind `in_a_test_tree`, the
/// `rsplit_once` behind `directory_of`, and the prefix comparisons behind `is_own_code`).
///
/// # What is left per request
///
/// The one term really about the cursor: how many leading path segments each of the user's own
/// directories shares with the cursor's. That is why this holds the directory, not the URI: the
/// split that finds it is the `rsplit_once` above, and it is a property of the document.
///
/// # Cost
///
/// The build takes a few milliseconds and runs rarely on a settled graph, so it pays for itself by
/// the second completion. **A keystroke does not drop it**: `completion`
/// defers its settle, so a `didChange` followed by a completion is answered from the deferred path
/// with the table intact.
///
/// # What can make it wrong
///
/// The graph's contents, which [`Indexed::graph_mut`] invalidates and cannot forget, and the two
/// non-graph inputs, which [`Indexed::forget_placement`] handles and which can be forgotten.
pub struct Placed {
    /// Every document that is the user's own code.
    ///
    /// A set, not only the rows below, because its readers ask `contains` once per *definition* of
    /// every candidate: `environment::placement` over a hundred thousand of them.
    own: HashSet<UriId>,
    /// The same documents as a list, with what the ranking needs already read off each path.
    each_own: Vec<Own>,
    /// Every document in the graph the application never loads, read of the **whole graph**,
    /// deliberately not of `own`: a gem's generator template is copied, not loaded, wherever it
    /// ships from, and an engine's `db/migrate/` is migrations wherever it ships from.
    /// `environment.md` argues that asymmetry.
    unloadable: Vec<UriId>,
    /// Every document ya-lsp generated, beside the id of the source that implied it.
    ///
    /// The source is stored as a `UriId`, not text, because that is what the lookup needs:
    /// `UriId::from` hashes the URI, and hashing it here does it once.
    generated: Vec<(UriId, UriId)>,
    /// Every document in the graph that is outside the project altogether: a file open beside the
    /// user's work, not in it. See `environment::Layout::is_outside`.
    ///
    /// **A set, not a list**, unlike the three above, because it is asked `contains` about a
    /// document already in one of the others: a loose file whose path has a `spec` segment is in
    /// `unloadable` too, and the entry a completion list reads must carry both facts, not whichever
    /// loop wrote last. It is a handful of documents on any real machine (those a client sent
    /// `didOpen` for), so it costs nothing.
    outside: HashSet<UriId>,
}

/// One of the user's own documents, as the completion ranking reads it.
pub(super) struct Own {
    pub(super) id: UriId,
    /// The document's URI without its last segment, which is what nearness is measured against.
    pub(super) directory: Box<str>,
    /// Whether the application never loads it: a test tree, a generator template tree or a
    /// migration. The first is read of `own` and the other two of the whole graph, per
    /// [`Placed::unloadable`]; this field folds them together.
    pub(super) unloadable: bool,
}

impl Placed {
    /// One pass over the graph, asking each document every question at once.
    ///
    /// `is_own` is [`Analysis::is_own_code`](super::Analysis) and `names` is `[trees]`;
    /// [`Indexed::placed`] explains why neither is read here.
    pub(super) fn of(
        graph: &Graph,
        is_own: impl Fn(&str) -> bool,
        layout: environment::Layout<'_>,
    ) -> Self {
        let names = layout.names;
        let started = std::time::Instant::now();
        let documents = graph.documents();
        // No reservation on any of the four: the graph holds the whole bundle and Ruby's signatures
        // beside the project, so each is a minority of `documents` by a project-specific amount,
        // not a constant, and this runs once per settle, where a few doublings cost microseconds.
        // `Members` reserves because it covers every declaration, where the ratio is stable.
        let mut own = HashSet::new();
        let mut each_own = Vec::new();
        let mut unloadable = Vec::new();
        let mut generated = Vec::new();
        let mut outside = HashSet::new();
        for (id, document) in documents {
            let uri = document.uri();
            // Asked of every document, not only the user's own, which is why it is a second list
            // and not a flag on the rows below.
            let elsewhere = environment::in_an_unloadable_tree(names, uri);
            if elsewhere {
                unloadable.push(*id);
            }
            // Asked of every document for `elsewhere`'s reason and answered in the same walk: a
            // prefix test against four lists, not a path split, so it rides along instead of
            // costing another pass.
            if layout.is_outside(uri) {
                outside.insert(*id);
            }
            if uri.starts_with(GENERATED_SCHEME) {
                generated.push((*id, UriId::from(source_of(uri))));
            }
            if is_own(uri) {
                own.insert(*id);
                each_own.push(Own {
                    id: *id,
                    directory: directory_of(uri).into(),
                    unloadable: elsewhere || names.in_a_test_tree(uri),
                });
            }
        }
        tracing::debug!(
            documents = documents.len(),
            own = own.len(),
            unloadable = unloadable.len(),
            generated = generated.len(),
            outside = outside.len(),
            "placed the graph's documents in {:.2?}",
            started.elapsed()
        );
        Self {
            own,
            each_own,
            unloadable,
            generated,
            outside,
        }
    }

    pub(super) fn own(&self) -> &HashSet<UriId> {
        &self.own
    }

    pub(super) fn each_own(&self) -> &[Own] {
        &self.each_own
    }

    pub(super) fn unloadable(&self) -> &[UriId] {
        &self.unloadable
    }

    pub(super) fn generated(&self) -> &[(UriId, UriId)] {
        &self.generated
    }

    pub(super) fn outside(&self) -> &HashSet<UriId> {
        &self.outside
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use rubydex::{
        model::{
            declaration::{
                Ancestor, Ancestors, ConstantDeclaration, Declaration, MethodDeclaration,
            },
            ids::DeclarationId,
        },
        resolution::Resolver,
    };

    use super::{HashSet, Indexed, LanguageId, OBJECT_MIXINS_URI, Placed, UriId};
    use crate::analysis::testing::{Harness, linked};
    use crate::{
        analysis::{
            environment::{Fence, Layout, Names},
            indexer,
        },
        workspace::DocUri,
    };

    /// The five shapes every branch of [`Placed::of`] depends on, in one graph.
    ///
    /// Hand-built instead of a `Harness` because the point is the *combinations* (a gem whose
    /// library holds a `test` segment, a template a gem ships, a migration the project wrote, a
    /// generated document and its source), and no real project holds all five at a readable size.
    fn a_graph_holding_every_shape() -> Indexed {
        let mut indexed = Indexed::default();
        for (uri, source) in [
            ("file:///p/app/models/store.rb", "class Store\nend\n"),
            (
                "file:///p/spec/models/store_spec.rb",
                "class FakeStore\nend\n",
            ),
            (
                "file:///p/db/migrate/20200101_add.rb",
                "class AddOne\nend\n",
            ),
            (
                "file:///g/pundit-2.3.1/lib/generators/pundit/install/templates/application_policy.rb",
                "class ApplicationPolicy\nend\n",
            ),
            (
                "file:///g/rack-test-2.1.0/lib/rack/test/utils.rb",
                "module Utils\nend\n",
            ),
            (
                "ya-lsp-generated:file:///p/app/models/store.rb#Store",
                "class Store\nend\n",
            ),
        ] {
            assert!(indexer::index_source(
                indexed.graph_mut(),
                uri,
                source,
                &LanguageId::Ruby,
            ));
        }
        indexed
    }

    fn placed_over(indexed: &Indexed) -> &Placed {
        indexed.placed(|graph| {
            Placed::of(
                graph,
                |uri| uri.starts_with("file:///p/"),
                Layout::default(),
            )
        })
    }

    #[test]
    fn the_placement_answers_what_each_rule_answers_of_each_document() {
        let indexed = a_graph_holding_every_shape();
        let placed = placed_over(&indexed);

        let id = UriId::from;
        let store = id("file:///p/app/models/store.rb");
        let spec = id("file:///p/spec/models/store_spec.rb");
        let migration = id("file:///p/db/migrate/20200101_add.rb");
        let template = id(
            "file:///g/pundit-2.3.1/lib/generators/pundit/install/templates/application_policy.rb",
        );
        let library = id("file:///g/rack-test-2.1.0/lib/rack/test/utils.rb");
        let generated = id("ya-lsp-generated:file:///p/app/models/store.rb#Store");

        // The user's own code is the three under the project root, and a generated document is
        // deliberately excluded: it is filed under a scheme, not a path, and excluding it keeps
        // generated definitions out of `rename`'s reach.
        let own: HashSet<UriId> = placed.own().clone();
        assert_eq!(own, HashSet::from([store, spec, migration]));

        // Read of the whole graph, not of `own`: a gem's generator template is copied, not loaded,
        // wherever it ships from. `rack-test` is the other half: a `test` segment inside a gem's
        // `lib/` is a published library, and this list must never hold it.
        let mut unloadable: Vec<UriId> = placed.unloadable().to_vec();
        unloadable.sort_unstable();
        let mut expected = vec![migration, template];
        expected.sort_unstable();
        assert_eq!(unloadable, expected);
        assert!(!placed.unloadable().contains(&library));

        assert_eq!(placed.generated(), [(generated, store)]);

        for own in placed.each_own() {
            let (directory, unloadable) = match own.id {
                x if x == store => ("file:///p/app/models", false),
                x if x == spec => ("file:///p/spec/models", true),
                x if x == migration => ("file:///p/db/migrate", true),
                _ => unreachable!("only the project's own three are rows"),
            };
            assert_eq!(&*own.directory, directory);
            assert_eq!(own.unloadable, unloadable, "{}", own.directory);
        }
    }

    /// The whole design, said as an assertion: an index of the graph dies with the graph.
    #[test]
    fn a_write_drops_both_of_the_indexes_beside_the_graph() {
        let mut indexed = a_graph_holding_every_shape();
        assert_eq!(indexed.members_named("size()").len(), 0);
        placed_over(&indexed);
        assert!(indexed.members_are_built() && indexed.placement_is_built());

        indexer::index_source(
            indexed.graph_mut(),
            "file:///p/app/models/order.rb",
            "class Order\nend\n",
            &LanguageId::Ruby,
        );

        assert!(
            !indexed.members_are_built() && !indexed.placement_is_built(),
            "taking the `&mut Graph` is what drops them"
        );
    }

    /// Found once, and again after anything it reads moves: the graph, whose code a document is,
    /// or the view convention.
    #[test]
    fn the_renderers_are_found_once_until_something_they_read_moves() {
        let mut indexed = a_graph_holding_every_shape();
        let found = std::cell::Cell::new(0);
        let laid = std::cell::Cell::new(0);
        // Each renderer's layouts are found from the renderers, so they go wherever those go.
        let ask = |indexed: &Indexed| {
            indexed.layouts(|| {
                laid.set(laid.get() + 1);
                std::rc::Rc::from([(
                    DeclarationId::from("Store"),
                    super::rails::Layouts::default(),
                )])
            });
            indexed.renderers(|| {
                found.set(found.get() + 1);
                std::rc::Rc::from([DeclarationId::from("Store")].as_slice())
            })
        };
        assert_eq!(*ask(&indexed), [DeclarationId::from("Store")]);
        ask(&indexed);
        assert_eq!((found.get(), laid.get()), (1, 1), "held across questions");
        indexed.forget_placement();
        ask(&indexed);
        assert_eq!(
            (found.get(), laid.get()),
            (2, 2),
            "whose code a document is may have moved"
        );
        indexed.forget_renderers();
        ask(&indexed);
        assert_eq!(
            (found.get(), laid.get()),
            (3, 3),
            "the view convention was rebuilt"
        );
        indexer::index_source(
            indexed.graph_mut(),
            "file:///p/app/models/order.rb",
            "class Order\nend\n",
            &LanguageId::Ruby,
        );
        ask(&indexed);
        assert_eq!((found.get(), laid.get()), (4, 4), "the graph moved");
    }

    /// Built once per object, side and fence, and again after anything it reads moves: the graph,
    /// or the layout under the fence.
    #[test]
    fn a_hierarchy_is_built_once_until_something_it_reads_moves() {
        let mut indexed = a_graph_holding_every_shape();
        let built = std::cell::Cell::new(0);
        let fence = |cursor| Fence::at(Some(cursor), Layout::default()).key();
        let app = fence("file:///p/app/models/store.rb");
        let ask = |indexed: &Indexed, class_side, fence| {
            indexed.hierarchy(DeclarationId::from("Store"), class_side, fence, || {
                built.set(built.get() + 1);
                None
            })
        };
        assert!(ask(&indexed, false, app).is_none());
        ask(
            &indexed,
            false,
            fence("file:///p/app/controllers/stores_controller.rb"),
        );
        assert_eq!(
            built.get(),
            1,
            "held across questions, a refusal too, for any cursor the fence treats alike"
        );
        ask(&indexed, true, app);
        assert_eq!(built.get(), 2, "the class object's side is another object");
        ask(
            &indexed,
            false,
            fence("file:///p/spec/models/store_spec.rb"),
        );
        assert_eq!(
            built.get(),
            3,
            "a spec keeps the tree rules off, so its own"
        );
        indexed.forget_placement();
        ask(&indexed, false, app);
        assert_eq!(built.get(), 4, "the layout under the fence may have moved");
        indexer::index_source(
            indexed.graph_mut(),
            "file:///p/app/models/order.rb",
            "class Order\nend\n",
            &LanguageId::Ruby,
        );
        ask(&indexed, false, app);
        assert_eq!(built.get(), 5, "the graph moved");
    }

    /// The other invalidation, which is the one that has to be remembered.
    #[test]
    fn a_question_that_moved_is_what_forget_placement_is_for() {
        // `[trees]` is an input to the tags, not to the graph, so nothing about the graph says the
        // held answer is wrong. Until `forget_placement` runs it still holds it, which is exactly
        // why the config reload calls it.
        let mut indexed = a_graph_holding_every_shape();
        let spec = UriId::from("file:///p/spec/models/store_spec.rb");
        let row = |indexed: &Indexed, names: Names<'_>| {
            let layout = Layout {
                names,
                ..Layout::default()
            };
            indexed
                .placed(|graph| Placed::of(graph, |uri| uri.starts_with("file:///p/"), layout))
                .each_own()
                .iter()
                .find(|own| own.id == spec)
                .expect("the spec is the project's own code")
                .unloadable
        };
        assert!(row(&indexed, Names::default()));

        let elsewhere = crate::workspace::config::TreesConfig {
            test: Some(vec!["qa".to_owned()]),
            ..crate::workspace::config::TreesConfig::default()
        };
        let moved = Names::of(&elsewhere);
        assert!(row(&indexed, moved), "the stale answer");
        indexed.forget_placement();
        assert!(
            !row(&indexed, moved),
            "asked again, and `spec/` is not a test tree under a replaced list"
        );
    }

    /// The fences' path memo, held beside the placement and dropped with it.
    #[test]
    fn the_paths_a_fence_read_are_held_until_the_layout_or_the_graph_moves() {
        // The same stale answer as above, by the other road: `Fence::unloadable` through the memo
        // `Analysis::layout` hands every request.
        let mut indexed = a_graph_holding_every_shape();
        let spec = "file:///p/spec/models/store_spec.rb";
        let unloadable = |indexed: &Indexed, names: Names<'_>| {
            let layout = Layout {
                names,
                held: Some(indexed.paths()),
                ..Layout::default()
            };
            Fence::at(Some("file:///p/app/models/store.rb"), layout).unloadable(spec)
        };
        assert!(unloadable(&indexed, Names::default()));
        assert_eq!(
            indexed.paths().len(),
            2,
            "the spec's answer, and the cursor's for the gate"
        );

        let elsewhere = crate::workspace::config::TreesConfig {
            test: Some(vec!["qa".to_owned()]),
            ..crate::workspace::config::TreesConfig::default()
        };
        let moved = Names::of(&elsewhere);
        assert!(unloadable(&indexed, moved), "the held answer");
        indexed.forget_placement();
        assert!(
            !unloadable(&indexed, moved),
            "read again under the replaced list"
        );

        assert!(indexed.paths().len() > 0);
        indexer::index_source(
            indexed.graph_mut(),
            "file:///p/app/models/order.rb",
            "class Order\nend\n",
            &LanguageId::Ruby,
        );
        assert_eq!(indexed.paths().len(), 0, "a write drops it too");
    }

    /// The seed is only a repair while it sorts first, and nothing in the type system enforces
    /// that.
    ///
    /// rubydex ranks a resolve's pending definitions by their document's lexicographic rank, so
    /// renaming the scheme to anything from `f` on would put the seed's `include Kernel` back
    /// behind the bundle's and quietly bring back the defect. The second half is the other part of
    /// the contract: a URI `DocUri` refuses can never reach a client.
    #[test]
    fn the_seed_uri_sorts_before_every_file_uri_and_cannot_be_opened() {
        assert!(
            OBJECT_MIXINS_URI < "file:",
            "{OBJECT_MIXINS_URI} must sort before every `file:` URI"
        );
        assert!(
            DocUri::from_graph_uri(OBJECT_MIXINS_URI).is_none(),
            "{OBJECT_MIXINS_URI} must never reach a client"
        );
    }

    /// The defect itself, in the smallest graph that can hold it.
    ///
    /// `Kernel`'s methods are private, so a `Kernel` in front of a module included into `Object`
    /// shadows that module's public methods, and they vanish from every list drawn from the chain
    /// (`DEBUGGER__::ForkInterceptor#fork` is the real case, leaving completion lists one item
    /// short). A `file:` URI here because that is what every gem in a real bundle has, and sorting
    /// before it is the seed's whole job.
    #[test]
    fn kernel_stays_behind_a_module_included_into_object() {
        let mut indexed = Indexed::default();
        assert!(indexer::index_source(
            indexed.graph_mut(),
            "file:///project/late.rb",
            "module Marker\nend\n\nclass Object\n  include Marker\nend\n",
            &LanguageId::Ruby,
        ));
        Resolver::new(indexed.graph_mut()).resolve();

        let chain: Vec<Ancestor> = indexed
            .declarations()
            .get(&DeclarationId::from("Object"))
            .and_then(Declaration::as_namespace)
            .unwrap()
            .ancestors()
            .iter()
            .copied()
            .collect();
        let names: Vec<&str> = chain
            .iter()
            .filter_map(|ancestor| match ancestor {
                Ancestor::Complete(id) => indexed.declarations().get(id).map(Declaration::name),
                Ancestor::Partial(_) => None,
            })
            .collect();

        assert_eq!(
            names,
            ["Object", "Marker", "Kernel", "BasicObject"],
            "`Kernel` is included at boot, so every later include sits above it"
        );
    }

    /// Neither shape below can come from rubydex, which is exactly why they are asserted.
    ///
    /// [`Members::of`] reads only method declarations and stops at a `#` with no following `)`, and
    /// both rest on how rubydex spells names, not on anything this file controls: `#` is composed
    /// in at one place and only for a method, an instance variable, a class variable or a global,
    /// and every site interning a method name appends `()`. A graph the crate cannot otherwise
    /// obtain is the only way to pin that: if either spelling changes upstream, the index would
    /// quietly answer a different list, and this fails first.
    #[test]
    fn a_name_no_method_could_have_keys_nothing() {
        let object = DeclarationId::from("Object");
        let mut indexed = Indexed::default();
        let declarations = indexed.graph_mut().declarations_mut();
        for (name, declaration) in [
            (
                "Foo#qux()",
                Declaration::Method(Box::new(MethodDeclaration::new(
                    "Foo#qux()".to_string(),
                    object,
                ))),
            ),
            (
                "Foo#bar",
                Declaration::Method(Box::new(MethodDeclaration::new(
                    "Foo#bar".to_string(),
                    object,
                ))),
            ),
            (
                "Foo#baz()",
                Declaration::Constant(Box::new(ConstantDeclaration::new(
                    "Foo#baz()".to_string(),
                    object,
                ))),
            ),
        ] {
            declarations.insert(DeclarationId::from(name), declaration);
        }

        assert_eq!(
            indexed.members_named("qux()").len(),
            1,
            "the one real shape"
        );
        assert!(indexed.members_named("bar").is_empty(), "a `#` with no `)`");
        assert!(
            indexed.members_named("bar()").is_empty(),
            "nor parenthesised"
        );
        assert!(indexed.members_named("baz()").is_empty(), "not a method");
    }

    /// A graph of `sources`, resolved and repaired as `analysis::resolve` does.
    fn repaired(sources: &[(&str, &str)]) -> Indexed {
        let mut indexed = Indexed::default();
        for (uri, source) in sources {
            assert!(indexer::index_source(
                indexed.graph_mut(),
                uri,
                source,
                &LanguageId::Ruby,
            ));
        }
        Resolver::new(indexed.graph_mut()).resolve();
        indexed.repair_superclasses();
        indexed
    }

    /// `name`'s chain state (`complete`, `partial` or `cut`), and every class and module in it, in
    /// order.
    fn chain_of(indexed: &Indexed, name: &str) -> (&'static str, Vec<String>) {
        let ancestors = indexed
            .declarations()
            .get(&DeclarationId::from(name))
            .and_then(Declaration::as_namespace)
            .unwrap_or_else(|| panic!("{name} is not a namespace"))
            .ancestors();
        let names = ancestors
            .iter()
            .filter_map(|ancestor| match ancestor {
                Ancestor::Complete(id) => indexed
                    .declarations()
                    .get(id)
                    .map(|declaration| declaration.name().to_owned()),
                Ancestor::Partial(_) => Some("?".to_owned()),
            })
            .collect();
        let state = match ancestors {
            Ancestors::Complete(_) => "complete",
            Ancestors::Partial(_) => "partial",
            Ancestors::Cyclic(_) => "cut",
        };
        (state, names)
    }

    fn descends(indexed: &Indexed, from: &str, below: &str) -> bool {
        indexed
            .declarations()
            .get(&DeclarationId::from(from))
            .and_then(Declaration::as_namespace)
            .is_some_and(|namespace| {
                namespace
                    .descendants()
                    .contains(&DeclarationId::from(below))
            })
    }

    const BASE: &str = "\
module Shared
end

module Front
end

module Audited
end

module Configurable
end

class ApplicationController
  include Shared
  extend Configurable

  def self.configure
  end
end
";

    const ADMIN: &str = "\
module Admin
  class ApplicationController < ApplicationController
    prepend Front
    include Shared
    include Audited

    def self.admin
    end
  end

  class UsersController < ApplicationController
    include Audited

    def self.users
    end
  end
end
";

    #[test]
    fn a_superclass_spelled_like_its_class_is_linked_to_the_class_ruby_names() {
        // Ruby reads `< ApplicationController` before `Admin::ApplicationController` exists, so it
        // names the top-level class. rubydex names the class being opened, and cuts the chain there
        // and below it. Every chain below is `Module#ancestors` as Ruby 4.0 prints it for these
        // files.
        let indexed = repaired(&[
            ("file:///p/app/controllers/application_controller.rb", BASE),
            ("file:///p/app/controllers/admin/base.rb", ADMIN),
        ]);
        let chain = |names: &[&str]| -> (&'static str, Vec<String>) {
            (
                "complete",
                names.iter().map(|name| (*name).to_owned()).collect(),
            )
        };
        // `Front` stays prepended, `Audited` is included, and `Shared` only where the parent has it:
        // an include of a module the superclass already has is skipped. The bare `include` names
        // resolve only after rubydex caches the cut chain, so they are read again.
        assert_eq!(
            chain_of(&indexed, "Admin::ApplicationController"),
            chain(&[
                "Front",
                "Admin::ApplicationController",
                "Audited",
                "ApplicationController",
                "Shared",
                "Object",
                "Kernel",
                "BasicObject",
            ])
        );
        // A class below the cut builds on it, and its own `include Audited` is skipped for the
        // same reason.
        assert_eq!(
            chain_of(&indexed, "Admin::UsersController"),
            chain(&[
                "Admin::UsersController",
                "Front",
                "Admin::ApplicationController",
                "Audited",
                "ApplicationController",
                "Shared",
                "Object",
                "Kernel",
                "BasicObject",
            ])
        );
        // And the other direction: everything above records everything below.
        for above in [
            "ApplicationController",
            "Shared",
            "Audited",
            "Front",
            "Object",
        ] {
            for below in ["Admin::ApplicationController", "Admin::UsersController"] {
                assert!(descends(&indexed, above, below), "{above} -> {below}");
            }
        }

        // The class side, from the parent's singleton, with the parent's `extend` in it.
        assert_eq!(
            chain_of(&indexed, "Admin::UsersController::<UsersController>"),
            chain(&[
                "Admin::UsersController::<UsersController>",
                "Admin::ApplicationController::<ApplicationController>",
                "ApplicationController::<ApplicationController>",
                "Configurable",
                "Object::<Object>",
                "BasicObject::<BasicObject>",
                "Class",
                "Module",
                "Object",
                "Kernel",
                "BasicObject",
            ])
        );
        assert!(descends(
            &indexed,
            "Configurable",
            "Admin::UsersController::<UsersController>"
        ));

        // Nothing is cut after the pass, so a second one writes nothing.
        let before = chain_of(&indexed, "Admin::UsersController");
        let mut again = indexed;
        again.repair_superclasses();
        assert_eq!(chain_of(&again, "Admin::UsersController"), before);
    }

    #[test]
    fn the_parent_is_found_where_ruby_finds_it() {
        // Checked against Ruby 4.0: the lexical scopes first, innermost out, then the ancestors of
        // the innermost, then the top level.
        let indexed = repaired(&[(
            "file:///p/lib/shapes.rb",
            "\
class X
end

module A
  class X
  end

  module Admin
    class X < X
    end
  end
end

class ApplicationPolicy
  class Scope
  end
end

class EventPolicy < ApplicationPolicy
  class Scope < Scope
  end
end

module Lonely
  class Thing < Thing
  end
end

class Plain
end

class Loud
  def self.shout
  end
end

module Admin
  class Plain < Plain
    def self.admin
    end
  end
end
",
        )]);
        // `A::X`, not the top-level `X`: an outer scope comes before the top level.
        assert_eq!(
            chain_of(&indexed, "A::Admin::X").1[..2],
            ["A::Admin::X", "A::X"]
        );
        // No `Scope` in any scope around it, so the ancestors of `EventPolicy` answer.
        assert_eq!(
            chain_of(&indexed, "EventPolicy::Scope").1[..2],
            ["EventPolicy::Scope", "ApplicationPolicy::Scope"]
        );
        // Ruby would raise here, so there is no chain to write: the cycle stays.
        assert_eq!(chain_of(&indexed, "Lonely::Thing").0, "cut");
        // A parent with no class method has no singleton in rubydex, so the nearest one above it
        // stands in. Ruby lists `#<Class:Plain>` second; it declares nothing, so nothing is lost.
        assert_eq!(
            chain_of(&indexed, "Admin::Plain::<Plain>"),
            (
                "complete",
                [
                    "Admin::Plain::<Plain>",
                    "Object::<Object>",
                    "BasicObject::<BasicObject>",
                    "Class",
                    "Module",
                    "Object",
                    "Kernel",
                    "BasicObject",
                ]
                .map(str::to_owned)
                .to_vec()
            )
        );
    }

    #[test]
    fn where_ruby_raises_the_cycle_stays() {
        // Each checked against Ruby 4.0, which raises at every one of these.
        let indexed = repaired(&[(
            "file:///p/lib/raises.rb",
            "\
module Outer
  module Thing
  end

  module Admin
    class Thing < Thing
    end
  end
end

class Thing
end

module Admin
  class Foo < Admin::Foo
  end
end

module Loop
  include Loop
end

class Base
  include Loop
end

module Admin
  class Base < Base
  end
end
",
        )]);
        // The first `Thing` the lookup meets is a module: `TypeError`, whatever is further out.
        assert_eq!(chain_of(&indexed, "Outer::Admin::Thing").0, "cut");
        // A qualified name cannot reach a class that does not exist yet: `NameError`.
        assert_eq!(chain_of(&indexed, "Admin::Foo").0, "cut");
        // The parent's own chain is a cycle, so there is nothing whole to build on.
        assert_eq!(chain_of(&indexed, "Admin::Base").0, "cut");
    }

    #[test]
    fn a_cut_class_s_own_mixins_follow_rubydex_s_rules() {
        let indexed = repaired(&[(
            "file:///p/lib/mixins.rb",
            "\
module Front
end

module Audited
end

module Loop
  include Loop
end

module Half
  include Missing
end

NOT_A_MODULE = 1

class Parent
end

module Admin
  class Parent < Parent
    prepend Front
    prepend Front
    include Audited
    include Audited
    include Front
    include NOT_A_MODULE
  end

  class Unknown < Parent
    include Gone
    prepend Lost
  end

  class Halved < Parent
    include Half
  end

  class Looped < Parent
    include Loop
  end
end
",
        )]);
        // A second `prepend Front` changes nothing, a second `include Audited` is skipped, and an
        // `include Front` of a module already prepended is skipped too, as Ruby 4.0 prints it. A
        // name that resolved to a value is skipped, as rubydex skips it.
        assert_eq!(
            chain_of(&indexed, "Admin::Parent").1[..4],
            ["Front", "Admin::Parent", "Audited", "Parent"]
        );
        // A name that never resolved is a partial entry where the module would go.
        assert_eq!(
            chain_of(&indexed, "Admin::Unknown"),
            (
                "partial",
                [
                    "?",
                    "Admin::Unknown",
                    "?",
                    "Front",
                    "Admin::Parent",
                    "Audited",
                    "Parent",
                    "Object",
                    "Kernel",
                    "BasicObject",
                ]
                .map(str::to_owned)
                .to_vec()
            )
        );
        // A module with an unresolved include of its own makes the chain partial; one that
        // includes itself leaves it a cycle, which is not this repair's to undo.
        assert_eq!(chain_of(&indexed, "Admin::Halved").0, "partial");
        assert_eq!(chain_of(&indexed, "Admin::Looped").0, "cut");
    }

    #[test]
    fn every_reader_sees_the_repaired_chain_and_it_survives_an_edit_to_either_side() {
        // Through the analysis thread, so the repair runs where `analysis::resolve` runs it, and
        // `definition` and `completion` read the graph the way they always do.
        let mut harness = Harness::new();
        let base_source = "class ApplicationController\n  def authenticate\n  end\nend\n";
        let base = harness.write("app/controllers/application_controller.rb", base_source);
        let admin_source = "\
module Admin
  class ApplicationController < ApplicationController
    def show
      authenticate
      audit
    end
  end
end
";
        let admin = harness.write("app/controllers/admin/base.rb", admin_source);
        harness.index();
        assert_eq!(
            linked(&harness.definition_at(&admin, admin_source, "authenticate")),
            ["application_controller.rb:1:6"]
        );
        let offered = harness.complete(&admin, &admin_source.replace("audit", "authen~"));
        assert!(
            offered["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["label"] == "authenticate")),
            "{offered}"
        );

        // An edit to the parent unlinks the class with the chain, and the next resolve links it
        // again, new method and all.
        let base_edited =
            "class ApplicationController\n  def authenticate\n  end\n\n  def audit\n  end\nend\n";
        harness.open(&base, base_source);
        harness.change(&base, base_edited);
        harness.open(&admin, admin_source);
        assert_eq!(
            linked(&harness.definition_at(&admin, admin_source, "audit")),
            ["application_controller.rb:4:6"]
        );

        // An edit to the class itself: rubydex cuts its chain again, and the pass repairs it again.
        let admin_edited = admin_source.replace("def show", "def edit\n    end\n\n    def show");
        harness.change(&admin, &admin_edited);
        assert_eq!(
            linked(&harness.definition_at(&admin, &admin_edited, "authenticate")),
            ["application_controller.rb:1:6"]
        );
    }
}
