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

use std::{cell::OnceCell, collections::HashSet, ops::Deref};

use rubydex::{
    indexing::{self, LanguageId},
    model::{
        graph::Graph,
        identity_maps::{IdentityHashBuilder, IdentityHashMap},
        ids::{DeclarationId, StringId, UriId},
    },
};

use super::{
    completion::directory_of,
    environment,
    synthesized::{GENERATED_SCHEME, source_of},
};

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
}

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
    /// contents, which [`Self::graph_mut`] invalidates.
    pub(super) fn forget_placement(&mut self) {
        self.placed.take();
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
            declaration::{Ancestor, ConstantDeclaration, Declaration, MethodDeclaration},
            ids::DeclarationId,
        },
        resolution::Resolver,
    };

    use super::{HashSet, Indexed, LanguageId, OBJECT_MIXINS_URI, Placed, UriId};
    use crate::{
        analysis::{
            environment::{Layout, Names},
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
}
