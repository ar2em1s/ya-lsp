//! The pass that reads what the workspace declares about itself and writes the RBS it implies.
//!
//! Every generator meets here: [`crate::knowledge::rails`] reads a `db/schema.rb` or a model's
//! macros, [`crate::knowledge::annotations`] reads a Sorbet `sig` or a YARD tag, each ends at a
//! [`Facts`](crate::generated::Facts), and one document per source file goes to
//! [`Synthesized::record`](synthesized::Synthesized::record).
//!
//! # What the generators may read, and what they may never wait for
//!
//! This runs **immediately before** `Resolver::resolve`, so its declarations are linked by the same
//! resolve, not a second one. The price, which every generator inherits: **declarations do not
//! exist yet; definitions are all there is to read.** A generator may ask which classes the
//! application defines and what a file's text says, but not what `User#name` resolves to, because
//! nothing has resolved.
//!
//! # Two phases; the second is `delegate`'s
//!
//! `delegate :name, to: :user` needs `Story#user -> User` and then `User#name -> String`, both
//! written in this pass into other files' documents.
//! [`Facts::returns`](crate::generated::Facts::returns) answers it: facts exist before any is
//! rendered, so a *second* phase can ask the first what it said.
//!
//! - **The table is the rails module's `delegate_declarations`**: the union of every phase-one
//!   fact, asked twice per `delegate`.
//! - **Built only when some file writes a `delegate`**, so projects without one pay nothing.
//! - **Built once**, after every phase-one generator has spoken, so no generator order is assumed.
//! - **Phase two's own output is not in it**, so a `delegate` whose target is another `delegate`
//!   answers `untyped`. The alternative is iterating to a fixed point over a graph a user can write
//!   a cycle into.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rubydex::model::{
    definitions::{Definition, Mixin, Receiver},
    document::Document,
    graph::Graph,
    ids::{DeclarationId, NameId, StringId, UriId},
    name::ParentScope,
};
use rubydex::query;

use super::{
    Analysis, environment, locator, locator::Site, render, synthesized, synthesized::Synthesized,
    views::Views,
};
use crate::generated::{At, Named, candidates};
use crate::knowledge::{self, Context, Contribution, ListId, Registry, Seen, Wants};
use crate::workspace::DocUri;

impl Analysis {
    /// Find the file Rails writes each generated query method in, now that the graph can say.
    ///
    /// - **Run after the resolve, never inside the pass.** The generators write the text rubydex is
    ///   about to link, so during the pass the graph holds four declarations (`Object`,
    ///   `BasicObject`, `Module`, `Class`) and every question about a gem's classes answers
    ///   nothing. The pass states *names*; this answers them, one step later in the same settle.
    /// - **Asked again every settle**, not cached across settles: a bundle can change under a
    ///   workspace, and a jump into a version that went away fails silently.
    /// - **Except for a settle whose only news is a reopened file** (`only`, from
    ///   [`Analysis::reopened_only`]): nothing but the documents it rewrote moved in the graph, so
    ///   every other document's places are what the last settle found, and only those are asked.
    /// - **Memoised within a settle.** The class side is written onto every base in the project
    ///   with the same names, so six bases ask the graph once and read the memo five times.
    pub(super) fn place_generated_members(&mut self, only: Option<&HashSet<UriId>>) {
        let asks = |document: &UriId| only.is_none_or(|only| only.contains(document));
        if !self
            .synthesized
            .named()
            .any(|(document, _)| asks(&document))
        {
            return;
        }
        let started = Instant::now();
        let mut rails = Owners::new(&self.knowledge);
        let layout = self.layout();
        // Collected before anything is written, because resolving reads the table places are
        // written into: `locator::places` uses it to tell a generated definition from one on disk,
        // and must see the same table for every member in one settle.
        let placed: Vec<(UriId, Vec<synthesized::Mapping>)> = self
            .synthesized
            .named()
            .filter(|(document, _)| asks(document))
            .map(|(document, named)| {
                (
                    document,
                    rails.mappings(&self.graph, &self.synthesized, layout, named),
                )
            })
            .collect();
        let found = placed.iter().map(|(_, found)| found.len()).sum::<usize>();
        let asked = placed.len();
        self.placings += asked as u64;

        for (document, found) in placed {
            self.synthesized.place(document, found);
        }
        tracing::debug!(
            "{found} of the members {asked} generated documents declare themselves have a \
             definition in the bundle, in {:.2?}",
            started.elapsed()
        );
    }
}

/// Where a generated member is really defined, for members whose place is in someone else's file.
///
/// - **Most generated members have a line the generator read**: a column's `t.string "title"` in
///   `db/schema.rb`, an association's `has_many :comments`. Some have none: nothing in a project
///   declares `Story.where`, so without this a *Resolved* card over one sends the reader nowhere.
/// - **They do have a definition, and the bundle is indexed**: `where` is a `def` in activerecord's
///   `relation/query_methods.rb`. So the place is found by asking the graph Ruby's question (walk
///   this class's ancestors for this name), which gives `Method#owner` by construction, since
///   rubydex built those ancestors from the framework's own `include`s.
/// - **A lookup, never a name match.**
///   [`Knowledge::places_members_on`](crate::knowledge::Knowledge::places_members_on) supplies the
///   classes, one list per module and side. A member is found on one of them or has no place. A
///   framework that moves a name loses its place and gains nothing wrong, and a project without an
///   indexed bundle is unaffected.
/// - **Callbacks are deliberately not looked up.** `before_save` is built by
///   `define_model_callbacks`, and jumping into machinery that defines a *family* of methods tells
///   a reader nothing about the one asked about (`workspace/rails/relations.rs`' argument). The
///   line is between *the file that defines this method* and *the file that defines methods*.
struct Owners {
    /// The owners to walk, in order, per side: instance, then class object.
    owners: [Vec<DeclarationId>; 2],
    /// `(singleton, name)` -> where Rails writes it, or that nothing does.
    ///
    /// Misses are cached as eagerly as hits. Most lookups miss (a project has many bases and one
    /// query-interface list), and re-asking the graph for a name that was absent last time is the
    /// easiest way to waste time here.
    found: HashMap<(bool, String), Option<Site>>,
}

impl Owners {
    /// Every registered module's owners, named once per settle.
    ///
    /// The names come from [`knowledge::Knowledge::places_members_on`] and the lookup is core's:
    /// this runs **after** the resolve, so the graph can answer, which a module may never assume
    /// while declaring.
    ///
    /// **Looked up by the exact name, never searched.** `declaration_search`'s `Exact` mode is
    /// `name.contains(query)`, so it answered every declaration *inside* an owner too, in hash
    /// order: `count` placed on `Relation::ExplainProxy`, `merge` on `Relation::Merger`, in every
    /// corpus. It also scanned the whole graph on every core once per name, 50–76 ms a settle. An
    /// owner the bundle lacks stays in the list and places nothing, since
    /// `find_member_in_ancestors` finds no declaration to walk.
    fn new(knowledge: &Registry) -> Self {
        let mut owners: [Vec<DeclarationId>; 2] = [Vec::new(), Vec::new()];
        for module in knowledge.modules() {
            for (side, names) in owners.iter_mut().zip(module.places_members_on()) {
                side.extend(names.iter().map(|name| DeclarationId::from(*name)));
            }
        }
        Self {
            owners,
            found: HashMap::new(),
        }
    }

    /// One `Mapping` per member this can place, nothing for the rest.
    fn mappings(
        &mut self,
        graph: &Graph,
        synthesized: &Synthesized,
        layout: environment::Layout<'_>,
        named: &[Named],
    ) -> Vec<synthesized::Mapping> {
        named
            .iter()
            .filter_map(|member| {
                let declared = self.site(graph, synthesized, layout, member)?;
                Some(synthesized::Mapping {
                    generated: member.generated,
                    declared,
                })
            })
            .collect()
    }

    fn site(
        &mut self,
        graph: &Graph,
        synthesized: &Synthesized,
        layout: environment::Layout<'_>,
        member: &Named,
    ) -> Option<Site> {
        let key = (member.singleton, member.name.clone());
        if let Some(cached) = self.found.get(&key) {
            return cached.clone();
        }
        let site = self.walk(graph, synthesized, layout, member);
        self.found.insert(key, site.clone());
        site
    }

    fn walk(
        &self,
        graph: &Graph,
        synthesized: &Synthesized,
        layout: environment::Layout<'_>,
        member: &Named,
    ) -> Option<Site> {
        // rubydex spells a method taking any arguments `where()` and one taking none `first`, and
        // this side cannot derive which: ya-lsp wrote an RBS parameter list, Rails wrote a `def`.
        // Both are asked, as `references` does with the same two spellings.
        let spellings = [
            StringId::from(member.name.as_str()),
            StringId::from(&*format!("{}()", member.name)),
        ];
        self.owners[usize::from(member.singleton)]
            .iter()
            .find_map(|owner| {
                // **A hit on the object model is not an answer**, [`locator::ruby_s_own`]'s rule:
                // every ancestor walk ends at `Object`, and `Kernel` alone declares `select`,
                // `format`, `open` and `test`. Without this, `select` would be answered with
                // `IO.select` in `core/kernel.rbs`: a confident card for a method nobody asked
                // about, the one failure this lookup must be incapable of.
                //
                // Inside the spelling loop, not around it, so a root hit for one spelling cannot
                // suppress the other's real answer.
                let found = spellings.iter().find_map(|spelling| {
                    query::find_member_in_ancestors(graph, *owner, *spelling, false)
                        .ok()
                        .filter(|found| !locator::ruby_s_own(graph, *found))
                })?;
                // The same list a jump would offer, so a borrowed place obeys every rule a read one
                // does: an `.rbs` stub loses to source beside it, and a second copy of a library
                // under a later load path is not a second place. No cursor, since this is decided
                // once for the project, which leaves the test-tree fence off (keeping more, not
                // less).
                locator::places(graph, synthesized, layout, found, None)
                    .into_iter()
                    .next()
            })
    }
}

/// The `StringId`s one document is filtered against, hashed once.
///
/// A struct, not two loop locals, because the loop body is callable for **one** document: the gate
/// re-asks [`Analysis::contribution`] of the document a keystroke touched, and building the filter
/// per call would hash every registered row's names for a single file.
struct Filters {
    /// Per registered row, the hashes of its `calls`.
    calls: Vec<Vec<StringId>>,
    /// Per registered row, the hashes of its `modules`.
    modules: Vec<Vec<StringId>>,
    /// Per registered row, the hashes of its `constants`.
    constants: Vec<Vec<StringId>>,
    /// Per registered row, its `spells` as bits of `Analysis::spelled`.
    spells: Vec<u64>,
}

impl Filters {
    fn new(knowledge: &Registry) -> Self {
        let hashes = |of: fn(&Wants) -> &'static [&'static str]| -> Vec<Vec<StringId>> {
            knowledge
                .wants()
                .iter()
                .map(|want| of(want).iter().map(|name| StringId::from(*name)).collect())
                .collect()
        };
        Self {
            calls: hashes(|want| want.calls),
            modules: hashes(|want| want.modules),
            constants: hashes(|want| want.constants),
            spells: (0..knowledge.wants().len())
                .map(|at| knowledge.spelled_by(at))
                .collect(),
        }
    }
}

/// How many documents each generator was handed, for the one log line saying the pass ran.
///
/// Only non-empty lists: a project with no `db/schema.rb` and no `config/routes.rb` should read
/// *models 412*, not five zeroes.
fn asked_for<'a>(documents: impl Iterator<Item = (&'a ListId, &'a Vec<String>)>) -> String {
    let named: Vec<String> = documents
        .filter(|(_, on)| !on.is_empty())
        .map(|(list, on)| format!("{list} {}", on.len()))
        .collect();
    if named.is_empty() {
        "no documents on any list".to_owned()
    } else {
        named.join(", ")
    }
}

/// The `N what, M what` of the log line that says the pass ran.
fn spelled(counted: &knowledge::Counted) -> String {
    counted
        .iter()
        .map(|(what, how_many)| format!("{how_many} {what}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a module reads any list only while the editor holds its documents
/// ([`Wants::buffers`]).
fn reads_buffers(module: &dyn knowledge::Knowledge) -> bool {
    module.wants().iter().any(|row| row.buffers)
}

/// The modules that read open buffers (`buffers`), or every other, through the three phases, into
/// a map of their own.
///
/// Two calls make a pass, the buffer-reading modules second. Neither group's phases see the
/// other's facts: a buffer-reading module's output is then a function of its own inputs alone, and
/// can be re-run alone when only a buffer opened or closed.
fn declare_all(
    knowledge: &mut Registry,
    declaring: &knowledge::Declaring<'_>,
    counted: &mut knowledge::Counted,
    buffers: bool,
) -> knowledge::Declared {
    let mut into = knowledge::Declared::new();
    let ours =
        |module: &&mut Box<dyn knowledge::Knowledge>| reads_buffers(module.as_ref()) == buffers;
    for module in knowledge.modules_mut().filter(ours) {
        counted.extend(module.conjure(declaring, &mut into));
    }
    for module in knowledge.modules_mut().filter(ours) {
        counted.extend(module.declare(declaring, &mut into));
    }
    // Nothing after this may declare; that is what phase three means.
    for module in knowledge.modules_mut().filter(ours) {
        counted.extend(module.derive(declaring, &mut into));
    }
    into
}

/// [`Analysis::defining_documents`]' memo: each of the project's own documents' `def`s of the names
/// last asked, by the document and its text's hash, and what decided which documents are the
/// project's.
///
/// Every pass asks (a routes file's bare calls), and a pass after an edit re-indexes one document:
/// reading every document's definitions again was a quarter of what that pass spent proving routes
/// on the largest corpus. A document whose hash moved is read again, one gone is let go, and other
/// names or another layout start over.
#[derive(Default)]
pub(super) struct Defining {
    wanted: BTreeSet<String>,
    layout: u64,
    documents: HashMap<UriId, (u64, Vec<String>)>,
    /// How many documents were read, for a test that asserts an unchanged one is not.
    pub(super) reads: usize,
}

/// Which classes each concern's macros really land on, resolved and then closed over.
///
/// 1. **Resolve.** An `include` names a constant, resolved against the nesting of the body that
///    wrote it: [`crate::generated::candidates`], the list an association's `class_name` uses. A
///    module the application does not define resolves to nothing and contributes nothing, so
///    `include Sidekiq::Worker` adds no row.
/// 2. **Close over.** `ActiveSupport::Concern` passes an inner concern's `included` block on to
///    whatever includes the outer one, so `Poll` including `Bigger` including `Expireable` really
///    gets `Expireable`'s `scope`. A module is walked *through*, not recorded, because nobody can
///    call `Bigger.expired`. `seen` guards against cycles in *source*: a module including itself is
///    a runtime `NoMethodError` and would be an infinite loop here.
fn includers_of(
    included: &[(String, String)],
    known: &BTreeSet<String>,
    modules: &BTreeSet<String>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut direct: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (owner, written) in included {
        let Some(target) = candidates(owner, written)
            .into_iter()
            .find(|candidate| known.contains(candidate))
        else {
            continue;
        };
        if modules.contains(&target) {
            direct.entry(target).or_default().insert(owner.clone());
        }
    }

    let mut includers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for module in direct.keys() {
        let mut classes: BTreeSet<String> = BTreeSet::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut queue: Vec<&str> = vec![module.as_str()];
        while let Some(current) = queue.pop() {
            for owner in direct.get(current).into_iter().flatten() {
                if !seen.insert(owner.as_str()) {
                    continue;
                }
                if modules.contains(owner) {
                    queue.push(owner.as_str());
                } else {
                    classes.insert(owner.clone());
                }
            }
        }
        if !classes.is_empty() {
            includers.insert(module.clone(), classes);
        }
    }
    includers
}

/// Every name a generated one could hang a segment off, so the walk below is bounded.
///
/// The proper prefixes of everything the application declares, minus names it declares itself, plus
/// the four constants this crate invents or looks up by name. That is exactly the set of namespaces
/// a generated owner can *introduce*: an owner is a name `Context` holds, a name derived from one
/// (`Comment::Relation`), or a module's own spellable names.
///
/// The bound is what makes this affordable: filtering on the last segment first and spelling only
/// matches costs a small fraction of spelling every namespace in the graph, because the filter is a
/// `StringId` compare and the walk never touches the tens of thousands of names nobody asked about.
fn wanted_namespaces(context: &Context, knowledge: &Registry) -> BTreeSet<String> {
    fn prefixes(name: &str, into: &mut BTreeSet<String>) {
        for (at, _) in name.match_indices("::") {
            into.insert(name[..at].to_owned());
        }
    }
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for name in &context.classes {
        prefixes(name, &mut wanted);
    }
    // Whatever each module writes onto that is not the application's own: names it needs spellable
    // that `classes` does not supply. Asked of the registry, so a build with nothing registered
    // asks the bundle only about its own prefixes.
    for module in knowledge.modules() {
        for name in module.spellable_names() {
            prefixes(name, &mut wanted);
            wanted.insert(name.to_owned());
        }
    }
    // **Every class a concern's class methods are written onto**, the pass's reach furthest outside
    // the application. `ActionController::Base` includes a dozen concerns and no user file declares
    // it, so without this its namespace is never asked about and `Namespaces::spellable` silently
    // declines the owner, for every member of every concern it includes. Only *prefixes* are
    // needed: `spellable` asks about the namespaces above a name, never the name.
    for includers in context.includers.values() {
        for name in includers {
            prefixes(name, &mut wanted);
        }
    }
    // A name the application declares needs no second opinion, and asking would let a gem's
    // `class Story` overrule this workspace's `module Story`.
    wanted.retain(|name| !context.classes.contains(name));
    wanted
}

/// A path inside an unpacked gem, from the gem's own directory down.
///
/// `…/gems/shouty-1.2.3/config/routes.rb` becomes `shouty-1.2.3/config/routes.rb`. The marker is a
/// directory literally named `gems`, which every layout `gems::gem_roots` knows ends in (a RubyGems
/// root, a vendored bundle, `bundler/gems` for git sources). `None` without such an ancestor, so
/// the caller keeps its own fallback.
pub(super) fn gem_relative(path: &Path) -> Option<std::path::PathBuf> {
    let mut here = path;
    while let Some(parent) = here.parent() {
        if parent.file_name() == Some(OsStr::new("gems")) {
            return path.strip_prefix(parent).ok().map(Path::to_path_buf);
        }
        here = parent;
    }
    None
}

/// Every class and module definition, by its name's last segment ([`Analysis::declarers`]), with
/// whether it is a `module`.
type Declarers = HashMap<StringId, Vec<(NameId, UriId, bool)>>;

/// What a file looked like when the pass read it ([`stamp_of`]).
type Stamp = Option<(std::time::SystemTime, u64)>;

/// The stamps a module's memo asked for while the pass read ([`Analysis::freshness`]), by the URI
/// it asked about, with the file's path: what [`Analysis::remember`] keeps without a second `stat`.
type Stamped = HashMap<String, (PathBuf, Stamp)>;

/// What a file looked like when the pass last read it: modification time and length.
///
/// `None` for a missing file, as a value, because a deleted schema and one that never existed must
/// compare unequal to one that did. Length too, because modification times are coarse and two
/// writes in one tick are a real edit.
fn stamp_of(path: &Path) -> Stamp {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

impl Analysis {
    /// Re-read everything the workspace declares about itself, and re-write the RBS it implies.
    ///
    /// - **Runs before every `resolve`** (cold index and debounce), because the inputs move
    ///   independently and almost none are watched: a schema edit changes columns, a `belongs_to`
    ///   changes a member, a *new model file* changes which classes anyone can name. (Only
    ///   `db/*structure.sql` is watched, because it is the one input that is not a graph document.)
    ///   Regenerating from everything each time makes "the answer is a function of what is on disk"
    ///   true, and it is *cheap* because [`synthesized::Synthesized::record`] hands nothing over
    ///   when neither text nor mappings changed.
    /// - **Six generators, one pass, one [`Facts`](crate::generated::Facts) per source file, one
    ///   generated document per *body*.** The schema (`db/*schema.rb` and `db/*structure.sql`, one
    ///   generator, one syntax type), model macros, hand-written annotations, mailer and job
    ///   conventions, the routing DSL, and `delegate` all end at a
    ///   [`Facts`](crate::generated::Facts); a file feeding two has both merged into the table its
    ///   URI names. `delegate` runs last, as a second *phase*, because it derives from what the
    ///   other five said.
    /// - **The cut into documents is [`Facts::split`](crate::generated::Facts::split), at render
    ///   time.** Precedence is settled on the whole file's facts and only rendering is partitioned,
    ///   so no generator knows about it. It exists because [`synthesized::Synthesized::record`] is
    ///   charged per re-indexed declaration: with the schema as one document a column edit
    ///   re-indexes every table; with one per body, one table.
    /// - **A file outside `index.include` is not read either.** Not indexed and not read are the
    ///   same thing, and this is not the place to overrule the configuration.
    /// - **Opening or closing a file only a buffer-reading module reads re-runs that module
    ///   alone** ([`Analysis::reopened_only`]); the generated documents it rewrote come back, for
    ///   the placing step to ask about them alone. `None` means anything may have moved.
    pub(super) fn synthesize(&mut self) -> Option<HashSet<UriId>> {
        if let Some(rewritten) = self.reopened_only() {
            return Some(rewritten);
        }
        let started = Instant::now();
        // **Two questions, which must stay two**: "would the walk produce the same projection" and
        // "has a file some generator reads changed". Answering the first with the second is right
        // for the generators and wrong for the walk: a keystroke in a model file changes what the
        // file *says*, not what the projection *is*, so the generators must run and the walk need
        // not.
        //
        // Both are asked of the projection already held, and that borrow must end before either
        // branch writes to `self`, so they are answered first and acted on after. `None` is the
        // first pass, where nothing is known.
        let (same, repeat) = match self.generated_from.as_ref() {
            Some(previous) => {
                let same = self.context_would_be_the_same(previous);
                (
                    same,
                    same && self.generators_would_repeat_themselves(previous),
                )
            }
            None => (false, false),
        };
        if repeat {
            tracing::debug!(
                "nothing the pass reads changed, in {:.2?} (no walk, {} touched)",
                started.elapsed(),
                self.touched.len()
            );
            self.touched.clear();
            self.reopened.clear();
            return None;
        }
        let walked = Instant::now();
        let context = if same {
            // The projection already held. **Taken, not cloned**: every path out of here ends at
            // `remember`, which puts it back. Per-document contributions stay: the gate has just
            // proved every document it could ask about contributes what it did last time, which
            // means the memo is still good.
            self.generated_from
                .take()
                .expect("`context_would_be_the_same` answered about a projection it holds")
        } else {
            self.walk()
        };
        let walk = walked.elapsed();
        // Only worth asking after a walk. Where the walk was reused, the projection is equal by
        // construction, and the other half of the gate just said no.
        if !same && self.pass_would_repeat_itself(&context) {
            // Not a cache or an incremental pass: the projection above is rebuilt in full and
            // compared, so only work whose *inputs* are provably unchanged is skipped. See
            // [`Analysis::pass_would_repeat_itself`].
            tracing::debug!(
                "nothing the pass reads changed, in {:.2?} ({walk:.2?} of it the walk, {} touched)",
                started.elapsed(),
                self.touched.len()
            );
            // The walk just ran, so the per-document contributions are fresh, and
            // `previous == context` is what got us here. `Analysis::walk` has already stored them,
            // which lets the *next* keystroke take the gate above; otherwise a document whose
            // contribution moved without changing the merged answer would fail the cheap comparison
            // for the rest of the session.
            self.touched.clear();
            self.reopened.clear();
            return None;
        }
        self.passes += 1;
        // Every file a generator is about to open is read and parsed here and only here, and only
        // if its text moved since the last pass.
        let stamped = self.refresh_sources(&context, None);
        if context.is_empty(&self.knowledge) && self.generated.is_empty() {
            // The view context is rebuilt too, with no sources rather than skipped: its helpers
            // half is a projection of the walk above and costs nothing, and a workspace whose last
            // macro was just deleted must *lose* the exports it had.
            self.views = self.view_context(&context);
            self.graph.forget_renderers();
            self.remember(context, stamped);
            return None;
        }

        self.views = self.view_context(&context);
        self.graph.forget_renderers();
        // **Every registered module, through the three phases**; core knows nothing else about
        // order. What a module's generators owe each other is the module's business; what they owe
        // *another* module's is what the phases are. Taken out and put back for `refresh_sources`'
        // reason: the view below borrows the rest of `self` while a module writes to itself.
        //
        // **The modules that read open buffers go last, into a map of their own**, so no other
        // module's phase reads what they said and they read nobody's. That makes what they write a
        // function of their own inputs, which is what lets a reopened file re-run them alone
        // ([`Analysis::reopened_only`]) and get what this pass would have.
        let mut knowledge = std::mem::take(&mut self.knowledge);
        let mut counted: knowledge::Counted = Vec::new();
        // What this pass reads replaces what the last one did; a reopening only adds to it.
        self.declared_from.borrow_mut().clear();
        let (mut generated, buffered) = self.declaring(&context, |declaring| {
            (
                declare_all(&mut knowledge, declaring, &mut counted, false),
                declare_all(&mut knowledge, declaring, &mut counted, true),
            )
        });
        self.knowledge = knowledge;
        self.unbuffered = generated.keys().cloned().collect();
        self.buffered = buffered.keys().cloned().collect();
        self.shared = generated
            .iter()
            .filter(|(source, _)| self.buffered.contains(*source))
            .map(|(source, held)| (source.clone(), held.clone()))
            .collect();
        for (uri, facts) in buffered.into_values() {
            knowledge::add(&mut generated, &uri, facts);
        }

        let (kept, documents, _) = self.record_generated(generated, &context);
        self.forget_stale(&kept);

        // Debug, not info: this runs before every resolve, one line per settle, and would drown an
        // ordinary session's log. These numbers exist nowhere else, and a report about this feature
        // needs them.
        //
        // `lists` is what the walk handed the generators; the counts after are what they made of
        // it. Together they answer "did a generator run, and on what", which is how a switched-off
        // generator becomes visible.
        let lists = asked_for(context.documents.iter());
        let declared = spelled(&counted);
        tracing::debug!(
            "{} files declare {declared}, into {documents} generated documents, from {lists}, in \
             {:.2?} ({walk:.2?} of it the walk)",
            kept.len(),
            started.elapsed()
        );
        self.remember(context, stamped);
        None
    }

    /// Run `declare` with what a generator may read while it declares, over one projection.
    ///
    /// **One walk of the definitions for every module's `declares`**, built on the first question
    /// and kept for the rest: nothing here writes to the graph.
    fn declaring<R>(
        &self,
        context: &Context,
        declare: impl FnOnce(&knowledge::Declaring<'_>) -> R,
    ) -> R {
        let declarers = OnceCell::new();
        declare(&knowledge::Declaring {
            context,
            features: self.workspace.features(),
            text: &|uri| {
                self.declared_from
                    .borrow_mut()
                    .insert(uri.as_str().to_owned());
                self.with_text(uri, |text| text.text().to_owned())
            },
            caption: &|uri| self.workspace_relative(uri),
            own: &|uri| self.is_own_code(uri),
            declares: &|wanted| {
                self.declaring_documents(declarers.get_or_init(|| self.declarers()), wanted)
            },
            kinds: &|wanted| {
                self.declared_kinds(declarers.get_or_init(|| self.declarers()), wanted)
            },
            loaded: &|uri| !environment::Fence::at(None, self.layout()).unloadable(uri.as_str()),
            methods: &|wanted| self.defining_documents(wanted),
        })
    }

    /// Which of the project's own documents write a `def` of each of these names.
    ///
    /// Read from the definitions, as every question the pass asks is (the declarations are one
    /// settle behind), and only the user's own documents', which are few beside the bundle's.
    /// **Each document's answer is held by its text's hash** ([`Defining`]): a pass after an edit
    /// reads the one document that moved, not every document's definitions.
    fn defining_documents(&self, wanted: &BTreeSet<String>) -> BTreeMap<String, Vec<DocUri>> {
        // Which documents are the project's own is decided by these three, and only by them.
        let layout = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (
                &self.workspace_prefix,
                &self.own_prefixes,
                &self.foreign_prefixes,
            )
                .hash(&mut hasher);
            hasher.finish()
        };
        let mut memo = self.defining.borrow_mut();
        if memo.wanted != *wanted || memo.layout != layout {
            memo.wanted = wanted.clone();
            memo.layout = layout;
            memo.documents.clear();
        }
        // A method definition's name carries its parentheses, as its declaration's does.
        let names: HashMap<StringId, &String> = wanted
            .iter()
            .map(|name| (StringId::from(format!("{name}()").as_str()), name))
            .collect();
        let mut seen: HashSet<UriId> = HashSet::new();
        let mut found: BTreeMap<String, Vec<DocUri>> = BTreeMap::new();
        for (id, document) in self.graph.documents() {
            if document.uri().starts_with(synthesized::GENERATED_SCHEME)
                || !self.is_own_code(document.uri())
            {
                continue;
            }
            seen.insert(*id);
            let hash = document.content_hash();
            if memo.documents.get(id).is_none_or(|(held, _)| *held != hash) {
                let mut writes: Vec<String> = document
                    .definitions()
                    .iter()
                    .filter_map(|id| match self.graph.definitions().get(id) {
                        Some(Definition::Method(method)) => {
                            names.get(method.str_id()).map(|name| (*name).clone())
                        }
                        _ => None,
                    })
                    .collect();
                writes.sort();
                writes.dedup();
                memo.reads += 1;
                memo.documents.insert(*id, (hash, writes));
            }
            let writes: &Vec<String> = &memo.documents[id].1;
            if writes.is_empty() {
                continue;
            }
            let Some(uri) = DocUri::from_graph_uri(document.uri()) else {
                continue;
            };
            for name in writes {
                found.entry(name.clone()).or_default().push(uri.clone());
            }
        }
        memo.documents.retain(|id, _| seen.contains(id));
        found
    }

    /// A settle whose only news is that the editor opened or closed files that modules read only
    /// while open: re-run those modules, and nothing else. `None` where that is not the settle, or
    /// not safe, and the whole pass runs instead.
    ///
    /// - **Why.** Opening a spec file ran the whole pass (every model, the schema, every struct,
    ///   then placing every generated member), for one file's example groups: a third of a second
    ///   per open on the largest corpus, spent re-deriving what nothing had changed.
    /// - **Sound because nothing else moved.** Every touched document is only reopened (the graph
    ///   holds the text it held), nothing bulk was indexed, the projection is the one held, and
    ///   every file a generator read is unchanged on disk: the full pass's own two gates, asked
    ///   of the reopened documents. Every other module would write what it wrote, and none of
    ///   them reads a buffer-reading module's output ([`declare_all`]'s order).
    /// - **A source both kinds write keeps the other modules' facts** from the last whole pass
    ///   ([`Analysis::shared`]), merged back in in the whole pass's order: a spec support file's
    ///   `Struct.new` beside its shared groups. Only a source both write for the first time (a spec
    ///   with a `Struct.new`, opened) takes the whole pass, which holds it from then on.
    fn reopened_only(&mut self) -> Option<HashSet<UriId>> {
        if self.touched_all
            || self.reopened.is_empty()
            || !self.touched.iter().all(|uri| self.reopened.contains(uri))
        {
            return None;
        }
        let started = Instant::now();
        let previous = self.generated_from.as_ref()?;
        if !self.context_would_be_the_same(previous)
            || !self.stamps.iter().all(|(path, was)| stamp_of(path) == *was)
        {
            return None;
        }
        let context = self.generated_from.take()?;
        self.refresh_sources(&context, Some(true));
        let mut knowledge = std::mem::take(&mut self.knowledge);
        let mut counted: knowledge::Counted = Vec::new();
        let buffered = self.declaring(&context, |declaring| {
            declare_all(&mut knowledge, declaring, &mut counted, true)
        });
        self.knowledge = knowledge;
        if buffered
            .keys()
            .any(|source| self.unbuffered.contains(source) && !self.shared.contains_key(source))
        {
            self.generated_from = Some(context);
            return None;
        }
        self.passes += 1;
        self.reopenings += 1;
        let written_by_these: HashSet<String> = buffered.keys().cloned().collect();
        let mut merged = self.shared.clone();
        for (uri, facts) in buffered.into_values() {
            knowledge::add(&mut merged, &uri, facts);
        }
        let (kept, documents, written) = self.record_generated(merged, &context);
        let stale: Vec<DocUri> = self
            .buffered
            .iter()
            .filter(|source| !kept.contains(*source))
            .filter_map(|source| DocUri::from_graph_uri(source))
            .collect();
        for source in stale {
            self.synthesized.forget(self.graph.graph_mut(), &source);
        }
        self.generated = self.unbuffered.union(&kept).cloned().collect();
        self.buffered = written_by_these;
        let declared = spelled(&counted);
        tracing::debug!(
            "{} reopened: {} files declare {declared}, into {documents} generated documents, in \
             {:.2?} (the other modules not run)",
            self.reopened.len(),
            kept.len(),
            started.elapsed()
        );
        self.generated_from = Some(context);
        self.touched.clear();
        self.reopened.clear();
        Some(written)
    }

    /// Render each source's facts, split into one document per body, and record them: the
    /// sources that declared something, how many documents changed, and every generated document
    /// written.
    fn record_generated(
        &mut self,
        generated: knowledge::Declared,
        context: &Context,
    ) -> (HashSet<String>, usize, HashSet<UriId>) {
        let mut kept: HashSet<String> = HashSet::new();
        let mut written: HashSet<UriId> = HashSet::new();
        let mut documents = 0;
        for (uri, facts) in generated.into_values() {
            // **Split, then rendered: one document per body.** The split is of the rendering, never
            // the generation: every generator has spoken and every collision is settled, so nothing
            // here can re-decide a rank (see [`Facts::split`](crate::generated::Facts::split)). It
            // sets the unit of invalidation, since [`Synthesized::record`] is charged per
            // re-indexed declaration: a column changing type re-indexes one table, not the whole
            // schema.
            //
            // Rendered here only, so every span is computed against the final text and no
            // generator's offsets need shifting by another's length.
            let parts: Vec<synthesized::Part> = facts
                .split()
                .into_iter()
                .map(|(body, facts)| {
                    let declarations = facts.render(&context.namespaces);
                    let mappings = declarations
                        .spans
                        .iter()
                        .map(|span| synthesized::Mapping {
                            generated: span.generated,
                            declared: Site {
                                uri: uri.as_str().to_owned(),
                                full: span.declared,
                                selection: span.selection,
                            },
                        })
                        .collect();
                    synthesized::Part {
                        body,
                        rbs: declarations.rbs,
                        mappings,
                        named: declarations.named,
                        ran: declarations.ran,
                    }
                })
                .collect();
            if let Ok(needle) = std::env::var("YA_LSP_DUMP")
                && uri.as_str().contains(&needle)
            {
                for part in &parts {
                    eprintln!("=== {} {} ===\n{}", uri.as_str(), part.body, part.rbs);
                }
            }
            let recorded =
                self.synthesized
                    .record(self.graph.graph_mut(), &mut self.types, &uri, parts);
            documents += recorded.len();
            written.extend(
                recorded
                    .iter()
                    .map(|document| UriId::from(document.as_str())),
            );
            kept.insert(uri.as_str().to_owned());
        }
        (kept, documents, written)
    }

    /// Whether the generators would write exactly what they wrote last time.
    ///
    /// - **Why.** `settle` runs this pass before every `resolve`, and a forced settle precedes
    ///   every graph-reading request, so without this gate a keystroke in a file with no macros
    ///   would pay for a whole-workspace regeneration that comes out byte-identical. Far worse is a
    ///   document whose text really moved: re-indexing one model's RBS into a settled graph costs
    ///   much more, and the whole schema more still (`synthesized.md` has the numbers).
    /// - **Two questions, both must be no.** An equal `Context` means equal generator arguments;
    ///   that catches a new class defined elsewhere, which a naive "did *this* document declare
    ///   anything" test would miss (`has_many :widgets` declines until another file writes
    ///   `class Widget`). The files themselves are the second half, because a `Context` says which
    ///   documents a generator opens, never their contents: `has_many :comments` becoming
    ///   `has_many :notes` is one document on one list either way.
    /// - **What is skipped** is reading, parsing, rendering and recording every listed file. The
    ///   **walk is not skipped here**, because building this gate's evidence is the walk.
    ///   [`Analysis::context_would_be_the_same`] runs first and needs no walk. This gate stays
    ///   because it is strictly wider (it catches a document whose contribution moved without
    ///   moving the merged answer), and it is only asked after a real walk, since an equal
    ///   projection makes it trivially true.
    fn pass_would_repeat_itself(&self, context: &Context) -> bool {
        let Some(previous) = self.generated_from.as_ref() else {
            return false;
        };
        // A bulk index (workspace walk, gem batch, watched file, rebuild) says nothing about
        // *which* documents moved, so it is never skipped. Only the buffer path reports one
        // document: the keystroke path this gate is for.
        if self.touched_all || previous != context {
            return false;
        }
        self.generators_would_repeat_themselves(previous)
    }

    /// Whether nothing a generator *reads* has changed: the half of the gate about files, not the
    /// projection. Its own function because both gates ask it.
    ///
    /// 1. **Was a listed file touched, or one a generator read through `Declaring::text`?** A
    ///    `Context` says which documents a generator opens, never what is in them, so touching the
    ///    file is the whole answer. A routes file's helper, a file a `draw` names and a module a
    ///    controller includes are on no list; what the last pass read of them is
    ///    (`Analysis::declared_from`), or an edit to one alone ran no pass and left the proofs
    ///    stale.
    /// 2. **Is every file it read still there, unchanged?** Not belt and braces: the pass claims
    ///    its answer is a function of what is on disk, so a `git checkout` deleting `db/schema.rb`
    ///    must stop the columns answering at the next settle, before any watcher notification
    ///    arrives. One `stat` per file read buys that back, and the parse memo rests on the same
    ///    `stat`.
    fn generators_would_repeat_themselves(&self, previous: &Context) -> bool {
        let declared_from = self.declared_from.borrow();
        if self.touched.iter().any(|uri| {
            declared_from.contains(uri) || previous.read_by_a_generator().any(|read| read == uri)
        }) {
            return false;
        }
        self.stamps.iter().all(|(path, was)| stamp_of(path) == *was)
    }

    /// Whether the walk would produce the `Context` already held: the gate that runs *before* the
    /// walk.
    ///
    /// [`Analysis::pass_would_repeat_itself`] compares the whole merged `Context`, which only
    /// exists after paying for the walk. This asks the same question of **one document** and pays
    /// for one.
    ///
    /// **It must not borrow the other gate's file clause.** "Is a touched file on a generator's
    /// list" is about what a generator *reads*, not what the walk *builds*. Asked alone, it would
    /// say yes for the commonest Rails edit, and the projection would be reused while the
    /// generators run.
    ///
    /// **Three properties make one document's `Contribution` enough:**
    ///
    /// 1. **The merged `Context` depends on the *set* of contributions**, not the order the graph
    ///    hands them over. [`Context::absorb`] and [`Context::settle`] pay for that.
    ///    [`Analysis::walk`] relies on the same property.
    /// 2. **A document nothing re-indexed cannot contribute anything different.** Only
    ///    `Analysis::index_buffer` names a document; every bulk route sets `touched_all` and is
    ///    refused here (the other gate's narrowing, reused).
    /// 3. **A document the walk does not *visit* is refused**, because
    ///    [`Analysis::bundle_namespaces`] reads every definition in the graph: an `.rbs` in the
    ///    project's `sig/` contributes nothing to the walk yet can still move a `Context`.
    ///
    /// The one input that is **not** a projection of the graph is asked directly:
    /// `db/*structure.sql`, which is not a document, so no `Contribution` can reach it (a
    /// `read_dir`, not a walk). The stamps of files a generator read are deliberately *not* checked
    /// here; that belongs to [`Analysis::generators_would_repeat_themselves`], asked beside this.
    fn context_would_be_the_same(&self, previous: &Context) -> bool {
        if self.touched_all {
            return false;
        }
        let filters = Filters::new(&self.knowledge);
        for uri in &self.touched {
            // One `let … else`, not two: "the graph has no such document" and "the walk does not
            // visit it" give the same answer here (no stored contribution, so the pass must run).
            // Asked separately, the comparison below would be unsound: two `None`s are equal, so a
            // document with no entry would match one that contributed nothing.
            let id = UriId::from(uri.as_str());
            let Some(now) = self
                .graph
                .documents()
                .get(&id)
                .and_then(|document| self.contribution(document, &filters))
            else {
                return false;
            };
            if self.contributions.get(&id) != Some(&now) {
                return false;
            }
        }
        // Not a projection of the graph, so nothing above covers it: a file nothing indexed has no
        // `Contribution`, which is what `Knowledge::discover` is for.
        let reading = knowledge::Reading {
            root: self.workspace.root(),
            admits: &|path| self.workspace.admits(path),
            features: self.workspace.features(),
            gems: self.workspace.gems_found(),
            i18n: &self.workspace.config().i18n,
        };
        self.knowledge
            .modules()
            .zip(&previous.projections)
            .all(|(module, projection)| {
                let mut found = module.discover(&reading);
                found.sort_unstable();
                projection
                    .0
                    .as_ref()
                    .is_none_or(|held| held.also_reads() == found)
            })
    }

    /// Hold what this pass read from, for the next settle's gates: the projection, and a stamp of
    /// every file a generator read from disk.
    ///
    /// - **A file's stamp is the one its module's memo took** (`stamped`), before the read: one
    ///   `stat` a file a settle, where there were two, and a file written while the pass ran then
    ///   differs at the next settle rather than passing for what was read.
    /// - **Only what is read from disk** ([`Context::read_from_disk`]): a spec file is read only
    ///   while the editor holds it, from the buffer, so a large suite cost thousands of `stat`s a
    ///   settle for files nothing opened.
    /// - A file on two lists is stamped once.
    fn remember(&mut self, context: Context, mut stamped: Stamped) {
        let mut seen: HashSet<&str> = HashSet::new();
        let stamps = context
            .read_from_disk(&self.knowledge)
            .filter(|uri| seen.insert(uri))
            .filter_map(|uri| {
                if let Some(taken) = stamped.remove(uri) {
                    return Some(taken);
                }
                let path = DocUri::from_graph_uri(uri)?.to_file_path()?;
                let stamp = stamp_of(&path);
                Some((path, stamp))
            })
            .collect();
        self.stamps = stamps;
        self.generated_from = Some(context);
        self.touched.clear();
        self.reopened.clear();
        self.touched_all = false;
    }

    /// The walk's answer alone, **projected cold**, for tests that assert on what the walk builds
    /// rather than what a generator did with it.
    ///
    /// The memo is dropped first, so this is the walk with nothing held: exactly the answer a
    /// memoised walk must equal, which lets a test assert it does.
    #[cfg(test)]
    pub(super) fn context(&mut self) -> Context {
        self.contributions.clear();
        self.walk()
    }

    /// Everything the generators need from the graph, in one pass over the user's code, keeping
    /// every document's projection so the next walk need not rebuild it.
    ///
    /// - **One pass, not one per generator.** Each wants a different projection of the same
    ///   documents, and every projection is a filter over what indexing recorded. The projections
    ///   are `WANTS`; `modules` and `superclasses`, which no generator reads yet, are collected
    ///   because they cost the same loop.
    /// - **The loop is incremental; nothing after it is.** [`Analysis::contribution`] was the
    ///   walk's whole cost, and a document rubydex has not re-indexed gives the same answer as last
    ///   time. What a memo cannot touch is below the loop: [`Context::absorb`] is a fold with no
    ///   inverse, and [`includers_of`] and [`Analysis::bundle_namespaces`] fold over the whole
    ///   projection. That is a floor, not a curve.
    /// - **Measured**: holding contributions cuts the walk to roughly a third on large apps, and
    ///   the pass around it roughly in half, because a keystroke adding a class name moves the
    ///   projection but almost never a generated document's text, so `Synthesized::record` hands
    ///   nothing over and the walk is what is left. The other kind of keystroke (an `attribute`
    ///   re-declaring one document) is dominated by re-indexing that document's RBS, and the walk
    ///   does not run at all.
    /// - **Keyed on "has rubydex re-indexed this document", never a file stamp.**
    ///   [`Analysis::contribution`] reads the graph, never a file, the bound this pass inherits; a
    ///   `stat`-keyed memo would break that and cost a syscall per document per settle.
    ///   `self.touched` and `self.touched_all` already carry the right question, so this is the
    ///   same mechanism as [`Analysis::context_would_be_the_same`], asked of every document, with
    ///   no new invalidation rule.
    /// - **The one non-document input is named explicitly.** `is_own_code` and
    ///   `is_generator_source` read `engine_prefixes`, a property of the bundle, so gem discovery
    ///   drops the whole map when it writes them; otherwise a former engine's `app/` could stay
    ///   admitted.
    fn walk(&mut self) -> Context {
        self.walks += 1;
        let started = Instant::now();
        // Invalidation, reusing the gate's narrowing. A bulk route does not say *which* documents
        // moved, so the whole map goes; only `Analysis::index_buffer` names one, on the keystroke
        // path.
        if self.touched_all {
            self.contributions.clear();
        } else {
            for uri in &self.touched {
                self.contributions.remove(&UriId::from(uri.as_str()));
            }
        }
        let filters = Filters::new(&self.knowledge);
        let mut context = Context::new(&self.knowledge);
        // `(the body that wrote the include, the constant it spelled)`, resolved after the loop;
        // see [`Context::includers`].
        let mut included: Vec<(String, String)> = Vec::new();
        // Drained, not written through, so a document the graph no longer holds takes its entry
        // with it. Whatever remains in `held` after the loop is a removed document, and is dropped.
        let mut held = std::mem::take(&mut self.contributions);
        let mut contributions: HashMap<UriId, Contribution> = HashMap::with_capacity(held.len());
        let mut projecting = std::time::Duration::ZERO;
        let mut projected = 0_usize;
        for (uri_id, document) in self.graph.documents() {
            let contribution = match held.remove(uri_id) {
                Some(contribution) => contribution,
                None => {
                    // Timed around the miss, not the loop, so this costs two clock reads per
                    // *re-projected* document: two on a keystroke, and negligible on a cold walk
                    // that holds nothing anyway.
                    let at = Instant::now();
                    let fresh = self.contribution(document, &filters);
                    projecting += at.elapsed();
                    projected += 1;
                    match fresh {
                        Some(contribution) => contribution,
                        // Not memoised, and that absence matters: a document the walk does not
                        // **visit** must have no entry, because that is the case the gate cannot
                        // reason about. See [`Analysis::context_would_be_the_same`].
                        None => continue,
                    }
                }
            };
            context.absorb(document.uri(), &contribution, &mut included);
            // Every document the walk **visits** gets an entry, including one that contributes
            // nothing.
            contributions.insert(*uri_id, contribution);
        }
        let visited = contributions.len();
        self.contributions = contributions;
        let absorbed = started.elapsed();
        // The one place a gem's own names are read. A concern's includer is often a gem class
        // (`ActiveRecord::Base` includes `ActiveModel::API`), so the chain runs through names no
        // user file writes.
        let known: BTreeSet<String> = context
            .classes
            .union(&context.foreign_classes)
            .cloned()
            .collect();
        let modules: BTreeSet<String> = context
            .modules
            .union(&context.foreign_modules)
            .cloned()
            .collect();
        let at = Instant::now();
        context.includers = includers_of(&included, &known, &modules);
        let includers = at.elapsed();
        // **Files nothing indexed**, found once per pass: a `db/*structure.sql` is not a graph
        // document, so no contribution or list can name it. Taken out and put back because
        // discovery reads the workspace while the module writes to itself.
        let mut knowledge = std::mem::take(&mut self.knowledge);
        {
            let reading = knowledge::Reading {
                root: self.workspace.root(),
                admits: &|path| self.workspace.admits(path),
                features: self.workspace.features(),
                gems: self.workspace.gems_found(),
                i18n: &self.workspace.config().i18n,
            };
            let found: Vec<Vec<DocUri>> = knowledge
                .modules()
                .map(|module| {
                    let mut found = module.discover(&reading);
                    found.sort_unstable();
                    found
                })
                .collect();
            for (module, found) in knowledge.modules_mut().zip(found) {
                module.discovered(found);
            }
        }
        self.knowledge = knowledge;
        // **What only the whole walk decides**, per module: whether a class is a model depends on
        // the chain above it, and the file defining one joins the list that opens it. Before the
        // feature gate below, so a switched-off list drops whatever joined it here too.
        for module in self.knowledge.modules() {
            module.after_the_walk(&mut context);
        }
        // **`contribution`'s gate, applied where that one cannot reach.** The hook above decides
        // membership *after* the walk, so a document with no macros can land on the model list
        // without passing the per-document test, and a `[rails] models = false` that only filtered
        // rows would keep reading every model. The loop's gate stays as the cheap half: it stops
        // the predicates running per document per switched-off list.
        context
            .documents
            .retain(|list, _| self.knowledge.wanted(*list, self.workspace.features()));
        // One walk instead of three lookups, because `Graph::get` reads the **declarations**, which
        // `Resolver::resolve` builds and this pass runs before, so it answers a settle late: at
        // this point only a sliver of the definitions have declarations. The definitions are all
        // there; only the index over them is not.
        let at = Instant::now();
        let namespaces = self.bundle_namespaces(&wanted_namespaces(&context, &self.knowledge));
        let bundled = at.elapsed();
        for (name, module) in namespaces {
            context.namespaces.declare(name, module);
        }
        // A directory's conjured name is kept only where **no `class` declares it** (a `user.rb`
        // writing `class User` beside the `user/` directory, in the application or a gem), since a
        // conjured body is a `module`. So the filter runs here, after both the application's names
        // and the bundle's are recorded, never in the loop above where neither set is complete. Which framework classes may be written onto is
        // likewise decided by what the bundle holds.
        //
        // The survivors are then declared, which makes every prefix of a conjured name spellable:
        // `Chat::Thread::Policy` needs `Chat::Thread`, which is either declared already or in this
        // map, because the conjuring module answers the whole chain, not just its last link.
        let mut conjured: Vec<String> = Vec::new();
        for module in self.knowledge.modules() {
            conjured.extend(module.after_the_bundle(&mut context));
        }
        for name in conjured {
            context.namespaces.declare(name, true);
        }
        context.settle();
        // The split, the instrument the memo is measured by: `projected` is how many visited
        // documents the memo could not answer, `projecting` what they cost (on a keystroke, the
        // `.rbs` files the walk refuses plus the one edited document). The three figures after are
        // the folds no memo reaches: the floor.
        tracing::debug!(
            "walked {visited} documents in {:.2?} ({projecting:.2?} projecting {projected} of \
             them, {:.2?} merging, {includers:.2?} includers, {bundled:.2?} bundle namespaces)",
            started.elapsed(),
            absorbed.saturating_sub(projecting),
        );
        context
    }

    /// What one document contributes to a [`Context`], or `None` when the walk does not visit it.
    ///
    /// The loop body of [`Analysis::walk`], callable for one document so the gate can re-project
    /// it. It reads the graph, never a file: the bound this whole pass inherits.
    ///
    /// **`None` matters.** A declined document still has definitions, and
    /// [`Analysis::bundle_namespaces`] reads *every* definition in the graph, so an `.rbs` in the
    /// project's `sig/`, or a gem file the user opened and typed in, can move a `Context` while
    /// contributing nothing here. The gate refuses a touched, unvisited document for exactly that
    /// reason, which lets the gate and the memo both stop at this function's outputs.
    fn contribution(&self, document: &Document, filters: &Filters) -> Option<Contribution> {
        // Ruby only, for a real reason: an `.rbs` has `def`s and doc comments like any document, so
        // a `@return` tag in one would put it on the annotated list and hand it to a Ruby parser,
        // and the generated RBS would fail to parse (caught by `Synthesized::record`'s gate, but
        // still a bug).
        //
        // A Rails engine's `app/` is read too, the only difference from `is_own_code`. Which of the
        // six lists an engine document may join is `Wants::engines` below: one flag per list, never
        // a second loop.
        let own = self.is_own_code(document.uri());
        // **Three widths.** `own` is the user's code; `generator` adds a Rails engine's `app/`
        // ([`Analysis::is_generator_source`]'s purpose); and everything else left after the `.rbs`
        // test is a gem's own Ruby, which exactly one list admits (see [`Wants::gems`]). Nothing in
        // the graph is outside all three.
        let generator = own || self.is_generator_source(document.uri());
        if document.uri().ends_with(".rbs") {
            return None;
        }
        let mut contribution = Contribution::default();
        let mut tagged = false;
        let rows = self.knowledge.wants().len();
        let mut defines = vec![false; rows];
        let mut declares = vec![false; rows];
        // One per **module**, not per row: `Wants::inherits` asks the module's own convention, so
        // two rows of one module share the answer and two modules never do.
        let mut inherits = vec![false; self.knowledge.len()];
        for definition in document
            .definitions()
            .iter()
            .filter_map(|id| self.graph.definitions().get(id))
        {
            match definition {
                Definition::Class(class) => {
                    let Some(name) = self.qualified_name(class.name_id()) else {
                        continue;
                    };
                    // The two entry-point conventions, asked once for both callers (the module's
                    // own `claims_by_ancestry`), so which documents are worth opening and which
                    // classes are worth reading cannot disagree. `include` only:
                    // `extend Sidekiq::Worker` puts the hook nowhere.
                    let superclass = class
                        .superclass_ref()
                        .and_then(|id| self.graph.constant_references().get(id))
                        .and_then(|reference| self.spelled_name(reference.name_id()));
                    let mixins: Vec<String> = class
                        .mixins()
                        .iter()
                        .filter_map(|mixin| match mixin {
                            Mixin::Include(include) => Some(include.constant_reference_id()),
                            Mixin::Prepend(_) | Mixin::Extend(_) => None,
                        })
                        .filter_map(|id| self.graph.constant_references().get(id))
                        .filter_map(|reference| self.spelled_name(reference.name_id()))
                        .collect();
                    // Asked of every registered module, none by name: whether a class with this
                    // superclass and these mixins is *yours* is the one question in the table core
                    // cannot ask.
                    for (found, module) in inherits.iter_mut().zip(self.knowledge.modules()) {
                        *found |= module.claims_by_ancestry(superclass.as_deref(), &mixins);
                    }
                    // The includer edge, read here for `superclasses`' reason: an `include` is
                    // recorded on the definition at index time, so which module it names is a graph
                    // question. Kept as written and resolved after the loop, because the set it
                    // resolves against is complete only when every document has been seen.
                    contribution
                        .included
                        .extend(mixins.iter().map(|written| (name.clone(), written.clone())));
                    if generator && let Some(superclass) = superclass {
                        contribution.superclasses.push((name.clone(), superclass));
                    }
                    if generator {
                        contribution.declared.push((name, false));
                    } else {
                        contribution.foreign.push((name, false));
                    }
                }
                Definition::Module(module) => {
                    // The sixth predicate, asked of the **last segment**, the name rubydex
                    // interned: `module ClassMethods` is one string compare per module definition,
                    // and the walk up its parents (the expensive half) runs only on a match.
                    if let Some(interned) = self.graph.names().get(module.name_id()) {
                        for (found, names) in declares.iter_mut().zip(&filters.modules) {
                            *found |= names.contains(interned.str());
                        }
                    }
                    if let Some(name) = self.qualified_name(module.name_id()) {
                        // A module's own `include`s, for the same edge: a concern including a
                        // concern is what the closure below walks through.
                        contribution.included.extend(
                            module
                                .mixins()
                                .iter()
                                .filter_map(|mixin| match mixin {
                                    Mixin::Include(include) => {
                                        Some(include.constant_reference_id())
                                    }
                                    Mixin::Prepend(_) | Mixin::Extend(_) => None,
                                })
                                .filter_map(|id| self.graph.constant_references().get(id))
                                .filter_map(|reference| self.spelled_name(reference.name_id()))
                                .map(|written| (name.clone(), written)),
                        );
                        if generator {
                            contribution.declared.push((name, true));
                        } else {
                            contribution.foreign.push((name, true));
                        }
                    }
                }
                // A YARD tag is a comment above a `def`, and indexing already put comments in the
                // graph, so which files are worth parsing for tags is answered without opening any.
                // The name is read for `def self.table_name_prefix`, the one thing on these lists a
                // file *defines* rather than calls or references.
                //
                // **Skipped for a gem**, which saves most of what widening the walk would cost:
                // method definitions are the commonest thing in any document, and both questions
                // here (a YARD tag, `def self.table_name_prefix`) are about lists no gem may join.
                Definition::Method(method) if generator => {
                    if !tagged {
                        tagged = method.comments().iter().any(|comment| {
                            comment.string().contains("@return")
                                || comment.string().contains("@param")
                        });
                    }
                    // A string lookup, not a hash, because rubydex records a `def` under its name
                    // *and* parameter list (`table_name_prefix()`), and a `WANTS` row spelling that
                    // out would silently stop matching if the rendering changed. One lookup per
                    // **singleton** method, the narrow half of the definitions.
                    if matches!(method.receiver(), Some(Receiver::SelfReceiver(_)))
                        && let Some(spelled) = self.graph.strings().get(method.str_id())
                    {
                        let name = render::simple_name(spelled.as_str());
                        for (found, want) in defines.iter_mut().zip(self.knowledge.wants()) {
                            *found |= want.defines.contains(&name);
                        }
                    }
                }
                _ => {}
            }
        }

        let calls = |names: &[StringId]| {
            !names.is_empty()
                && document
                    .method_references()
                    .iter()
                    .filter_map(|id| self.graph.method_references().get(id))
                    .any(|reference| names.contains(reference.str()))
        };
        // The last segment of a constant reference, which is what `Struct` is in every spelling:
        // `Struct`, `::Struct`, and (the one that matters inside `module Admin`) the same `Struct`
        // with a nesting rubydex records separately from the name.
        let mentions = |names: &[StringId]| {
            !names.is_empty()
                && document
                    .constant_references()
                    .iter()
                    .filter_map(|id| self.graph.constant_references().get(id))
                    .filter_map(|reference| self.graph.names().get(reference.name_id()))
                    .any(|name| names.contains(name.str()))
        };
        // What the indexer found in the text ([`Wants::spells`]): the one list test rubydex's index
        // has no record for.
        let spelled = self
            .spelled
            .get(&UriId::from(document.uri()))
            .copied()
            .unwrap_or(0);
        let features = self.workspace.features();
        for (((((at, want), names), constants), (defined, declared)), spells) in self
            .knowledge
            .wants()
            .iter()
            .enumerate()
            .zip(&filters.calls)
            .zip(&filters.constants)
            .zip(defines.into_iter().zip(declares))
            .zip(&filters.spells)
        {
            let module = self.knowledge.behind(at);
            // **The one place a switched-off generator is switched off.** The rows decide which
            // documents any generator ever sees, so an empty list is a generator that does nothing,
            // with no removal path to write. A config reload re-indexes the workspace anyway
            // (`analysis/mod.rs`), so declarations produced before a generator was turned off are
            // dropped by that rebuild.
            if !module.wanted(want.list, features) {
                continue;
            }
            if !own && !(if generator { want.engines } else { want.gems }) {
                continue;
            }
            // Once per document per list, however many of the three tests say yes, which is why
            // they are one loop: a file with a `sig` *and* a `@return` tag is one entry on the
            // annotated list; two would generate it twice.
            if calls(names)
                || spelled & spells != 0
                || mentions(constants)
                || want
                    .path
                    .is_some_and(|suffix| document.uri().ends_with(suffix))
                || (want.tags && tagged)
                || (want.inherits && inherits[self.knowledge.module_at(at)])
                || defined
                || declared
            {
                contribution.lists.push(want.list);
            }
        }
        // **Every module's own half, as a fold over what is already in hand**: the names this
        // document declares, the superclass and `include`s beside each, and the URI. Nothing here
        // returns to the graph, so a module's projection never costs a second walk. The one thing
        // that cannot be folded is a byte offset, which the `Seen::confirming` closure answers.
        let seen = Seen {
            uri: document.uri(),
            own,
            generator,
            declared: &contribution.declared,
            superclasses: &contribution.superclasses,
            included: &contribution.included,
            confirming: &|parent, names| self.confirming_path(document, parent, names),
        };
        contribution.modules = self
            .knowledge
            .modules()
            .map(|module| knowledge::Contributed(module.contribute(&seen)))
            .collect();
        Some(contribution)
    }

    /// What the **whole graph** (the bundle, Ruby's signatures, everything indexed) says each of
    /// `wanted` is: a class or a module.
    ///
    /// - **Reads the definitions, not the declarations.** `Graph::get` is the map
    ///   `Resolver::resolve` builds, and this runs just before the resolve, so it would answer
    ///   about the *previous* settle: every gem class would be invisible for the first two settles
    ///   and then appear, giving a different answer on each early pass over an unchanged workspace.
    /// - **Never reads a document this crate generated**, filtered by scheme (what a non-`file:`
    ///   URI is for). Otherwise the pass would read its own previous output and the second settle
    ///   could not equal the third.
    fn bundle_namespaces(&self, wanted: &BTreeSet<String>) -> Vec<(String, bool)> {
        // Hashes of the last segments, so a definition nobody asked about costs one compare, never
        // a walk up its parents.
        let last: HashSet<StringId> = wanted
            .iter()
            .map(|name| StringId::from(name.rsplit("::").next().unwrap_or(name)))
            .collect();
        let mut found: BTreeMap<String, bool> = BTreeMap::new();
        for definition in self.graph.definitions().values() {
            let (name_id, module) = match definition {
                Definition::Class(class) => (class.name_id(), false),
                Definition::Module(module) => (module.name_id(), true),
                _ => continue,
            };
            let Some(name) = self.graph.names().get(name_id) else {
                continue;
            };
            if !last.contains(name.str()) {
                continue;
            }
            if self
                .graph
                .documents()
                .get(definition.uri_id())
                .is_none_or(|document| document.uri().starts_with(synthesized::GENERATED_SCHEME))
            {
                continue;
            }
            let Some(spelled) = self.qualified_name(name_id) else {
                continue;
            };
            if !wanted.contains(&spelled) {
                continue;
            }
            // A name spelled differently by two files is a `module` only if all of them say so:
            // opening a body for a name someone else declares as a class is a real mistake, and
            // `false` writes nothing.
            found
                .entry(spelled)
                .and_modify(|already| *already &= module)
                .or_insert(module);
        }
        found.into_iter().collect()
    }

    /// Where each of `conjured`'s names is written, on the line that confirmed the directory.
    ///
    /// - **The source this generator read.** The generated `module Api` has no member to hang a
    ///   place on, so its place is the `Api` in the `class Api::V1::Foo` this document writes, the
    ///   same rule as every other span in this pass, reached through a body instead of a member.
    /// - **Read from the declaration the chain was confirmed on, never the whole document.** A file
    ///   naming `Api` inside a method names a *use*, not a declaration. So the window is that
    ///   declaration's construct, and within it the **earliest** reference to a name is the one the
    ///   path wrote (a superclass and body come after it). `declared` is the parent the caller
    ///   already matched, passed in so this has no arm for a chain it cannot be called with.
    /// - **Matched by name, not position.** A nested spelling (`module Api` around `class V1::Foo`)
    ///   writes fewer references than the path has segments, and pairing by position would put
    ///   `Api`'s place on `V1`'s line. A name with no reference in the window is absent here, and
    ///   the rails module's `autoloaded_declarations` writes its body with no span (no mapping, no
    ///   place). That costs nothing: a document spelling the nesting out declares every name in the
    ///   chain, so its `autoloaded` filter drops them anyway.
    /// - **`full` is the namespace the declaration opens** (`Api::V1::Accounts` of
    ///   `class Api::V1::Accounts::CredentialsController`), and `selection` the one segment. The
    ///   class's own name is outside it on purpose: the reader is sent to where the namespace is
    ///   written.
    fn confirming_path(
        &self,
        document: &Document,
        declared: &str,
        names: &BTreeSet<&str>,
    ) -> HashMap<String, At> {
        // The first declaration `declared` names, in document order: a second in the same file
        // confirms the same namespace again, and the earlier line is where a reader would expect to
        // land.
        let within = document
            .definitions()
            .iter()
            .filter_map(|id| self.graph.definitions().get(id))
            .find_map(|definition| {
                let name = self.qualified_name(definition.name_id()?)?;
                let (parent, _) = name.rsplit_once("::")?;
                (parent == declared)
                    .then(|| (definition.offset().start(), definition.offset().end()))
            });
        within.map_or_else(HashMap::new, |(from, to)| {
            let mut segments: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
            for reference in document
                .constant_references()
                .iter()
                .filter_map(|id| self.graph.constant_references().get(id))
                .filter(|reference| {
                    reference.offset().start() >= from && reference.offset().end() <= to
                })
            {
                let Some(spelled) = self
                    .qualified_name(reference.name_id())
                    .and_then(|name| names.get(name.as_str()).copied())
                else {
                    continue;
                };
                let at = (reference.offset().start(), reference.offset().end());
                // The path's own segment is the **earliest** occurrence in the construct, which is
                // not the first handed over: rubydex indexes `class Mod::A < Mod::B`'s superclass
                // before the name. `Ord::min` rather than a comparison, because a file cannot write
                // a superclass before its class's name, and an arm for that would be unreachable.
                segments
                    .entry(spelled)
                    .and_modify(|held| *held = (*held).min(at))
                    .or_insert(at);
            }
            // One `full` serves every segment, because they all belong to one path: it opens at the
            // outermost and closes at the innermost this file wrote.
            let opens = segments.values().map(|at| at.0).min();
            let closes = segments.values().map(|at| at.1).max();
            opens
                .zip(closes)
                .map(|full| {
                    segments
                        .into_iter()
                        .map(|(name, selection)| (name.to_owned(), (full, selection)))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// A class or module's name with its nesting, spelled the way Ruby writes it.
    ///
    /// Three spellings reach the same place and rubydex records them differently: `class Story`
    /// has no parent and no nesting, `class ::Tag` says top level outright, and both
    /// `module Admin; class Story` and `class Admin::Post` are nested (the first in the lexical
    /// nesting, the second in the name's own parent). Walking both links gives the same string a
    /// `class_name: "Admin::Setting"` must match.
    ///
    /// `None` for a singleton's attached name, which is not a constant anybody writes.
    fn qualified_name(&self, name_id: &NameId) -> Option<String> {
        let name = self.graph.names().get(name_id)?;
        let own = self.graph.strings().get(name.str())?.as_str();
        let parent = match name.parent_scope() {
            ParentScope::Some(parent) => Some(parent),
            ParentScope::Attached(_) => return None,
            ParentScope::TopLevel => None,
            ParentScope::None => name.nesting().as_ref(),
        };
        match parent {
            Some(parent) => Some(format!("{}::{own}", self.qualified_name(parent)?)),
            None => Some(own.to_owned()),
        }
    }

    /// A constant as it is *written*, without the lexical nesting around it.
    ///
    /// The difference from [`Self::qualified_name`] is why both exist. That one answers "which
    /// class is this", which needs the nesting; this one answers "what does this line say", which
    /// must not have it. `class Story < ApplicationRecord` inside `module Admin` names
    /// `ApplicationRecord`, not `Admin::ApplicationRecord`, and Ruby resolves it to the top-level
    /// one. A reference is not a definition and cannot borrow a definition's nesting.
    fn spelled_name(&self, name_id: &NameId) -> Option<String> {
        let name = self.graph.names().get(name_id)?;
        let own = self.graph.strings().get(name.str())?.as_str();
        match name.parent_scope() {
            ParentScope::Some(parent) => Some(format!("{}::{own}", self.spelled_name(parent)?)),
            _ => Some(own.to_owned()),
        }
    }

    /// Which document declares each of these namespaces, for the generators that must open a file
    /// they were not handed, or hang their facts on one: a concern's module, and the class or module
    /// a framework row is on.
    ///
    /// - **Answered from [`Analysis::declarers`]**, the pass's one walk, by last segment first: a
    ///   definition of another name is never looked at.
    /// - **A name several files reopen answers with the first in URI order**: arbitrary but
    ///   deterministic. What this reads is what a module declares in its own body, and a module
    ///   spread over two files would need both read whichever were picked.
    /// - **A generated document is skipped**, for `bundle_namespaces`' reason: this pass wrote
    ///   those, and reading its own output back is how a fact starts deriving from itself.
    fn declaring_documents(
        &self,
        declarers: &Declarers,
        wanted: &BTreeSet<String>,
    ) -> BTreeMap<String, DocUri> {
        let last: HashSet<StringId> = wanted
            .iter()
            .map(|name| StringId::from(name.rsplit("::").next().unwrap_or(name)))
            .collect();
        let mut found: BTreeMap<String, (String, DocUri)> = BTreeMap::new();
        for (name_id, uri_id, _) in last.iter().filter_map(|last| declarers.get(last)).flatten() {
            let Some(document) = self.graph.documents().get(uri_id) else {
                continue;
            };
            if document.uri().starts_with(synthesized::GENERATED_SCHEME) {
                continue;
            }
            let Some(spelled) = self.qualified_name(name_id) else {
                continue;
            };
            if !wanted.contains(&spelled) {
                continue;
            }
            let Some(uri) = DocUri::from_graph_uri(document.uri()) else {
                continue;
            };
            match found.entry(spelled) {
                std::collections::btree_map::Entry::Occupied(mut held) => {
                    if document.uri() < held.get().0.as_str() {
                        held.insert((document.uri().to_owned(), uri));
                    }
                }
                std::collections::btree_map::Entry::Vacant(empty) => {
                    empty.insert((document.uri().to_owned(), uri));
                }
            }
        }
        found
            .into_iter()
            .map(|(name, (_, uri))| (name, uri))
            .collect()
    }

    /// Which of these names some file declares, and whether as a `module` ([`knowledge::Kinds`]).
    ///
    /// [`Self::declaring_documents`]' walk and its skip of this pass's own documents, keeping the
    /// kind instead of the file. A name is a `module` only where every file says so, as in
    /// `bundle_namespaces`: opening a `module` for a name another file declares a class is the
    /// mistake, and a `class` line opens nothing new.
    fn declared_kinds(
        &self,
        declarers: &Declarers,
        wanted: &BTreeSet<String>,
    ) -> BTreeMap<String, bool> {
        let last: HashSet<StringId> = wanted
            .iter()
            .map(|name| StringId::from(name.rsplit("::").next().unwrap_or(name)))
            .collect();
        let mut found: BTreeMap<String, bool> = BTreeMap::new();
        for (name_id, uri_id, module) in
            last.iter().filter_map(|last| declarers.get(last)).flatten()
        {
            if self
                .graph
                .documents()
                .get(uri_id)
                .is_none_or(|document| document.uri().starts_with(synthesized::GENERATED_SCHEME))
            {
                continue;
            }
            let Some(spelled) = self.qualified_name(name_id) else {
                continue;
            };
            if wanted.contains(&spelled) {
                found
                    .entry(spelled)
                    .and_modify(|already| *already &= *module)
                    .or_insert(*module);
            }
        }
        found
    }

    /// Every class and module definition in the graph, by its name's last segment: what
    /// [`Analysis::declaring_documents`] answers from.
    ///
    /// **One walk a pass, however many modules ask.** Each call walked every definition in the
    /// graph, and four modules call it on every settle: a fifth of a large app's settle, for a
    /// dozen names.
    fn declarers(&self) -> Declarers {
        let mut declarers = Declarers::new();
        for definition in self.graph.definitions().values() {
            let (name_id, module) = match definition {
                Definition::Module(module) => (module.name_id(), true),
                Definition::Class(class) => (class.name_id(), false),
                _ => continue,
            };
            if let Some(name) = self.graph.names().get(name_id) {
                declarers.entry(*name.str()).or_default().push((
                    *name_id,
                    *definition.uri_id(),
                    module,
                ));
            }
        }
        declarers
    }

    /// What a template's implicit receiver can answer, asked of every module.
    ///
    /// The first answer wins and there is only ever one: this is not a declaration, so there is
    /// no rank to settle it with, and two modules both claiming to know what a bare word in a
    /// template means would be a design question rather than a precedence one.
    fn view_context(&self, context: &Context) -> Views {
        self.declaring(context, |declaring| {
            self.knowledge
                .modules()
                .find_map(|module| module.views(declaring))
                .unwrap_or_default()
        })
    }

    /// Read and parse every file the generators are about to open, and only the ones that moved.
    ///
    /// - **The parse memo.** Without it, one keystroke in a model file re-reads and re-parses
    ///   every file on every list: thousands of files on a large app, to learn what all but one
    ///   said last time. The memo is keyed by document URI and valid exactly as long as
    ///   [`crate::knowledge::Fresh`] says the text has not moved.
    /// - **It checks the disk, not a notification.** The pass gate claims the answer is a
    ///   function of what is on disk and earns it by re-reading everything; a memo stops doing
    ///   that, so a `stat` per wanted file replaces the re-read. A `git checkout` that rewrites
    ///   a model still takes effect at the next settle, with no watcher involved.
    /// - **Two readers are deliberately not memoised**, because neither is a function of its own
    ///   text: `structs::read` takes the `Context`'s namespaces, and a routes reader takes a
    ///   prefix another file's parse computed. Memoising either would need a second input
    ///   compared, which is the design this rejects.
    ///
    /// `buffers` limits it to the modules that read open buffers (`Some(true)`), as
    /// [`declare_all`] does. What comes back is every stamp a memo took, for [`Analysis::remember`].
    fn refresh_sources(&mut self, context: &Context, buffers: Option<bool>) -> Stamped {
        // Taken out and put back, because the closures below borrow the rest of `self` (open
        // buffers, the workspace root) while a module writes to itself. `Analysis::walk` does the
        // same with the contributions.
        let mut knowledge = std::mem::take(&mut self.knowledge);
        let stamped = RefCell::new(Stamped::new());
        let sources = knowledge::Sources {
            context,
            features: self.workspace.features(),
            fresh: &|uri| self.freshness(uri, &stamped),
            held: &|uri| self.open.contains_key(uri),
            text: &|uri| self.with_text(uri, |text| text.text().to_owned()),
            caption: &|uri| self.workspace_relative(uri),
        };
        for module in knowledge.modules_mut().filter(|module| {
            buffers.is_none_or(|buffers| reads_buffers(module.as_ref()) == buffers)
        }) {
            module.refresh(&sources);
        }
        self.knowledge = knowledge;
        stamped.into_inner()
    }

    /// How one document's text is identified for a module's own memo; see [`knowledge::Fresh`] for
    /// why the two authorities cannot share one answer. A file's stamp is also kept in `stamped`.
    fn freshness(&self, uri: &DocUri, stamped: &RefCell<Stamped>) -> knowledge::Fresh {
        match self.open.get(uri) {
            Some(open) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                open.text.text().hash(&mut hasher);
                knowledge::Fresh::Buffer(hasher.finish())
            }
            None => {
                let Some(path) = uri.to_file_path() else {
                    return knowledge::Fresh::Disk(None);
                };
                let stamp = stamp_of(&path);
                stamped
                    .borrow_mut()
                    .insert(uri.as_str().to_owned(), (path, stamp));
                knowledge::Fresh::Disk(stamp)
            }
        }
    }

    /// Drop declarations a source no longer makes.
    ///
    /// Scoped to the sources *this pass* wrote last time, on purpose: the side table is shared, and
    /// pruning "everything I did not just write" would delete anything else that records into it.
    /// That is real: this module's own tests act as a generator and record from a file no rule here
    /// recognises.
    fn forget_stale(&mut self, kept: &HashSet<String>) {
        let stale: Vec<DocUri> = self
            .generated
            .iter()
            .filter(|source| !kept.contains(*source))
            .filter_map(|source| DocUri::from_graph_uri(source))
            .collect();
        for source in stale {
            self.synthesized.forget(self.graph.graph_mut(), &source);
        }
        self.generated = kept.clone();
    }

    /// How a workspace file should be spelled to a person: `db/animals_schema.rb`.
    ///
    /// - **A gem's file is captioned from the gem directory down.** Once an engine's
    ///   `config/routes.rb` can declare a helper, a root-relative caption would read "From
    ///   `routes.rb`", the same as the project's own routes file. `shouty-1.2.3/config/routes.rb`
    ///   says which.
    /// - **The gem root is the directory whose parent is named `gems`**, the one shape every layout
    ///   in `gems::gem_roots` shares. Otherwise fall back to the file name: a caption is not the
    ///   place to fail.
    pub(super) fn workspace_relative(&self, uri: &DocUri) -> String {
        let Some(path) = uri.to_file_path() else {
            return uri.as_str().to_owned();
        };
        let fallback = || {
            gem_relative(&path)
                .unwrap_or_else(|| Path::new(path.file_name().unwrap_or(OsStr::new(""))).into())
        };
        path.strip_prefix(self.workspace.root())
            .map_or_else(|_| fallback(), Path::to_path_buf)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::synthesize;
    use crate::analysis::testing::*;
    use crate::knowledge::{
        annotations as annotations_list, rails as rails_lists, structs as structs_list,
    };

    /// The registry the server really builds, for fixtures that merge a `Context` by hand.
    fn registered() -> Registry {
        Registry::new(vec![
            Box::new(rails_lists::Rails::default()),
            Box::new(annotations_list::Annotations::default()),
            Box::new(structs_list::Structs::default()),
        ])
    }

    /// One document's contribution, as [`Analysis::contribution`] would build it.
    ///
    /// The Rails half goes in the module's slot, where the walk puts it: slots are positional and
    /// `Rails` is registered first.
    fn declaring(name: &str, superclass: &str, table: &str) -> Contribution {
        claiming(name, superclass, &[(table, name)])
    }

    /// The same, for a document claiming a table under more than one name.
    fn claiming(name: &str, superclass: &str, claims: &[(&str, &str)]) -> Contribution {
        Contribution {
            superclasses: vec![(name.to_owned(), superclass.to_owned())],
            declared: vec![(name.to_owned(), false)],
            modules: vec![
                knowledge::Contributed(Some(Box::new(rails_lists::Contribution {
                    claims: claims
                        .iter()
                        .map(|(table, class)| ((*table).to_owned(), (*class).to_owned()))
                        .collect(),
                    ..rails_lists::Contribution::default()
                }))),
                knowledge::Contributed(None),
                knowledge::Contributed(None),
            ],
            ..Contribution::default()
        }
    }

    /// What Rails made of a merged `Context`, for a test that built one by hand.
    fn rails_of(context: &Context) -> &rails_lists::Projection {
        rails_lists::projection_of(context)
    }

    fn merged(order: [(&str, Contribution); 2]) -> Context {
        let mut context = Context::new(&registered());
        let mut included = Vec::new();
        for (uri, contribution) in order {
            context.absorb(uri, &contribution, &mut included);
        }
        context.settle();
        context
    }

    /// Core names a module only on the line that registers it, and that line is not in the pass.
    ///
    /// **Enforced, not asserted.** The registry is swapped for an empty one and the workspace
    /// re-indexed: the pass still runs (the walk, both gates, the split, the record) and declares
    /// nothing. A seam that only looked right in the source would pass a reading and fail this.
    #[test]
    fn a_pass_with_no_body_of_knowledge_registered_runs_and_declares_nothing() {
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        assert!(harness.has("Story#title()"), "the fixture declares nothing");
        let passes = harness.analysis.passes;

        // Swap the registry and force the pass, rather than `rebuild`: a rebuild is a config change
        // and restores the registered modules, which is right but the wrong thing to test with.
        harness.analysis.knowledge = knowledge::Registry::empty();
        harness.analysis.mark_dirty();
        harness.settle();

        assert!(harness.analysis.passes > passes, "the pass did not run");
        assert!(
            harness.analysis.synthesized.is_empty(),
            "a build with nothing registered generated something"
        );
        assert!(!harness.has("Story#title()"));
        // The walk still ran: what is empty is what the modules would have filled.
        let context = harness.analysis.context();
        assert!(context.classes.contains("Story"), "the walk stopped too");
        assert!(context.documents.is_empty(), "a list nobody registered");
        assert!(context.projections.is_empty());
    }

    /// RSpec, through the pass: each `describe` is a class whose block runs as it, an `it` and a
    /// `let` run on an instance, a `let` is its block's value, and a nested group inherits and
    /// overrides. A shared group's block is nobody's, so nothing answers `self` there.
    #[test]
    fn an_example_group_is_a_class_and_a_let_is_its_block() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let source = "\
describe Story do
  let(:story) { Story.new }
  let(:title) { \"x\" }
  let(:twice) { \"first\" }
  let(:twice) { 2 }
  it \"works\" do
    t = twice
    a = story
    b = title
    c = described_class
    d = subject
    Class.new do
      k = title
    end
  end

  context \"when nested\" do
    let(:title) { 2 }

    it do
      e = title
      f = story
    end
  end

  shared_examples \"anything\" do
    it { g = self }
  end
end
";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        harness.index_gems();
        // Only a spec file the editor holds declares its groups: nothing else names them.
        assert!(harness.generated_for(&uri).is_none());
        harness.open(&uri, source);
        // `k` has no label: a `Class.new` block's `self` is the class it makes, not the example.
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    t: Integer = twice
    a: Story = story
    b: String = title
    c: Story:class = described_class
    d: Story = subject
      e: Integer = title
      f: Story = story"
        );
        let story = card(&mut harness, &uri, source, "story\n    b");
        assert!(story.contains("-> Story"), "{story}");
        harness.run(Task::DidClose { uri: uri.clone() });
        harness.settle();
        assert!(
            harness.generated_for(&uri).is_none(),
            "closing the file drops what it implied"
        );
    }

    /// Opening or closing a spec file re-runs only the modules that read open files, and writes
    /// what the whole pass writes; nothing else is asked where it is placed. An edit is not a
    /// reopening.
    #[test]
    fn reopening_a_spec_re_runs_only_what_reads_open_files() {
        let (mut harness, _) = bundle_in(Harness::configured("[rspec]\nenabled = true\n"));
        harness.write("lib/rspec.rb", RSPEC_CORE);
        let source = "describe Story do\n  let(:story) { Story.new }\n\n  it do\n    a = story\n  end\nend\n";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        let analysis = &harness.analysis;
        let (walks, reopenings, placings) =
            (analysis.walks, analysis.reopenings, analysis.placings);
        assert!(placings > 0, "the query interface was placed");

        harness.open(&uri, source);
        assert_eq!(harness.analysis.reopenings, reopenings + 1);
        assert_eq!(harness.analysis.walks, walks);
        assert_eq!(
            harness.analysis.placings, placings,
            "only what the reopening wrote is asked about, and it has nothing to place"
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    a: Story = story"
        );
        let opened = harness.generated_for(&uri);
        assert!(opened.is_some());
        // What the whole pass would have written.
        harness.analysis.mark_dirty();
        harness.settle();
        assert_eq!(harness.analysis.reopenings, reopenings + 1);
        assert!(harness.analysis.placings > placings);
        assert_eq!(harness.generated_for(&uri), opened);

        harness.run(Task::DidClose { uri: uri.clone() });
        harness.settle();
        assert_eq!(harness.analysis.reopenings, reopenings + 2);
        assert!(harness.generated_for(&uri).is_none());

        // Opened with text the graph does not hold, then edited: neither is a reopening.
        let edited = source.replace("a = story", "b = story");
        harness.open(&uri, &edited);
        harness.change(&uri, source);
        assert_eq!(harness.analysis.reopenings, reopenings + 2);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    a: Story = story"
        );
    }

    /// A request at a spec the editor has just opened waits for its groups: the graph before that
    /// settle never held them, and would answer `story` with every `def story` by name.
    #[test]
    fn a_spec_just_opened_is_answered_after_its_groups() {
        let (mut harness, _) = bundle_in(Harness::configured("[rspec]\nenabled = true\n"));
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write(
            "app/models/shelf.rb",
            "class Shelf\n  def story\n    1\n  end\nend\n",
        );
        let source = "describe Story do\n  let(:story) { Story.new }\n\n  it do\n    a = story\n  end\nend\n";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        // `didOpen` with nothing settled behind it, as an editor's first request meets it.
        harness.analysis.handle(Task::DidOpen {
            uri: uri.clone(),
            text: source.to_owned(),
            version: Some(1),
        });
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "story\n  end")),
            ["story_spec.rb:1:7"]
        );
    }

    /// A spec file another module also writes about takes the whole pass the first time it is
    /// opened, which holds that module's facts from then on: closing and opening it again are
    /// reopenings, and its `Struct` stays declared through both.
    #[test]
    fn a_spec_another_module_writes_about_takes_the_whole_pass_once() {
        // Opened, closed and opened again: how many were reopenings, what the margin says, and
        // whether the spec's own `Struct` was declared after each.
        let reopenings = |above: &str| {
            let mut harness = signed(
                &[("core/core.rbs", TYPED_RBS)],
                "\n[rspec]\nenabled = true\n",
            );
            harness.write("lib/rspec.rb", RSPEC_CORE);
            harness.write("app/models/story.rb", "class Story\nend\n");
            harness.write("lib/point.rb", "Point = Struct.new(:x)\n");
            let source = format!(
                "{above}describe Story do\n  let(:point) {{ Point.new(1) }}\n\n  it do\n    a = point\n  end\nend\n"
            );
            let uri = harness.write("spec/models/story_spec.rb", &source);
            harness.index();
            let before = harness.analysis.reopenings;
            let mut pairs = Vec::new();
            harness.open(&uri, &source);
            pairs.push(harness.has("Pair#left()"));
            harness.run(Task::DidClose { uri: uri.clone() });
            harness.settle();
            pairs.push(harness.has("Pair#left()"));
            harness.open(&uri, &source);
            pairs.push(harness.has("Pair#left()"));
            (
                harness.analysis.reopenings - before,
                drawn_hints(&source, &harness.hints_in(&uri)),
                pairs,
            )
        };
        // Nothing but RSpec writes about the spec: all three are reopenings.
        assert_eq!(
            reopenings(""),
            (3, "    a: Point = point".to_owned(), vec![false; 3])
        );
        // A `Struct` written in it: the first open is the whole pass.
        assert_eq!(
            reopenings("Pair = Struct.new(:left)\n\n"),
            (2, "    a: Point = point".to_owned(), vec![true; 3])
        );
    }

    /// A support file both kinds write about, a shared group beside a `Struct`, leaves every
    /// reopening a reopening, and each writes what the whole pass writes.
    #[test]
    fn a_support_file_both_kinds_write_about_leaves_reopenings_whole() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let support = harness.write(
            "spec/support/pairs.rb",
            "Pair = Struct.new(:left)\n\nRSpec.shared_context \"with a pair\" do\n  \
             let(:pair) { Pair.new(1) }\nend\n",
        );
        let source = "describe Story do\n  include_context \"with a pair\"\n\n  it do\n    a = pair\n  end\nend\n";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        let whole = harness.generated_for(&support);
        assert!(
            whole
                .as_deref()
                .is_some_and(|rbs| rbs.contains("left") && rbs.contains("pair")),
            "{whole:?}"
        );
        let before = harness.analysis.reopenings;

        harness.open(&uri, source);
        assert_eq!(harness.analysis.reopenings, before + 1);
        assert_eq!(harness.generated_for(&support), whole);
        assert!(harness.has("Pair#left()"));
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    a: Pair = pair"
        );

        harness.run(Task::DidClose { uri: uri.clone() });
        harness.settle();
        assert_eq!(harness.analysis.reopenings, before + 2);
        assert_eq!(harness.generated_for(&support), whole);
        assert!(harness.has("Pair#left()"));
    }

    /// A reopening is the whole pass's answer only while nothing else moved: a file a generator
    /// read that changed on disk with nobody told takes the whole pass, and the whole pass after
    /// a reopening still drops what another module stopped declaring.
    #[test]
    fn a_reopening_leaves_the_rest_of_the_pass_whole() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let point = harness.write("lib/point.rb", "Point = Struct.new(:x)\n");
        let source = "describe Story do\n  it do\n    a = 1\n  end\nend\n";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        let before = harness.analysis.reopenings;
        // Rewritten on disk, and nobody told: the stamp says so.
        harness.write("lib/point.rb", "Point = Struct.new(:x, :y)\n");
        harness.open(&uri, source);
        assert_eq!(harness.analysis.reopenings, before);
        assert!(harness.has("Point#y()"));

        harness.run(Task::DidClose { uri: uri.clone() });
        harness.settle();
        assert_eq!(harness.analysis.reopenings, before + 1);
        harness.write("lib/point.rb", "Point = 1\n");
        harness.watch(&[&point]);
        assert!(
            !harness.has("Point#x()"),
            "the struct's members went with it"
        );
    }

    /// Nothing but a reopening takes the short pass: not an edit that follows the open before the
    /// settle, not another file edited in the same settle, and not a schema dump that appeared on
    /// disk, which no document notices.
    #[test]
    fn a_reopening_is_only_ever_a_reopening() {
        let spec =
            "FactoryBot.define do\n  factory :thing, class: \"A\"\nend\n\ndescribe Story do\nend\n";
        let fixture = || {
            let mut harness = signed(
                &[("core/core.rbs", TYPED_RBS)],
                "\n[rspec]\nenabled = true\n",
            );
            harness.write("lib/rspec.rb", RSPEC_CORE);
            harness.write(
                "lib/factory_bot.rb",
                "module FactoryBot\n  module Syntax\n    module Methods\n    end\n  end\nend\n",
            );
            for model in ["Story", "A", "B"] {
                harness.write(
                    &format!("app/models/{}.rb", model.to_lowercase()),
                    &format!("class {model}\nend\n"),
                );
            }
            let point = harness.write("lib/point.rb", "Point = Struct.new(:x)\n");
            let uri = harness.write("spec/models/story_spec.rb", spec);
            harness.index();
            (harness, uri, point)
        };
        let open = |harness: &mut Harness, uri: &DocUri, text: &str| {
            harness.analysis.handle(Task::DidOpen {
                uri: uri.clone(),
                text: text.to_owned(),
                version: Some(1),
            });
        };

        // Opened, then edited before anything settled.
        let (mut harness, uri, _) = fixture();
        let before = harness.analysis.reopenings;
        open(&mut harness, &uri, spec);
        harness.analysis.handle(Task::DidChange {
            uri: uri.clone(),
            changes: vec![TextChange {
                range: None,
                text: spec.replace("\"A\"", "\"B\""),
            }],
            version: Some(2),
        });
        harness.analysis.settle();
        assert_eq!(harness.analysis.reopenings, before);
        let rows = harness.generated_rbs("lib/factory_bot.rb");
        assert!(
            rows.contains(
                "(:thing factory, *untyped args, **untyped kwargs) ?{ (untyped) -> untyped } -> ::B"
            ),
            "{rows}"
        );

        // Opened while another file's buffer moved.
        let (mut harness, uri, point) = fixture();
        open(&mut harness, &uri, spec);
        open(&mut harness, &point, "Point = Struct.new(:x, :y)\n");
        harness.analysis.settle();
        assert_eq!(harness.analysis.reopenings, before);
        assert!(harness.has("Point#y()"));

        // Opened after a dump appeared behind the server's back.
        let (mut harness, uri, _) = fixture();
        std::fs::create_dir_all(harness.root.path().join("db")).unwrap();
        std::fs::write(
            harness.root.path().join("db/structure.sql"),
            "CREATE TABLE stories (\n  title character varying\n);\n",
        )
        .unwrap();
        harness.open(&uri, spec);
        assert_eq!(harness.analysis.reopenings, before);
        assert!(harness.has("Story#title()"));
    }

    /// `expect`, `allow`, `receive` and a customization of it, which the gems define at run time:
    /// each is what the gem's own body makes.
    #[test]
    fn expect_allow_and_receive_make_what_the_gems_make() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write("lib/rspec/syntax.rb", RSPEC_SYNTAX);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let source = "\
describe Story do
  it do
    a = expect(1)
    b = expect { 2 }
    c = allow(Story)
    d = receive(:find).and_return(1).once
    e = allow_any_instance_of(Story)
    f = allow(Story).to(d)
  end
end
";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        harness.open(&uri, source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    a: RSpec::Expectations::ValueExpectationTarget = expect(1)
    b: RSpec::Expectations::BlockExpectationTarget = expect { 2 }
    c: RSpec::Mocks::AllowanceTarget = allow(Story)
    d: RSpec::Mocks::Matchers::Receive = receive(:find).and_return(1).once
    e: RSpec::Mocks::AnyInstanceAllowanceTarget = allow_any_instance_of(Story)"
        );
        // `to` is a member, answering nothing: what it returns is the matcher's business.
        let to = card(&mut harness, &uri, source, "to(d)");
        assert!(to.contains("RSpec::Mocks::AllowanceTarget#to"), "{to}");
        // Each parameter named as the gem's `def` names it, never `arg0`.
        for (needle, signature) in [
            (
                "expect(1)",
                "#expect(value) -> RSpec::Expectations::ValueExpectationTarget",
            ),
            (
                "allow(Story)",
                "#allow(target) -> RSpec::Mocks::AllowanceTarget",
            ),
            (
                "receive(",
                "#receive(method_name, &block) -> RSpec::Mocks::Matchers::Receive",
            ),
            (
                "and_return",
                "#and_return(*args, &block) -> RSpec::Mocks::Matchers::Receive",
            ),
            ("allow_any_instance_of", "#allow_any_instance_of(klass)"),
            ("to(d)", "#to(matcher, &block)"),
        ] {
            let card = card(&mut harness, &uri, source, needle);
            assert!(card.contains(signature), "{needle}: {card}");
        }
    }

    /// The gems' own `def`s of the DSL and the syntax, as rspec writes them: in modules the group
    /// extends, and inside a `module_exec` or `class_exec` in a `Syntax` module.
    const RSPEC_DEFS: &str = "\
module RSpec
  module Core
    module Hooks
      def before(*args, &block)
      end
    end

    module MemoizedHelpers
      module ClassMethods
        def let(name, &block)
        end
      end
    end

    class ExampleGroup
      extend Hooks
      extend MemoizedHelpers::ClassMethods
    end
  end

  module Expectations
    module Syntax
      module_function

      def enable_expect(syntax_host = ::RSpec::Matchers)
        syntax_host.module_exec do
          def expect(value = nil, &block)
          end
        end
      end
    end
  end

  module Mocks
    module Syntax
      def self.enable_expect(syntax_host = ::RSpec::Mocks::ExampleMethods)
        syntax_host.class_exec do
          def allow(target)
          end

          def receive(method_name, &block)
          end
        end
      end
    end

    class MessageExpectation
      def and_return(first_value, *values)
      end
    end
  end
end
";

    /// A jump on a member RSpec defines at run time goes to the `def` the gem writes it in, however
    /// rubydex files that, and one a gem only `define_method`s goes nowhere. An edited spec asking
    /// about that one does not settle to learn it again.
    #[test]
    fn rspec_s_run_time_words_jump_to_the_gems_own_defs() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write("lib/rspec/syntax.rb", RSPEC_SYNTAX);
        harness.write("lib/rspec/defs.rb", RSPEC_DEFS);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let source = "\
describe Story do
  let(:story) { Story.new }
  before { }

  it do
    expect(story)
    allow(Story).to receive(:find).and_return(story)
    frobnicate
  end
end
";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        harness.open(&uri, source);
        let jump = |harness: &mut Harness, needle: &str| {
            linked(&harness.definition_at(&uri, source, needle))
        };
        assert_eq!(jump(&mut harness, "let("), ["defs.rb:9:12"]);
        // The card names the DSL's parameters as rspec-core's `def`s do.
        for (needle, signature) in [
            ("let(", "ExampleGroup.let(name, &block)"),
            ("before", "ExampleGroup.before(*args, &block)"),
        ] {
            let card = card(&mut harness, &uri, source, needle);
            assert!(card.contains(signature), "{needle}: {card}");
        }
        assert_eq!(jump(&mut harness, "before"), ["defs.rb:3:10"]);
        assert_eq!(jump(&mut harness, "expect("), ["defs.rb:26:14"]);
        assert_eq!(jump(&mut harness, "allow("), ["defs.rb:37:14"]);
        assert_eq!(jump(&mut harness, "receive("), ["defs.rb:40:14"]);
        assert_eq!(jump(&mut harness, "and_return"), ["defs.rb:47:10"]);
        assert!(jump(&mut harness, "describe").is_empty());

        // Typed into and not yet indexed: `describe` is still found with nowhere to go, so no
        // settle. A call nothing declares still settles and asks again, whatever was asked before.
        let edited = format!("{source}# typed\n");
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: None,
                text: edited.clone(),
            }],
        );
        let ask = |harness: &mut Harness, method: &str, needle: &str| {
            harness.ask(
                method,
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(&edited, needle),
                }),
            )
        };
        assert!(ask(&mut harness, "textDocument/definition", "describe").is_null());
        assert!(
            harness.analysis.dirty,
            "a member with no place settled to find none again"
        );
        assert!(ask(&mut harness, "textDocument/hover", "frobnicate").is_null());
        assert!(
            !harness.analysis.dirty,
            "a call nothing declares kept no retry"
        );

        // The same for a jump: nothing found may be found once the edit is indexed.
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: None,
                text: format!("{edited}# again\n"),
            }],
        );
        assert!(ask(&mut harness, "textDocument/definition", "frobnicate").is_null());
        assert!(
            !harness.analysis.dirty,
            "a jump that found nothing kept no retry"
        );
    }

    /// test-prof's `let_it_be` is a `let` whose block runs on an instance of the group, until the
    /// project registers a modifier of its own.
    #[test]
    fn a_let_it_be_is_its_block_until_the_project_modifies_it() {
        let typed = |support: Option<&str>| {
            let mut harness = signed(
                &[("core/core.rbs", TYPED_RBS)],
                "\n[rspec]\nenabled = true\n",
            );
            harness.write("lib/rspec.rb", RSPEC_CORE);
            harness.write(
                "lib/test_prof.rb",
                "module TestProf\n  module LetItBe\n  end\nend\n",
            );
            harness.write("app/models/story.rb", "class Story\nend\n");
            if let Some(support) = support {
                harness.write("spec/support/test_prof.rb", support);
            }
            let source = "\
describe Story do
  let(:made) { Story.new }
  let_it_be(:story) { made }
  let_it_be_with_reload(:again) { Story.new }

  it do
    a = story
    b = again
  end
end
";
            let uri = harness.write("spec/models/story_spec.rb", source);
            harness.index();
            harness.open(&uri, source);
            drawn_hints(source, &harness.hints_in(&uri))
        };
        // `made` is found from the block: it runs on an instance of the group.
        assert_eq!(typed(None), "    a: Story = story\n    b: Story = again");
        let modified = typed(Some(
            "TestProf::LetItBe.configure do |config|\n  config.register_modifier :touch do |record, _|\n    record\n  end\nend\n",
        ));
        assert!(!modified.contains("Story"), "{modified}");
    }

    /// A `def` in a group's block is a method of that group's class. rubydex files every spec's
    /// `def` of one name as one `Object` method, whose margin was the union of all of them.
    #[test]
    fn a_def_in_a_group_is_its_group_s_own_method() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let one = "\
describe Story do
  let(:title) { 1 }

  def helper
    \"x\"
  end

  def title
    \"shadowed\"
  end

  def self.made
    Story.new
  end

  built = made

  shared_context \"counted\" do
    let(:count) { \"let\" }

    def count
      2
    end
  end

  include_context \"counted\"

  it do
    a = helper
    b = title
    c = count
  end

  context \"nested\" do
    let(:helper) { 3 }

    it do
      d = helper
    end
  end
end
";
        let two = "describe Story do\n  def helper\n    1\n  end\nend\n";
        let uri = harness.write("spec/models/one_spec.rb", one);
        harness.write("spec/models/two_spec.rb", two);
        harness.index();
        harness.open(&uri, one);
        assert_eq!(
            drawn_hints(one, &harness.hints_in(&uri)),
            "  def helper -> String
  def title -> String
  def self.made -> Story
  built: Story = made
    def count -> Integer
    a: String = helper
    b: String = title
    c: Integer = count
      d: Integer = helper"
        );
        let card = card(&mut harness, &uri, one, "helper\n    \"x\"");
        assert!(card.contains("RSpec::ExampleGroups::"), "{card}");
        assert!(card.contains("#helper -> String"), "{card}");
    }

    /// A group a shared group writes is a class of its own under the shared group's module: its
    /// examples see its `let`s and the shared group's, and what only an including group writes
    /// answers nothing.
    #[test]
    fn a_shared_group_s_own_group_takes_its_lets() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let source = "\
shared_examples \"a story\" do
  let(:story) { Story.new }

  context \"when titled\" do
    let(:title) { \"x\" }

    it do
      a = story
      b = title
      c = owned
    end

    context \"deeper\" do
      it do
        d = title
      end
    end
  end
end

describe Story do
  let(:owned) { 1 }

  it_behaves_like \"a story\"
end
";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        harness.open(&uri, source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "      a: Story = story
      b: String = title
        d: String = title"
        );
    }

    /// Seeds build with the same factories: `FactoryBot.create` reaches the strategies through
    /// `extend Syntax::Default`, which includes `Syntax::Methods`, and the jump goes to the factory
    /// in the suite.
    #[test]
    fn a_seeds_file_builds_with_the_suite_s_factories() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "lib/factory_bot.rb",
            "module FactoryBot\n  module Syntax\n    module Methods\n    end\n\n    \
             module Default\n      include Methods\n    end\n  end\n\n  extend Syntax::Default\nend\n",
        );
        harness.write(
            "app/models/user.rb",
            "class User\n  def self.table = 1\nend\n",
        );
        harness.write(
            "spec/factories/users.rb",
            "FactoryBot.define do\n  factory :user\nend\n",
        );
        let source = "a = FactoryBot.create(:user)\n";
        let uri = harness.write("db/seeds.rb", source);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "a: User = FactoryBot.create(:user)"
        );
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "create(:user)")),
            ["users.rb:1:11"]
        );
    }

    /// A gem's factories are read too, and the project's own win a name both write.
    #[test]
    fn a_gem_s_factories_answer_where_the_project_writes_none() {
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/shouty/factories.rb",
            "FactoryBot.define do\n  factory :gadget\n  factory :user, class: \"Gadget\"\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "lib/factory_bot.rb",
            "module FactoryBot\n  module Syntax\n    module Methods\n    end\n  end\nend\n",
        );
        for model in ["User", "Gadget"] {
            harness.write(
                &format!("app/models/{}.rb", model.to_lowercase()),
                &format!("class {model}\n  def self.table = 1\nend\n"),
            );
        }
        harness.write(
            "spec/factories/users.rb",
            "FactoryBot.define do\n  factory :user\nend\n",
        );
        let source = "\
class Runner
  include FactoryBot::Syntax::Methods

  def go
    a = create(:gadget)
    b = create(:user)
  end
end
";
        let uri = harness.write("app/runner.rb", source);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def go -> User\n    a: Gadget = create(:gadget)\n    b: User = create(:user)"
        );
    }

    /// A relation hands its array methods to its records (`ActiveRecord::Delegation`), so what
    /// they answer is the records' answer.
    #[test]
    fn a_relation_s_array_methods_are_its_records() {
        let (mut harness, _) = bundle_in(signed(&[("core/core.rbs", TYPED_RBS)], ""));
        let source = "a = Story.where(id: 1).join(\",\")\nb = Story.all.reverse\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "a: String = Story.where(id: 1).join(\",\")"
        );
        let card = harness.hover_at(&uri, source, "reverse")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(card.contains("Relation#reverse"), "{card}");
    }

    /// A mailbox's `mail` and `inbound_email`, which ActionMailbox writes with `attr_reader` and
    /// `delegate` in its own class.
    #[test]
    fn a_mailbox_s_mail_is_the_inbound_email_s() {
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/action_mailbox.rb",
            "module ActionMailbox\n  class Base\n  end\n\n  class InboundEmail\n  end\nend\n\n\
             module Mail\n  class Message\n  end\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "\
class InboxMailbox < ActionMailbox::Base
  def process
    a = mail
    b = inbound_email
  end
end
";
        let uri = harness.write("app/mailboxes/inbox_mailbox.rb", source);
        harness.write(
            "config/application.rb",
            "module Shop\n  class Application < Rails::Application\n  end\nend\n",
        );
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def process -> ActionMailbox::InboundEmail\n    a: Mail::Message = mail\n    \
             b: ActionMailbox::InboundEmail = inbound_email"
        );
    }

    /// ActiveSupport's `try(:title)` and `try!(:title)` call `title` publicly on the receiver, and
    /// `nil`'s own `try` answers `nil`, so on a receiver that may be `nil` the answer may be too.
    #[test]
    fn try_answers_what_the_method_it_names_answers() {
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/active_support.rb",
            "module ActiveSupport\n  module Tryable\n    def try(*args, &block)\n      \
             public_send(*args, &block) if respond_to?(args.first)\n    end\n\n    \
             def try!(*args, &block)\n      public_send(*args, &block)\n    end\n  end\nend\n\n\
             class Object\n  include ActiveSupport::Tryable\nend\n\n\
             class NilClass\n  def try(*, &)\n    nil\n  end\n\n  def try!(*, &)\n    nil\n  \
             end\nend\n\nclass String\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "\
class Widget
  def title
    \"x\"
  end

  def maybe
    Widget.new if rand
  end

  private

  def secret
    \"s\"
  end
end

a = Widget.new.try(:title)
b = Widget.new.try!(:title)
c = Widget.new.maybe.try(:title)
d = Widget.new.try(:nothing)
e = Widget.new.try(:secret)
f = Widget.new.try { |w| w }
";
        let uri = harness.write("app/models/widget.rb", source);
        harness.write(
            "config/application.rb",
            "module Shop\n  class Application < Rails::Application\n  end\nend\n",
        );
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def title -> String\n  def maybe -> Widget?\n  def secret -> String\n\
             a: String = Widget.new.try(:title)\nb: String = Widget.new.try!(:title)\n\
             c: String? = Widget.new.maybe.try(:title)"
        );
    }

    /// What Rails' own `def`s hand back where their bodies cannot be read to a type:
    /// one arm per shape of call where the answer depends on it, and nothing where a keyword the
    /// row declines (`raw:`, `async:`) or a bare key says only running Ruby knows.
    #[test]
    fn a_rails_method_answers_what_its_source_fixes_for_that_call() {
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/rails_bits.rb",
            "module ActiveSupport\n  class Duration\n    def since(time = nil); end\n    \
             alias from_now since\n    def ago(time = nil); end\n    def to_i; end\n  end\n\n  \
             class TimeWithZone\n  end\n\n  module Cache\n    class Store\n      \
             def fetch(name, options = nil); end\n      def write(name, value, options = nil); end\n    \
             end\n  end\nend\n\n\
             module ActiveRecord\n  module Persistence\n    def save(**options); end\n    \
             def save!(**options); end\n    def update(attributes); end\n  end\n\n  \
             class Base\n    include Persistence\n  end\n\n  class Result\n  end\n\n  \
             module ConnectionAdapters\n    module DatabaseStatements\n      \
             def select_all(arel, name = nil, binds = [], preparable: nil, async: false); end\n    \
             end\n  end\nend\n\n\
             module ActionController\n  class Parameters\n    def expect(*filters); end\n  end\n\n  \
             class ExpectedParameterMissing\n  end\nend\n\n\
             class Time\nend\n\nclass Date\nend\n\nclass DateTime < Date\nend\n\n\
             class Integer\nend\n\nclass String\nend\n\nclass Array\nend\n\nclass TrueClass\nend\n\n\
             class FalseClass\nend\n\nclass NilClass\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "\
class Story < ActiveRecord::Base
end

class Adapter
  include ActiveRecord::ConnectionAdapters::DatabaseStatements
end

a = ActiveSupport::Duration.new.ago
b = ActiveSupport::Duration.new.from_now
c = ActiveSupport::Duration.new.ago(Date.new)
d = ActiveSupport::Duration.new.ago(Time.new)
e = ActiveSupport::Duration.new.to_i
f = ActiveSupport::Cache::Store.new.fetch(\"k\") { 1 }
g = ActiveSupport::Cache::Store.new.fetch(\"k\", expires_in: 3) { \"x\" }
h = ActiveSupport::Cache::Store.new.fetch(\"k\", raw: true) { 1 }
i = ActiveSupport::Cache::Store.new.fetch(\"k\")
j = ActiveSupport::Cache::Store.new.write(\"k\", 1)
k = Story.new.save
l = Story.new.save!
m = Story.new.update(title: \"x\")
n = ActionController::Parameters.new.expect(story: [:title])
o = ActionController::Parameters.new.expect(:id)
p = Adapter.new.select_all(\"SELECT 1\")
q = Adapter.new.select_all(\"SELECT 1\", async: true)
";
        let uri = harness.write("app/models/story.rb", source);
        harness.write(
            "config/application.rb",
            "module Shop\n  class Application < Rails::Application\n  end\nend\n",
        );
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "a: ActiveSupport::TimeWithZone | Time = ActiveSupport::Duration.new.ago\n\
             b: ActiveSupport::TimeWithZone | Time = ActiveSupport::Duration.new.from_now\n\
             c: Date | ActiveSupport::TimeWithZone | Time = ActiveSupport::Duration.new.ago(Date.new)\n\
             d: Time = ActiveSupport::Duration.new.ago(Time.new)\n\
             e: Integer = ActiveSupport::Duration.new.to_i\n\
             f: Integer = ActiveSupport::Cache::Store.new.fetch(\"k\") { 1 }\n\
             g: String = ActiveSupport::Cache::Store.new.fetch(\"k\", expires_in: 3) { \"x\" }\n\
             j: bool? = ActiveSupport::Cache::Store.new.write(\"k\", 1)\n\
             k: bool? = Story.new.save\nl: true? = Story.new.save!\n\
             m: bool? = Story.new.update(title: \"x\")\n\
             n: ActionController::Parameters = ActionController::Parameters.new.expect(story: [:title])\n\
             p: ActiveRecord::Result = Adapter.new.select_all(\"SELECT 1\")"
        );
    }

    /// A literal key is what the main locale holds under it: a `String`, a `Hash` for a
    /// subtree, a `String` for a plural given `count:`, an html-safe buffer for a view's `_html`
    /// key; a key no file writes, a variable, a `default:` that is not a string, another
    /// `locale:`, `count:` on a subtree that is no plural and a view's `t` with a block answer
    /// nothing. `localize` is a `String`, unless a `default:` may be what it hands back.
    #[test]
    fn a_translation_key_is_what_the_main_locale_holds() {
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/i18n.rb",
            "module I18n\n  module Base\n    def translate(key = nil, **options)\n      \
             config.backend.translate(key, **options)\n    end\n    alias :t :translate\n    \
             def locale; end\n    def localize(object, **options)\n      \
             config.backend.localize(object, **options)\n    end\n    alias :l :localize\n  end\n  \
             extend Base\nend\n\nclass String\nend\n\nclass Hash\nend\n\n\
             class Symbol\nend\n\nclass Array\nend\n\nmodule ActiveSupport\n  \
             class SafeBuffer < String\n  end\nend\n\nmodule ActionView\n  module Helpers\n    \
             module TranslationHelper\n      def translate(key, **options)\n        \
             I18n.translate(key, **options)\n      end\n      alias :t :translate\n      \
             def localize(object, **options)\n        I18n.localize(object, **options)\n      end\n      \
             alias :l :localize\n    end\n  end\nend\n\nclass View\n  include ActionView::Helpers::TranslationHelper\nend\n\n\
             module ActiveModel\n  class Name\n    def human(options = {})\n    end\n  end\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n[i18n]\nenabled = true\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("config/locales")).unwrap();
        std::fs::write(
            dir.path().join("config/locales/en.yml"),
            "en:\n  hello:\n    world: Hello\n    tree:\n      a: x\n  items:\n    one: one item\n    \
             other: \"%{count} items\"\n  days: [Mon, Tue]\n  flag: yes\n  title_html: <b>x</b>\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("config/locales/de.yml"),
            "de:\n  hello:\n    world: Hallo\n  only_de: x\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "\
key = \"hello.world\"
a = I18n.t(\"hello.world\")
b = I18n.translate(:\"hello.tree\")
c = I18n.t(\"items\", count: 2)
d = I18n.t(\"items\")
e = I18n.t(\"days\")
f = I18n.t(\"world\", scope: :hello)
g = I18n.t(\"flag\")
h = I18n.t(\"nowhere\")
i = I18n.t(key)
j = I18n.t(\"hello.world\", default: :other)
k = I18n.t(\"hello.world\", locale: :de)
l = I18n.t(\"only_de\")
m = I18n.locale
n = I18n.l(1, format: :short)
o = I18n.l(nil, default: 0)
q = I18n.t(\"hello.tree\", count: 1)
r = View.new.t(\"hello.world\")
s = View.new.t(\"title_html\")
u = View.new.t(\"hello.world\") { |text| 1 }
v = I18n.t(\"hello.world\") { |text| 1 }
w = ActiveModel::Name.new.human
x = View.new.l(1)
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "a: String = I18n.t(\"hello.world\")\n\
             b: Hash[Symbol, untyped] = I18n.translate(:\"hello.tree\")\n\
             c: String = I18n.t(\"items\", count: 2)\n\
             d: Hash[Symbol, untyped] = I18n.t(\"items\")\n\
             e: Array[String] = I18n.t(\"days\")\n\
             f: String = I18n.t(\"world\", scope: :hello)\n\
             m: Symbol = I18n.locale\n\
             n: String = I18n.l(1, format: :short)\n\
             r: String = View.new.t(\"hello.world\")\n\
             s: ActiveSupport::SafeBuffer = View.new.t(\"title_html\")\n\
             v: String = I18n.t(\"hello.world\") { |text| 1 }\n\
             w: String = ActiveModel::Name.new.human\n\
             x: String = View.new.l(1)"
        );
        // The key jumps to the line that writes it, and its card is what the main locale holds.
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "world\")\nb")),
            ["en.yml:2:4"]
        );
        let keyed = card(&mut harness, &uri, source, "world\")\nb");
        assert!(keyed.contains("hello.world: Hello"), "{keyed}");
        assert!(harness.hover_at(&uri, source, "nowhere").is_null());
        assert!(linked(&harness.definition_at(&uri, source, "only_de")).is_empty());
        // `t` and `l` are `alias`es, whose cards print the parameters of the method each renames,
        // never the generated row's first arm, whose `default:` exists only to type a call.
        for (needle, signature) in [
            ("t(\"items\", count: 2)", "#t(key = nil, **options)"),
            ("t(\"title_html\")", "#t(key, **options)"),
            ("l(1, format", "#l(object, **options)"),
            ("l(1)", "#l(object, **options)"),
        ] {
            let card = card(&mut harness, &uri, source, needle);
            assert!(card.contains(signature), "{needle}: {card}");
        }
        // Completion lists the keys under what the key already says, from the main locale alone.
        let other = harness.write("app/other.rb", "");
        assert_eq!(
            harness.declarations_at(&other, "I18n.t(\"hello.~\")\n"),
            ["tree", "world"]
        );
        assert_eq!(
            harness.declarations_at(&other, "I18n.t(\"h~\")\n"),
            ["days", "flag", "hello", "items", "title_html"]
        );
    }

    /// `include Singleton` gives the class `instance`, its one object; a `Singleton` the project
    /// writes nearer the class is its own module, which gives nothing.
    #[test]
    fn a_singleton_s_instance_is_its_one_object() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "app/lib/tag_manager.rb",
            "module ActivityPub\n  class TagManager\n    include Singleton\n\n    \
             def uri_for(_)\n      \"x\"\n    end\n  end\nend\n",
        );
        harness.write(
            "app/lib/local.rb",
            "module Local\n  module Singleton\n  end\n\n  class Thing\n    include Singleton\n  end\nend\n",
        );
        let source = "\
a = ActivityPub::TagManager.instance
b = ActivityPub::TagManager.instance.uri_for(1)
c = Local::Thing.instance
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "a: ActivityPub::TagManager = ActivityPub::TagManager.instance\n\
             b: String = ActivityPub::TagManager.instance.uri_for(1)"
        );
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "instance\nb")),
            ["tag_manager.rb:2:12"]
        );
    }

    /// A setting the project assigns to `config` is what it was assigned, where it is read.
    #[test]
    fn a_setting_the_project_assigns_is_what_it_was_assigned() {
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/rails.rb",
            "module Rails\n  class Railtie\n    class Configuration\n    end\n  end\n\n  \
             class Engine < Railtie\n    class Configuration < ::Rails::Railtie::Configuration\n    \
             end\n  end\n\n  class Application < Engine\n    \
             class Configuration < ::Rails::Engine::Configuration\n    end\n  end\n\n  \
             def self.configuration\n    application.config\n  end\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "config/application.rb",
            "module Shop\n  class Application < Rails::Application\n    \
             config.dispatcher = Dispatcher.new\n  end\nend\n",
        );
        harness.write("app/lib/dispatcher.rb", "class Dispatcher\nend\n");
        harness.write("app/lib/relay.rb", "class Relay\nend\n");
        harness.write(
            "config/initializers/events.rb",
            "Rails.application.configure do\n  config.dispatcher = Dispatcher.new\nend\n",
        );
        let source = "a = Rails.configuration.dispatcher\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "a: Dispatcher = Rails.configuration.dispatcher"
        );
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "dispatcher")),
            ["application.rb:2:11", "events.rb:1:9"]
        );
        // An engine's `config` is the same store; a `Relay`'s writer of the name is not.
        harness.write(
            "lib/blog/engine.rb",
            "module Blog\n  class Engine < Rails::Engine\n    config.dispatcher = Relay.new\n  \
             end\nend\nRelay.new.dispatcher = 1\n",
        );
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "a: Dispatcher | Relay = Rails.configuration.dispatcher"
        );
        // A receiver only a guess types may be one of them, and so may one nothing types.
        for wiring in [
            "module Rails\n  class Application\n    def wire\n      configuration.dispatcher = Relay.new\n    \
             end\n  end\nend\n",
            "def wire(target)\n  target.dispatcher = Relay.new\nend\n",
        ] {
            harness.write("app/lib/wiring.rb", wiring);
            harness.index();
            assert_eq!(
                drawn_hints(source, &harness.hints_in(&uri)),
                "null",
                "{wiring}"
            );
        }
    }

    /// Where the bundle declares no `Rails::Railtie::Configuration`, a setting declares nothing: the
    /// class would be invented.
    #[test]
    fn a_setting_needs_rails_configuration() {
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/rails.rb",
            "module Rails\n  class Railtie\n  end\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "config/application.rb",
            "Rails.application.configure do\n  config.dispatcher = 1\nend\n",
        );
        harness.index();
        harness.index_gems();
        assert!(harness.has("Rails::Railtie"));
        assert!(!harness.has("Rails::Railtie::Configuration"));
    }

    /// With RSpec off, nothing is written: not the groups, not the DSL's rows on rspec-core.
    #[test]
    fn rspec_off_writes_nothing() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = false\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        // Something for the pass to do, so it asks every module to declare.
        harness.write("lib/point.rb", "Point = Struct.new(:x)\n");
        let source = "RSpec.describe \"Story\" do\n  let(:story) { 1 }\nend\n";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        harness.index_gems();
        harness.open(&uri, source);
        harness.settle();
        assert!(harness.generated_for(&uri).is_none());
        assert!(!harness.has("RSpec::Core::ExampleGroup::<ExampleGroup>#it()"));
    }

    /// A group's constant is looked up from the `module` around it, two files whose paths spell one
    /// module name take it in URI order, and nothing is written where rspec-core is not indexed.
    #[test]
    fn a_spec_file_s_module_and_what_it_describes() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        // A class method, as a model has: a class nothing calls on its class side has no class
        // object in rubydex to type `described_class` as.
        harness.write(
            "app/models/shop/order.rb",
            "module Shop\n  class Order\n    def self.build = new\n  end\nend\n",
        );
        let source = "module Shop\n  RSpec.describe Order do\n    it do\n      x = described_class\n    end\n  end\nend\n";
        let one = harness.write("spec/a_b/c_spec.rb", source);
        let two = harness.write("spec/a/b_c_spec.rb", source);
        harness.index();
        harness.index_gems();
        harness.open(&one, source);
        harness.open(&two, source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&one)),
            "      x: Shop::Order:class = described_class"
        );
        let first = harness
            .generated_for(&one)
            .expect("the first file declared");
        let second = harness
            .generated_for(&two)
            .expect("the second file declared");
        assert!(
            first.contains("SpecABCSpec_2") != second.contains("SpecABCSpec_2"),
            "{first}\n{second}"
        );

        let mut bare = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        let uri = bare.write("spec/a_spec.rb", source);
        bare.index();
        bare.open(&uri, source);
        bare.settle();
        assert!(
            bare.generated_for(&uri).is_none(),
            "no rspec-core, nothing to write under"
        );
    }

    /// What `RSpec.configure` includes reaches the groups it says: every group for a bare
    /// `include`, a directory's type where rspec-rails infers one, a tag's groups for a filter, and
    /// an `extend` onto the class a group's body runs as. A model spec does not take a request
    /// helper.
    #[test]
    fn what_rspec_configure_includes_reaches_the_groups_it_names() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write(
            "spec/support/helpers.rb",
            "module Helpers\n  def helper_value = 1\nend\n\nmodule RequestHelpers\n  \
             def request_value = \"x\"\nend\n\nmodule ClassHelpers\n  def class_value = 1.5\nend\n",
        );
        harness.write(
            "spec/rails_helper.rb",
            "RSpec.configure do |config|\n  config.include Helpers\n  \
             config.include RequestHelpers, type: :request\n  config.extend ClassHelpers, :slow\n  \
             config.infer_spec_type_from_file_location!\n  config.include Missing\n  \
             config.include Helpers, type: some_type\nend\n",
        );
        let request = "\
RSpec.describe \"Stories\" do
  let(:from_a_helper) { helper_value }

  it do
    a = helper_value
    b = request_value
    z = from_a_helper
  end

  describe \"slowly\", :slow do
    c = class_value
  end
end
";
        let model = "\
RSpec.describe \"Story\" do
  it do
    d = helper_value
    e = request_value
  end
end
";
        let requests = harness.write("spec/requests/stories_spec.rb", request);
        let models = harness.write("spec/models/story_spec.rb", model);
        harness.index();
        harness.index_gems();
        harness.open(&requests, request);
        harness.open(&models, model);
        assert_eq!(
            drawn_hints(request, &harness.hints_in(&requests)),
            "    a: Integer = helper_value\n    b: String = request_value\n    z: Integer = from_a_helper\n    c: Float = class_value"
        );
        // `e` has no label: a model spec takes no request helper.
        assert_eq!(
            drawn_hints(model, &harness.hints_in(&models)),
            "    d: Integer = helper_value"
        );
    }

    /// A shared group's `let`s reach the groups that bring it in: `include_context` from a support
    /// file (redefining a `let` written before it, as Ruby does), `it_behaves_like` from this file
    /// with its own block, and `config.include_context` by tag. Inside the shared block, what only
    /// an including group defines is not answered.
    #[test]
    fn a_shared_groups_lets_reach_the_groups_that_bring_it_in() {
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n[rspec]\nenabled = true\n",
        );
        harness.write("lib/rspec.rb", RSPEC_CORE);
        harness.write(
            "spec/support/api.rb",
            "RSpec.shared_context \"with api\" do\n  let(:token) { \"t\" }\n  let(:count) { 1 }\nend\n\n\
             shared_context \"automatic\" do\n  let(:automatic) { 1.5 }\nend\n",
        );
        harness.write(
            "spec/rails_helper.rb",
            "RSpec.configure do |config|\n  config.include_context \"automatic\", :auto\nend\n",
        );
        let source = "\
RSpec.describe \"Story\" do
  shared_examples \"local\" do
    let(:local) { 2 }

    it { h = local }
  end

  let(:count) { \"early\" }
  include_context \"with api\"

  it do
    a = token
    b = count
  end

  it_behaves_like \"local\" do
    it { c = local }
  end

  describe \"tagged\", :auto do
    it { d = automatic }
  end
end
";
        let uri = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        harness.index_gems();
        harness.open(&uri, source);
        // `b` is the shared group's: it redefines the `let` written above it. `h` has no label: a
        // shared block is read as any group's, and only one that includes it has `local`.
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    a: String = token\n    b: Integer = count\n    it { c: Integer = local }\n    \
             it { d: Float = automatic }"
        );
    }

    /// FactoryBot's definitions, a runner calling every strategy, and its URI.
    fn factory_fixture(config: &str) -> (Harness, DocUri, &'static str) {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], config);
        harness.write(
            "lib/factory_bot.rb",
            "module FactoryBot\n  module Syntax\n    module Methods\n    end\n  end\n\n  \
             def self.define\n  end\nend\n",
        );
        for model in ["User", "Location", "Ctor"] {
            harness.write(
                &format!("app/models/{}.rb", model.to_lowercase()),
                &format!("class {model}\n  def self.table = 1\n\n  def label = 1\nend\n"),
            );
        }
        harness.write(
            "spec/factories/users.rb",
            "FactoryBot.define do\n  factory :user do\n    factory :admin do\n    end\n  end\n\n  \
             factory :author, class: \"User\", aliases: [:writer]\n  factory :place, class: :location\n  \
             factory :weird do\n    initialize_with { Other.build }\n  end\n\n  \
             factory :ctor do\n    initialize_with { new(1) }\n  end\nend\n",
        );
        let source = "\
class Runner
  include FactoryBot::Syntax::Methods

  def go
    a = create(:user)
    b = build(:admin, name: \"x\")
    c = create_list(:writer, 3)
    d = create(:weird)
    e = create(:unknown)
    h = attributes_for(:user)
    i = create(:ctor)
    j = build(:place, :with_trait)
  end
end
";
        let uri = harness.write("app/runner.rb", source);
        harness.index();
        harness.index_gems();
        (harness, uri, source)
    }

    /// A factory's name picks the class it builds, through a nested parent, `class:`, an alias and a
    /// Symbol class. A factory whose `initialize_with` builds something else, and a name nothing
    /// defines, say nothing.
    #[test]
    fn a_factory_s_name_is_the_class_it_builds() {
        let (mut harness, uri, source) = factory_fixture("");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def go -> Location
    a: User = create(:user)
    b: User = build(:admin, name: \"x\")
    c: Array[User] = create_list(:writer, 3)
    h: Hash = attributes_for(:user)
    i: Ctor = create(:ctor)
    j: Location = build(:place, :with_trait)"
        );
        // The overload the name picks, said as what the method declares: no body was read.
        let card = harness.hover_at(&uri, source, "create(:user)")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(card.contains("-> User\n```"), "{card}");
        // Its parameters named as a reader calls it, and a list's count after the factory.
        assert!(
            card.contains("#create(factory, *args, **kwargs, &block) -> User"),
            "{card}"
        );
        let card = harness.hover_at(&uri, source, "create_list(:writer")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            card.contains("#create_list(factory, amount, *args, **kwargs, &block) -> Array[User]"),
            "{card}"
        );
        // A generated strategy has no place of its own: the jump goes to the factory its first
        // argument names, an alias's to the factory it is an alias of.
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "create(:user)")),
            ["users.rb:1:11"]
        );
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "create_list(:writer")),
            ["users.rb:6:11"]
        );
        // A name no factory writes goes nowhere.
        assert!(
            harness
                .definition_at(&uri, source, "create(:unknown)")
                .is_null()
        );
        // What it built completes as that class.
        let probe = harness.write("app/probe.rb", "");
        let offered = harness.complete(
            &probe,
            "class Probe\n  include FactoryBot::Syntax::Methods\n\n  def go\n    \
             create(:user).lab~\n  end\nend\n",
        );
        let item = offered["items"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["label"] == "label"))
            .cloned()
            .unwrap_or_else(|| panic!("no `label`: {offered}"));
        let card = harness.ask("completionItem/resolve", item)["documentation"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(card.contains("User#label"), "{card}");
        assert!(!card.contains("Guessed from name alone"), "{card}");
    }

    /// A callback's block is handed what its factory builds, and what every factory inheriting it
    /// builds, and runs on a `SyntaxRunner`; an attribute's block runs on the evaluator, and the
    /// factory's own on its definition's proxy.
    #[test]
    fn a_factory_s_callback_is_handed_what_the_factory_builds() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "lib/factory_bot.rb",
            "module FactoryBot\n  module Syntax\n    module Methods\n    end\n  end\n\n  \
             class DefinitionProxy\n    def association(name) = 1\n  end\n\n  \
             class SyntaxRunner\n    include Syntax::Methods\n  end\n\n  \
             class Evaluator\n    def association(name) = \"x\"\n  end\n\n  \
             def self.define\n  end\nend\n",
        );
        for model in ["User", "Admin", "Post"] {
            harness.write(
                &format!("app/models/{}.rb", model.to_lowercase()),
                &format!("class {model}\n  def self.table = 1\n\n  def label = 1\nend\n"),
            );
        }
        let source = "\
FactoryBot.define do
  factory :user do
    a = association(:user)
    name { b = association(:user) }
    after(:create) do |user, evaluator|
      c = user
      d = create(:post)
      e = evaluator
    end
    trait :named do
      after(:build) { |named| f = named }
    end
    before(:build, :create) { |maybe| g = maybe }
    factory :admin, class: \"Admin\" do
      after(:stub) { |admin| h = admin }
    end
  end

  factory :post do
    after(:all) { |any| i = any }
    sequence(:title) { |n| j = association(:x) }
  end

  factory :odd, class: \"Post\" do
    after(:create) { |one, two, three| k = one }
  end
end
";
        let uri = harness.write("spec/factories/users.rb", source);
        harness.index();
        harness.index_gems();
        // `admin` inherits `user`'s callbacks and traits, so they are handed either. `before(:build)`
        // is handed `nil`. The evaluator, `after(:all)` (a `Hash` for `attributes_for`), a
        // `sequence`'s block and a block taking three are not read.
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    a: Integer = association(:user)
    name { b: String = association(:user) }
    after(:create) do |user: Admin | User, evaluator|
      c: Admin | User = user
      d: Post = create(:post)
      after(:build) { |named: Admin | User| f = named }
      after(:build) { |named| f: Admin | User = named }
    before(:build, :create) { |maybe: Admin? | User| g = maybe }
    before(:build, :create) { |maybe| g: Admin? | User = maybe }
      after(:stub) { |admin: Admin| h = admin }
      after(:stub) { |admin| h: Admin = admin }"
        );
    }

    /// With `[types] factories = false`, no strategy is declared and nothing is typed.
    #[test]
    fn factories_off_type_nothing() {
        let (mut harness, uri, _) = factory_fixture("\n[types]\nfactories = false\n");
        let hints = harness.hints_in(&uri);
        assert!(hints.as_array().is_none_or(Vec::is_empty), "{hints}");
        assert!(!harness.has("FactoryBot::Syntax::Methods#create()"));
    }

    #[test]
    fn two_documents_merge_to_the_same_context_in_either_order() {
        // The property the per-document contributions rest on (for the gate and the memo alike),
        // stated where it is decided rather than through a walk whose order cannot be forced.
        //
        // Both fields are easy to get wrong, in different ways: `superclasses` is last-writer-wins
        // over a `HashMap`'s iteration unless it takes the lowest URI (as `defined_in`, filled two
        // lines away, already does), and `claims` is pushed once per definition.
        //
        // An engine monorepo writes the first shape: `class Spree::Product < Spree::Base` in the library, and
        // again in a spec.
        let model = "file:///p/app/models/double.rb";
        let spec = "file:///p/spec/support/double.rb";
        // The spec claims the same table under a second name, so the two documents disagree about
        // `superclasses` *and* push different strings into one `claims` list.
        let from_the_model = || declaring("Double", "ApplicationRecord", "doubles");
        let from_the_spec = || {
            claiming(
                "Double",
                "Object",
                &[("doubles", "Double"), ("doubles", "Stunt")],
            )
        };
        let forwards = merged([(model, from_the_model()), (spec, from_the_spec())]);
        let backwards = merged([(spec, from_the_spec()), (model, from_the_model())]);
        assert_eq!(forwards, backwards);
        assert_eq!(
            forwards.superclasses.get("Double").map(String::as_str),
            Some("ApplicationRecord"),
            "the spec's reopening displaced the model's own superclass"
        );
        assert_eq!(forwards.defined_in["Double"], model);
        assert_eq!(
            rails_of(&forwards).claims["doubles"],
            vec!["Double".to_owned(), "Double".to_owned(), "Stunt".to_owned()]
        );
    }

    #[test]
    fn one_document_saying_it_twice_keeps_the_line_ruby_would_run() {
        // The `<=` in `Context::absorb`, not `<`. A file writing `class Story` twice disagrees with
        // itself, and Ruby's answer is the last line, which is what the walk produces, since within
        // one document the loop follows the order definitions were recorded.
        let uri = "file:///p/app/models/story.rb";
        let mut context = Context::new(&registered());
        let mut included = Vec::new();
        context.absorb(
            uri,
            &Contribution {
                superclasses: vec![
                    ("Story".to_owned(), "First".to_owned()),
                    ("Story".to_owned(), "Second".to_owned()),
                ],
                ..Contribution::default()
            },
            &mut included,
        );
        assert_eq!(
            context.superclasses.get("Story").map(String::as_str),
            Some("Second")
        );
    }

    #[test]
    fn a_walk_that_holds_every_document_answers_what_a_walk_that_holds_none_does() {
        // **The memo's whole claim, in one sentence.** The two tests below name the field a stale
        // entry would show in; this one compares the *whole* projection, so a field nobody thought
        // of is covered too.
        //
        // Both walks run over one graph with nothing between them, which makes them comparable: the
        // pass runs *before* the resolve and writes generated documents the next walk can see, so
        // walks either side of a settle may legitimately differ.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        harness.write("app/models/comment.rb", "class Comment\nend\n");
        harness.write("app/lib/plain.rb", "class Plain\n  def a\n  end\nend\n");
        harness.index();
        harness.analysis.settle();

        // The realistic mixture: one document marked re-indexed, so the walk re-projects it and
        // takes every other one from the memo.
        harness.analysis.touched.insert(story.as_str().to_owned());
        let mixed = harness.analysis.walk();
        // The same graph with nothing held.
        harness.analysis.contributions.clear();
        let cold = harness.analysis.walk();
        assert_eq!(mixed, cold);
        assert!(
            cold.classes.contains("Story") && cold.classes.contains("Plain"),
            "and both walks really visited the workspace"
        );
    }

    #[test]
    fn a_document_the_graph_re_indexed_is_projected_again_rather_than_remembered() {
        // The memo's invalidation, through both ways a document is re-indexed: `touched`, which
        // names one, and `touched_all`, which names none and drops the map. Nothing else can make a
        // held `Contribution` wrong.
        //
        // `classes` is asserted on because it is a set: a stale entry not only misses the new name,
        // it keeps asserting the old one, so both halves of each assertion matter.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let buffered = harness.write("app/lib/buffered.rb", "class First\nend\n");
        let watched = harness.write("app/lib/watched.rb", "class Third\nend\n");
        harness.index();
        harness.analysis.settle();

        // One named document, which only a keystroke ever produces.
        harness.open(&buffered, "class First\nend\n");
        harness.change(&buffered, "class Second\nend\n");
        harness.analysis.settle();
        {
            let held = harness
                .analysis
                .generated_from
                .as_ref()
                .expect("a pass has run");
            assert!(
                held.classes.contains("Second"),
                "the edit is not in the walk"
            );
            assert!(
                !held.classes.contains("First"),
                "the class the edit replaced is still in the walk"
            );
        }

        // And a route naming none: the file system changed under an unopened file, so `touched_all`
        // is all the pass is told.
        harness.write("app/lib/watched.rb", "class Fourth\nend\n");
        harness.watch(&[&watched]);
        harness.analysis.settle();
        let held = harness
            .analysis
            .generated_from
            .as_ref()
            .expect("a pass has run");
        assert!(
            held.classes.contains("Fourth"),
            "the file the watcher reported is not in the walk"
        );
        assert!(
            !held.classes.contains("Third"),
            "the class it replaced is still in the walk"
        );
    }

    #[test]
    fn an_engine_may_not_claim_a_table_or_host_the_route_helpers() {
        // The two `Context` outputs that mean "the application" rather than "a class the reader can
        // name"; letting an engine into either would make an answer worse.
        //
        // - **Table claims.** A table is claimed by pluralizing a top-level class name, so an
        //   engine defining one could take the application's model's table, or (as here) a table no
        //   user class has, putting the schema's columns on a gem's class.
        // - **Hosts.** An engine's controllers are hosts in Rails but not here:
        //   `rails_lists::ROUTES` is closed to engines, so there are no helpers to give them.
        let (dir, root, env) = project_with_engine(&[
            ("models/widget.rb", "class Widget\nend\n"),
            (
                "controllers/shouty/base_controller.rb",
                "class Shouty::BaseController < ActionController::Base\nend\n",
            ),
            ("models/shouty/message.rb", "class Shouty::Message\nend\n"),
            // A module named the one way that makes a module a host. Rails gives an engine's helper
            // modules the application's route helpers; ya-lsp does not, since `rails_lists::ROUTES`
            // is closed to engines.
            (
                "helpers/shouty/blast_helper.rb",
                "module Shouty::BlastHelper\nend\n",
            ),
        ]);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[8.0].define(version: 1) do\n  \
             create_table \"widgets\", force: :cascade do |t|\n    \
             t.string \"name\"\n  \
             end\n\
             end\n",
        );
        harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :stories\nend\n",
        );
        harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController < ActionController::Base\nend\n",
        );
        harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\nend\n",
        );
        harness.index();
        harness.index_gems();

        let context = harness.analysis.context();
        assert!(
            context.classes.contains("Shouty::Message"),
            "the engine's classes are nameable — that is the widening"
        );
        assert!(
            !rails_of(&context).claims.contains_key("widgets"),
            "and an engine claims no table: {:?}",
            rails_of(&context).claims
        );
        assert!(
            !harness.has("Widget#name()"),
            "so the schema declares nothing on it"
        );
        let hosts = format!("{:?}", rails_of(&context).hosts);
        assert!(
            hosts.contains("ApplicationController") && hosts.contains("StoriesHelper"),
            "the application's own base and helper module are hosts: {hosts}"
        );
        assert!(
            !hosts.contains("Shouty::BaseController") && !hosts.contains("Shouty::BlastHelper"),
            "and neither the engine's controller nor its helper module is: {hosts}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn which_of_the_six_lists_an_engines_document_may_go_on() {
        // The engine rule, asserted through the real path, not by reading the table: a list is open
        // to engines when what it reads declares members on a class the reader can name, and closed
        // when it declares something application-scoped.
        //
        // The routes list is open; what a gem's routes file may *say* is a separate question (the
        // list only decides which documents a generator may open). The two closed lists have
        // consequences: `schema.rb` is the *application's* database (an engine ships migrations,
        // not one), and `self.table_name=` feeds a generator that is itself closed. Both files are
        // under `app/` so the list rule is what gets measured, not the walk.
        let (dir, root, env) = project_with_engine(&[
            (
                "models/shouty/message.rb",
                "class Shouty::Message\n  has_many :horns\nend\n",
            ),
            (
                "models/shouty/tagged.rb",
                "class Shouty::Tagged\n  # @return [String]\n  def tag\n  end\nend\n",
            ),
            (
                "jobs/shouty/blast_job.rb",
                "class Shouty::BlastJob < ActiveJob::Base\n  def perform\n  end\nend\n",
            ),
            (
                "models/shouty/renamed.rb",
                "class Shouty::Renamed\n  self.table_name = \"loud\"\nend\n",
            ),
            (
                "misc/schema.rb",
                "ActiveRecord::Schema[8.0].define(version: 1) do\nend\n",
            ),
            (
                "misc/routes.rb",
                "Rails.application.routes.draw do\n  resources :blobs\nend\n",
            ),
        ]);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty::Message.new\n");
        harness.index();
        harness.index_gems();

        let context = harness.analysis.context();
        let on = |list: ListId, needle: &str| {
            context
                .documents(list)
                .iter()
                .any(|uri| uri.contains("shouty-1.2.3") && uri.ends_with(needle))
        };
        assert!(on(rails_lists::MODELS, "message.rb"), "models open");
        assert!(
            on(annotations_list::ANNOTATED, "tagged.rb"),
            "annotations open"
        );
        assert!(
            on(rails_lists::ENTRYPOINTS, "blast_job.rb"),
            "entry points open"
        );
        assert!(on(rails_lists::ROUTES, "routes.rb"), "routes open");
        assert!(!on(rails_lists::RENAMED, "renamed.rb"), "renames closed");
        assert!(!on(rails_lists::SCHEMAS, "schema.rb"), "schemas closed");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A line inserted above a column moves the jump and leaves the graph alone.
    ///
    /// - **Why both halves.** A provenance comment names a file and macro, never a line, so pushing
    ///   `t.string "title"` down a line re-derives byte-identical RBS with every mapping moved.
    ///   Handing that to rubydex would drop the generated document, invalidate everything it
    ///   touched and relink it all to reach the same graph, at a cost that scales with the
    ///   workspace, not the edited file.
    /// - **The counter** is the only instrument that shows it, as with `Analysis::walks`: a graph
    ///   rebuilt into its own shape answers every question the same way.
    /// - **The mappings cannot be ignored**: what moved really moved, and a jump landing one line
    ///   high is worse than a slow one.
    #[test]
    fn an_edit_that_moves_a_declaration_without_changing_it_leaves_the_graph_alone() {
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = rails_project(source);

        let before = harness.definition_at(&uri, source, "title");
        assert_eq!(
            before[0]["targetRange"]["start"]["line"],
            serde_json::json!(2),
            "{before}"
        );
        let indexed = harness.analysis.synthesized.indexed();
        assert!(indexed > 0, "the schema declared something to begin with");

        // Opening it changes nothing; then one blank first line moves every column down a line
        // without changing a word.
        harness.open(&schema, SCHEMA_RB);
        harness.change(&schema, &format!("\n{SCHEMA_RB}"));

        assert_eq!(
            harness.analysis.synthesized.indexed(),
            indexed,
            "the RBS is byte-identical, so the graph has nothing to learn from it"
        );
        let after = harness.definition_at(&uri, source, "title");
        assert_eq!(
            after[0]["targetRange"]["start"]["line"],
            serde_json::json!(3),
            "and the jump still lands on the line that declares it: {after}"
        );
    }

    /// A second database's schema. Rails names it `db/<database>_schema.rb`.
    const ANIMALS_SCHEMA: &str = "\
ActiveRecord::Schema[8.0].define(version: 2024_01_01_000000) do
  create_table \"dogs\", force: :cascade do |t|
    t.string \"name\", null: false
  end
end
";

    /// Why a generated document is a **body**, not a file.
    #[test]
    fn a_column_that_changed_type_re_indexes_its_own_table_and_no_other() {
        // `Synthesized::record` is charged per re-indexed declaration, so one document per file
        // would make one column's type change cost every column in the project. One per body costs
        // one table. `synthesized.md` has the measurement.
        let (mut harness, schema, _uri) = rails_project("Story.new\n");
        let widget = harness.write("app/models/widget.rb", "class Widget\nend\n");
        harness.watch(&[&widget]);
        assert!(harness.has("Story#title()") && harness.has("Widget#name()"));
        // Two tables, two documents, one file.
        assert!(harness.analysis.synthesized.len() > 1);

        let indexed = harness.analysis.synthesized.indexed();
        let before = harness.every_generated_document();
        harness.write(
            "db/schema.rb",
            &SCHEMA_RB.replace("t.string \"name\"", "t.integer \"name\""),
        );
        harness.watch(&[&schema]);

        assert!(harness.has("Widget#name()"));
        // The discriminating half: `stories` is in the same file and its document is
        // byte-identical, so `record` never hands it over.
        let moved: Vec<_> = harness
            .every_generated_document()
            .into_iter()
            .filter(|(uri, rbs)| before.get(uri) != Some(rbs))
            .map(|(uri, _)| uri)
            .collect();
        assert_eq!(
            moved,
            vec![rubydex::model::ids::UriId::from(
                synthesized::generated_uri(&schema, "class:Widget").as_str()
            )],
            "only the table whose column moved was rewritten"
        );
        assert_eq!(
            harness.analysis.synthesized.indexed() - indexed,
            1,
            "and only it was handed to the graph again"
        );
    }

    /// A `datetime` column is what Rails' railtie makes it, an `ActiveSupport::TimeWithZone`, until a
    /// file of the project's own writes one of the settings that move that default. Then whether it
    /// is converted is a value this pass does not follow, and the column is either class; taking
    /// the setting away gives the one class back.
    #[test]
    fn a_setting_that_moves_rails_time_zone_default_leaves_time_columns_either_class() {
        let (mut harness, schema, _uri) = rails_project("Story.new\n");
        harness.write(
            "db/schema.rb",
            &SCHEMA_RB.replace(
                "t.text \"description\"",
                "t.text \"description\"\n    t.datetime \"published_at\"",
            ),
        );
        harness.watch(&[&schema]);
        let published = |harness: &Harness| {
            harness
                .generated_for(&schema)
                .unwrap_or_default()
                .lines()
                .find(|line| line.contains("def published_at"))
                .map(|line| line.trim().to_owned())
        };
        let zoned = Some("def published_at: () -> ActiveSupport::TimeWithZone?".to_owned());
        let unzoned =
            Some("def published_at: () -> (ActiveSupport::TimeWithZone | Time | nil)".to_owned());
        assert_eq!(published(&harness), zoned);

        for (path, setting) in [
            (
                "config/application.rb",
                "config.active_record.time_zone_aware_attributes = false",
            ),
            (
                "config/initializers/zones.rb",
                "ActiveRecord::Base.time_zone_aware_types = [:datetime]",
            ),
            (
                "app/models/story.rb",
                "self.skip_time_zone_conversion_for_attributes = [:published_at]",
            ),
        ] {
            let body = |said: &str| format!("class Story\n  {said}\nend\n");
            let file = harness.write(path, &body(setting));
            harness.watch(&[&file]);
            assert_eq!(published(&harness), unzoned, "{path}");
            harness.write(path, &body("def other; end"));
            harness.watch(&[&file]);
            assert_eq!(published(&harness), zoned, "{path}");
        }
    }

    #[test]
    fn a_second_database_s_schema_is_read_and_types_its_columns() {
        // Rails has had multiple databases since 6.0, and a new Rails 8 app ships three secondary
        // schemas (solid_queue, solid_cache, solid_cable). one corpus has three of its own. Reading
        // only `db/schema.rb` would silently miss whole models.
        let source = "Dog.new.name\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let animals = harness.write("db/animals_schema.rb", ANIMALS_SCHEMA);
        let dog = harness.write("app/models/dog.rb", "class Dog\nend\n");
        harness.watch(&[&animals, &dog]);

        assert!(
            harness.has("Dog#name()"),
            "the second database was not read"
        );
        assert!(
            harness.has("Story#title()"),
            "and the first one stopped being"
        );
        assert_eq!(harness.analysis.synthesized.len(), 2);

        let definition = harness.definition_at(&uri, source, "name");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(animals.as_str()),
            "{definition}"
        );
        // The provenance line names the file it really came from. Naming `db/schema.rb` above a
        // column that is not in it would be a confidently wrong answer.
        let card = card(&mut harness, &uri, source, "name");
        assert!(card.contains("Dog#name -> String"), "{card}");
        assert!(!card.contains("`db/schema.rb`"), "{card}");
    }

    #[test]
    fn a_table_two_schemas_declare_is_declared_by_neither() {
        // Like two classes claiming one table, and worse: the model would answer with two schemas,
        // and `Types::harvest` would keep whichever column it read last, depending on `HashMap`
        // iteration order.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        assert!(harness.has("Story#title()"));

        let replica = harness.write("db/replica_schema.rb", SCHEMA_RB);
        harness.watch(&[&replica]);

        assert!(
            !harness.has("Story#title()"),
            "an ambiguous table answered anyway"
        );
        assert!(harness.analysis.synthesized.is_empty());
    }

    #[test]
    fn a_schema_that_stops_declaring_anything_takes_its_columns_with_it() {
        // A generator that reads several files is not notified when one stops having anything to
        // say (it is still there, indexed, a schema). So the pass that writes also prunes.
        let (mut harness, _schema, _uri) = rails_project("Dog.new\n");
        let animals = harness.write("db/animals_schema.rb", ANIMALS_SCHEMA);
        let dog = harness.write("app/models/dog.rb", "class Dog\nend\n");
        harness.watch(&[&animals, &dog]);
        assert!(harness.has("Dog#name()"));

        harness.open(&animals, ANIMALS_SCHEMA);
        harness.change(
            &animals,
            "ActiveRecord::Schema[8.0].define(version: 0) do\nend\n",
        );

        assert!(
            !harness.has("Dog#name()"),
            "an emptied schema still answers"
        );
        assert!(
            harness.has("Story#title()"),
            "and it took the other one with it"
        );
        assert_eq!(harness.analysis.synthesized.len(), 1);
    }

    #[test]
    fn the_schema_pass_leaves_another_generator_s_work_where_it_is() {
        // What every later generator inherits. They generate from *model* files through the same
        // side table, so a `db/*schema.rb` pass pruning "everything I did not just write" would
        // delete their work. Which sources belong to which generator is the caller's question,
        // which is why `Synthesized::sources` returns sources instead of deciding.
        let source = "Story.new.author\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let model = DocUri::from_path(&harness.root.path().join("app/models/story.rb"))
            .expect("a file uri");
        let rbs = "class Story\n  def author: () -> String\nend\n";
        harness.synthesize(
            &model,
            rbs,
            vec![synthesized::Mapping {
                generated: span(rbs, "  def author: () -> String\n"),
                declared: Site {
                    uri: model.as_str().to_owned(),
                    full: (0, 11),
                    selection: (6, 11),
                },
            }],
        );

        // Both generators' documents still answering, after a settle that ran the schema pass over
        // a workspace whose only schema is `db/schema.rb`.
        assert_eq!(harness.analysis.synthesized.len(), 2);
        assert!(
            harness.has("Story#author()"),
            "another generator was pruned"
        );
        assert!(harness.has("Story#title()"));
        assert!(
            harness.definition_at(&uri, source, "author")[0]["targetUri"]
                .as_str()
                .is_some_and(|target| target.ends_with("app/models/story.rb")),
        );
    }

    #[test]
    fn a_schema_dump_is_never_a_document_in_the_graph() {
        // The property `make canary`'s file count depends on, and why the plumbing is a watcher,
        // not a template-style blanking hook: on every route this server takes itself (cold walk,
        // watcher, pass), rubydex never sees SQL, so there is no parse error, no diagnostics and no
        // document to count. The *generated* document exists, with no file behind it. (A client
        // whose selector sends a `.sql` on `didOpen` gets it indexed like any buffer; no shipped
        // client does, since the extension's `LANGUAGES` is `ruby` and `erb`.)
        let (harness, dump, _uri) = sql_project("Story.new\n");
        assert!(
            !harness.analysis.indexed(&dump),
            "the dump was indexed as if it were Ruby"
        );
        assert!(harness.latest(&dump).is_none(), "the dump got diagnostics");
        assert_eq!(harness.analysis.synthesized.len(), 1);
    }

    #[test]
    fn a_table_a_dump_and_a_ruby_schema_both_declare_is_declared_by_neither() {
        // The two-schema rule by a second road. An application uses one format or the other, so
        // this is a repository that switched and kept the old file: two sources for one table,
        // where answering at all means answering with whichever was read last.
        let (mut harness, _dump, _uri) = sql_project("Story.new\n");
        assert!(harness.has("Story#title()"));

        let schema = harness.write("db/schema.rb", SCHEMA_RB);
        harness.watch(&[&schema]);

        assert!(
            !harness.has("Story#title()"),
            "an ambiguous table answered anyway"
        );
    }

    #[test]
    fn a_dump_written_deleted_and_rewritten_on_disk_re_settles_each_time() {
        // `refresh`'s third outcome: a `.sql` is neither re-indexed nor forgotten, so its branch
        // invalidates and indexes nothing. All three events go through it, which is why it sits
        // above the `is_file` test and the index gate: a deleted dump is pruned by `forget_stale`
        // on the settle this triggers, and nothing else would trigger one.
        let source = "Story.new.title\n";
        let (mut harness, _dump, _uri) = sql_project(source);
        let secondary = harness.root.path().join("db/animals_structure.sql");
        let dog = harness.write("app/models/dog.rb", "class Dog\nend\n");
        harness.watch(&[&dog]);

        // Rails names every other database's dump `db/<database>_structure.sql`, as it names the
        // Ruby one `db/<database>_schema.rb`.
        let animals = harness.write(
            "db/animals_structure.sql",
            "CREATE TABLE public.dogs (\n    name character varying NOT NULL\n);\n",
        );
        harness.watch(&[&animals]);
        assert!(harness.has("Dog#name()"), "a new dump was not read");
        assert_eq!(harness.analysis.synthesized.len(), 2);

        // Deleted. Nothing re-reads a missing file, so a column left behind here would answer for
        // the life of the process.
        std::fs::remove_file(&secondary).unwrap();
        harness.watch(&[&animals]);
        assert!(!harness.has("Dog#name()"), "a deleted dump still answers");
        assert!(
            harness.has("Story#title()"),
            "and it took the other with it"
        );
        assert_eq!(harness.analysis.synthesized.len(), 1);
    }

    #[test]
    fn a_relation_is_one_class_per_element_type_and_is_not_a_place() {
        // The two bounds on a relation class. Two models declare `has_many :comments` and share
        // **one** `Comment::Relation`, so class count tracks models, not associations. And nothing
        // in it is a jump target: no code declares `Comment::Relation#first`, and pointing at one
        // of the two `has_many`s would pick half of a coin flip.
        let source = "Story.new.comments.first\n";
        let (mut harness, _story, uri) = models_project(source);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("Comment::Relation")
                .map_or(0, |definitions| definitions.len()),
            1,
            "one relation class per element type, whoever asked for it"
        );
        assert!(
            harness.definition_at(&uri, source, "first").is_null(),
            "a class this crate invented must not be a place a user is sent"
        );
    }

    #[test]
    fn a_relation_class_the_project_already_has_is_not_shadowed() {
        // The one way relation classes could make an answer *worse*, not just absent. A project
        // that wrote its own `Comment::Relation` meant something by it, so the pass emits nothing
        // for that element type: the collection loses its type rather than the user losing their
        // class.
        let (mut harness, _story, _uri) = models_project("");
        let own = harness.write(
            "app/models/comment/relation.rb",
            "class Comment\n  class Relation\n    def own_method\n    end\n  end\nend\n",
        );
        harness.watch(&[&own]);

        assert!(harness.has("Comment::Relation#own_method()"));
        assert!(!harness.has("Comment::Relation#first()"));
        assert!(
            !harness.has("Story#comments()"),
            "an association whose relation was declined must decline too"
        );
    }

    /// The bundle answers on the settle it lands, not the one after.
    ///
    /// `Context::framework` must not ask `Graph::get`, which reads the map `Resolver::resolve`
    /// builds: this pass runs just *before* the resolve, so it would answer about the previous
    /// settle, and `has_one_attached` would declare nothing until the next keystroke, invisible to
    /// any measurement that does not settle twice.
    ///
    /// `index_gems` settles exactly once, which makes this a test rather than a coincidence: that
    /// settle is the one a bundle macro must be answered on.
    #[test]
    fn the_bundle_says_which_class_a_macro_names_on_the_settle_it_lands() {
        let (dir, _gem_home, env) = project_with_gem(
            "module ActiveStorage\n  module Attached\n    class One\n      def attach\n      \
             end\n    end\n  end\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "Story.new.avatar.attach\n";
        let uri = harness.write("app/main.rb", source);
        harness.write(
            "app/models/story.rb",
            "class Story\n  has_one_attached :avatar\nend\n",
        );
        harness.index();
        assert!(
            !harness.has("Story#avatar()"),
            "before the bundle is in, the class the macro names is genuinely not there"
        );

        harness.index_gems();

        assert!(
            harness.has("Story#avatar()"),
            "and the settle the bundle lands on is the one that declares it"
        );
        let chained = card(&mut harness, &uri, source, "attach");
        assert!(
            chained.contains("ActiveStorage::Attached::One#attach"),
            "the chain runs on through the gem's own class: {chained}"
        );
    }

    /// Two settles over an untouched workspace write the same bytes.
    ///
    /// **The pass must never read a document it generated.** The graph holds the *previous*
    /// settle's generated documents when read, so a lookup that saw one would answer differently
    /// each pass with nothing in the output saying so. The filter is by URI **scheme**, not a list
    /// (what a non-`file:` scheme is for); this asserts the property, not the filter.
    #[test]
    fn two_settles_over_an_unchanged_workspace_generate_the_same_bytes() {
        let (mut harness, _schema, _uri) = rails_project("Story.new.title\n");
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\n  \
             delegate :name, to: :author\nend\n",
        );
        let comment = harness.write(
            "app/models/comment.rb",
            "module Reports::Registry\n  Row = Struct.new(:total)\nend\nclass Comment < \
             ApplicationRecord\n  belongs_to :story\nend\n",
        );
        harness.watch(&[&story, &comment]);
        let before = harness.every_generated_document();
        assert!(before.len() >= 3, "the fixture feeds several generators");

        harness.analysis.dirty = true;
        harness.analysis.settle();

        assert_eq!(
            harness.every_generated_document(),
            before,
            "a settle over an unchanged workspace is a settle that changes nothing"
        );
    }

    /// A namespace only a **gem** declares is one a generated name may be spelled under.
    ///
    /// It lands in `Namespaces::spellable`: a joined `class Shouty::Thing::Point` introduces
    /// `Shouty`, which is safe exactly when something declares `Shouty`, and "something" was never
    /// meant to be "this application". What is **not** widened is which class a macro may name;
    /// `synthesized.md` has the measurement that settles it.
    #[test]
    fn a_namespace_only_a_gem_declares_is_one_a_generated_name_may_hang_off() {
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty::Thing::Point.new.x\n");
        harness.write(
            "app/models/thing.rb",
            "class Shouty::Thing\n  Point = Struct.new(:x)\nend\n",
        );
        harness.index();
        assert!(
            !harness.has("Shouty::Thing::Point#x()"),
            "with nothing declaring `Shouty`, the joined name would introduce it"
        );

        harness.index_gems();

        assert!(
            harness.has("Shouty::Thing::Point#x()"),
            "the gem declares `Shouty`, so the joined name introduces nothing"
        );
    }

    /// One document may open the same body twice, and `Synthesized::record` must keep it.
    ///
    /// A file under a namespace **nothing declares** writes its members one body per segment, so a
    /// file declaring on the module *and* on a class inside it opens `class Ns` twice. Reopening is
    /// legal RBS, and the parse gate is where a mistake about that goes silent: it would drop
    /// one application's whole `include` document with every test green.
    #[test]
    fn a_document_that_opens_one_body_twice_is_still_indexed() {
        let mut harness = Harness::new();
        let file = harness.write(
            "app/services/ns/admin.rb",
            "module Ns::Admin\n  # @return [String]\n  def self.label\n  end\n\n  \
             class Panel\n    # @return [Integer]\n    def size\n    end\n  end\nend\n",
        );
        harness.watch(&[&file]);

        let rbs = harness.generated_rbs("app/services/ns/admin.rb");
        // Opened twice, in two spellings, the second from the conjured namespace:
        // `app/services/ns/` declares `Ns`, so the singleton body is *wrapped* in it and `Panel`'s
        // is joined onto it. Two openings either way, which is what the parse gate is asked about.
        assert_eq!(rbs.matches("module Admin\n").count(), 1, "{rbs}");
        assert_eq!(rbs.matches("module Ns::Admin\n").count(), 1, "{rbs}");
        assert!(harness.has("Ns::Admin::<Admin>#label()"), "{rbs}");
        assert!(harness.has("Ns::Admin::Panel#size()"), "{rbs}");
    }

    /// A `def` inside a compact-path class belongs to that class, not to `Object`.
    ///
    /// Stated as a member, not a declaration, because the missing declaration was the mild half.
    /// `class User::Policy::NotAlreadySilenced` in a file Rails loads is ordinary Rails. With
    /// nothing declaring `User::Policy`, rubydex binds the plain `def`s inside by walking the
    /// lexical chain past a class not yet declared, stopping at `Object`: `call` lands on every
    /// object in the workspace, and the `def` line answers nothing. Declaring the namespace the
    /// directory names is what Ruby does anyway, and the binding follows.
    #[test]
    fn a_def_in_a_class_the_autoloader_namespaces_is_a_member_of_that_class() {
        let mut harness = Harness::new();
        harness.write("app/models/user.rb", "class User\nend\n");
        let policy = harness.write(
            "app/services/user/policy/not_already_silenced.rb",
            "class User::Policy::NotAlreadySilenced\n  def call\n  end\nend\n",
        );
        harness.watch(&[&policy]);

        assert!(harness.has("User::Policy"));
        assert!(harness.has("User::Policy::NotAlreadySilenced#call()"));
        // The wrong-answer half: without this, every receiver in the workspace answers `call`, and
        // the jump lands in a service object.
        assert!(!harness.has("Object#call()"));
    }

    /// The namespace is conjured by the **directory**, so a file that declares it wins.
    ///
    /// The filter's other side, tested with a `class`: a directory conjures a `module`, and a
    /// `policy.rb` writing `class User::Policy` would be overruled by a generated body with the
    /// other keyword. rubydex holds one declaration per constant, so the two kinds are not a
    /// reopening but a coin toss.
    #[test]
    fn a_namespace_a_file_declares_is_not_conjured_a_second_time() {
        let mut harness = Harness::new();
        let user = harness.write("app/models/user.rb", "class User\nend\n");
        let declared = harness.write(
            "app/services/user/policy.rb",
            "class User::Policy\n  def self.all\n  end\nend\n",
        );
        let policy = harness.write(
            "app/services/user/policy/not_already_silenced.rb",
            "class User::Policy::NotAlreadySilenced\n  def call\n  end\nend\n",
        );
        harness.watch(&[&user, &declared, &policy]);

        assert!(harness.has("User::Policy::NotAlreadySilenced#call()"));
        // The file's own kind survived, which is what the filter protects: a generated
        // `module User::Policy` would have taken this singleton with it.
        assert!(harness.has("User::Policy::<Policy>#all()"));
    }

    /// A directory declares its module beside a file that writes `module` too.
    ///
    /// Zeitwerk defines `Reports` from `app/jobs/reports/` whether or not `lib/reports.rb` exists,
    /// so deleting the written line leaves the constant and the confirming file is a place of it.
    /// The written line comes first: a generated body sorts behind every file.
    #[test]
    fn a_namespace_a_module_line_declares_keeps_its_directory_s_places_after_it() {
        let mut harness = Harness::new();
        let written = harness.write("lib/reports.rb", "module Reports\n  LIMIT = 1\nend\n");
        let nightly = "class Reports::Nightly\n  def perform\n  end\nend\n";
        let job = harness.write("app/jobs/reports/nightly.rb", nightly);
        harness.watch(&[&written, &job]);

        assert_eq!(
            linked(&harness.definition_at(&job, nightly, "Reports::Nightly")),
            ["reports.rb:0:7", "nightly.rb:0:6"]
        );
        assert_eq!(
            harness.generated_rbs("app/jobs/reports/nightly.rb"),
            "module Reports\nend\n"
        );
    }

    /// Two directories spelling one namespace: one constant, with a place in each.
    ///
    /// `app/jobs/reports/` and `app/services/reports/` both name `Reports`. Neither directory is a
    /// line anyone can be sent to, but both files are, both write the constant, and deleting either
    /// keeps it, so both declare it and the merged declaration has a definition from each. The
    /// graph hands documents over in no order, so `Context::autoloaded` sorts: the order of places
    /// is what a reader gets, and two runs must not differ.
    #[test]
    fn one_namespace_two_directories_is_declared_in_both() {
        let mut harness = Harness::new();
        let job = harness.write(
            "app/jobs/reports/nightly.rb",
            "class Reports::Nightly\n  def perform\n  end\nend\n",
        );
        let service = harness.write(
            "app/services/reports/build.rb",
            "class Reports::Build\n  def call\n  end\nend\n",
        );
        harness.watch(&[&job, &service]);

        assert!(harness.has("Reports::Nightly#perform()"));
        assert!(harness.has("Reports::Build#call()"));
        assert!(!harness.has("Object#perform()"));
        assert!(!harness.has("Object#call()"));
        // One body per confirming file; the directory keys nothing now.
        assert_eq!(
            harness.generated_rbs("app/jobs/reports/nightly.rb"),
            "module Reports\nend\n"
        );
        assert_eq!(
            harness.generated_rbs("app/services/reports/build.rb"),
            "module Reports\nend\n"
        );
        assert!(harness.generated_rbs("app/jobs/reports").is_empty());
    }

    /// A namespace only a directory declares is a place in every file that writes it.
    ///
    /// Without this, `hover` read `module Mod` from the conjured declaration while `definition`
    /// answered nothing, because the declaration was keyed by a directory, which has no line.
    ///
    /// The places are the `Mod` of each `class Mod::…`, where Ruby's own operational test puts the
    /// declaration (no single file declares it, all of them do), in path order:
    /// `Context::autoloaded`'s sort, not the walk's.
    #[test]
    fn a_namespace_only_a_directory_declares_is_a_place_in_every_file_that_writes_it() {
        let mut harness = Harness::new();
        // Three references to `Mod` in the first file, and only the path's may be the place: the
        // use above the class is outside the construct, and the superclass is inside but later.
        // `FLAG` is a declaration whose parent is not `Mod`, which the window skips.
        let flagged = "\
Mod::Audit.record
class Mod::FlaggedController < Mod::ModController
  FLAG = 1
  def index
    Mod::Audit.record
  end
end
";
        let notes = "class Mod::NotesController\n  def show\n  end\nend\n";
        let one = harness.write("app/controllers/mod/flagged_controller.rb", flagged);
        let two = harness.write("app/controllers/mod/notes_controller.rb", notes);
        harness.watch(&[&one, &two]);

        let targets = harness.definition_at(&one, flagged, "Mod::FlaggedController");
        let targets = targets.as_array().expect("an array").clone();
        assert_eq!(targets.len(), 2, "{targets:?}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(one.as_str()));
        assert_eq!(targets[1]["targetUri"], serde_json::json!(two.as_str()));
        // The one segment on the `class` line: line 1, not the `Mod::Audit` above or the superclass
        // after.
        assert_eq!(
            targets[0]["targetSelectionRange"],
            serde_json::json!({
                "start": { "line": 1, "character": 6 },
                "end": { "line": 1, "character": 9 },
            }),
            "{targets:?}"
        );
        // One segment, so the namespace the declaration opens is that segment.
        assert_eq!(
            targets[0]["targetRange"], targets[0]["targetSelectionRange"],
            "{targets:?}"
        );
        // The card still reads the same, the half that was never broken.
        let markdown =
            harness.hover_at(&one, flagged, "Mod::FlaggedController")["contents"]["value"]
                .as_str()
                .expect("markdown")
                .to_owned();
        assert!(markdown.contains("module Mod"), "{markdown}");

        // The picker gains it as a consequence, not a second rule: `search` drops a row whose
        // `locator::site` is `None`, so a namespace nothing could jump to was one nobody could
        // search for either.
        let names: Vec<String> = harness
            .ask("workspace/symbol", serde_json::json!({ "query": "Mod" }))
            .as_array()
            .into_iter()
            .flatten()
            .map(|symbol| symbol["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        assert!(names.contains(&"Mod".to_owned()), "{names:?}");
        // Once, though two files declare it: the picker offers declarations, and this is one.
        assert_eq!(
            names.iter().filter(|name| *name == "Mod").count(),
            1,
            "{names:?}"
        );
    }

    /// A chain of directories is a place per segment, each its own bytes.
    ///
    /// `app/controllers/api/v1/accounts/` conjures three namespaces from one file, and the three
    /// spans slice one constant path. `full` is the namespace the declaration opens and stops
    /// before the class's own name, a different constant.
    #[test]
    fn every_segment_of_a_conjured_chain_is_its_own_place() {
        let mut harness = Harness::new();
        let source = "class Api::V1::Accounts::CredentialsController\n  def show\n  end\nend\n";
        let uri = harness.write(
            "app/controllers/api/v1/accounts/credentials_controller.rb",
            source,
        );
        harness.watch(&[&uri]);

        // `Api::V1::Accounts` is bytes 6..23 of the line; the three segments slice it.
        for (needle, start, end) in [
            ("Api::V1::Accounts::Credentials", 6, 9),
            ("V1::Accounts::Credentials", 11, 13),
            ("Accounts::Credentials", 15, 23),
        ] {
            let targets = harness.definition_at(&uri, source, needle);
            let targets = targets.as_array().expect("an array").clone();
            assert_eq!(targets.len(), 1, "{needle}: {targets:?}");
            assert_eq!(
                targets[0]["targetSelectionRange"],
                serde_json::json!({
                    "start": { "line": 0, "character": start },
                    "end": { "line": 0, "character": end },
                }),
                "{needle}: {targets:?}"
            );
            assert_eq!(
                targets[0]["targetRange"],
                serde_json::json!({
                    "start": { "line": 0, "character": 6 },
                    "end": { "line": 0, "character": 23 },
                }),
                "{needle}: {targets:?}"
            );
        }
    }

    /// A namespace some file really declares keeps that file's place and gains no others.
    ///
    /// The bound on the rule: `User::Policy` is conjured only while nothing declares it, so a
    /// `policy.rb` beside the directory removes the name from `Context::autoloaded`, and the
    /// `class User::Policy` line answers alone. The twelve files under `user/policy/` must not
    /// become places for it.
    #[test]
    fn a_namespace_a_file_declares_is_not_given_the_files_that_open_it() {
        let mut harness = Harness::new();
        let declared = "class User::Policy\n  def self.all\n  end\nend\n";
        let opener = "class User::Policy::NotAlreadySilenced\n  def call\n  end\nend\n";
        let user = harness.write("app/models/user.rb", "class User\nend\n");
        let policy = harness.write("app/services/user/policy.rb", declared);
        let silenced = harness.write("app/services/user/policy/not_already_silenced.rb", opener);
        harness.watch(&[&user, &policy, &silenced]);

        let targets = harness.definition_at(&silenced, opener, "Policy");
        let targets = targets.as_array().expect("an array").clone();
        assert_eq!(targets.len(), 1, "{targets:?}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(policy.as_str()));
    }

    /// A model whose whole namespace the application declares is left joined, and still answers.
    ///
    /// The half that is *not* a repair: the damage comes from a segment **nothing declares**, so a
    /// name whose every segment some file writes is left joined as written. Asserting it keeps the
    /// rule from spreading: writing a body per segment unconditionally costs real positions on
    /// names like `Comment::Relation`.
    #[test]
    fn a_model_inside_a_module_is_left_joined_and_still_answers() {
        let source = "Admin.table_name_prefix\n";
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.write(
            "app/models/admin.rb",
            "module Admin\n  def self.table_name_prefix\n    \"admin_\"\n  end\nend\n",
        );
        harness.write("app/models/admin/deep.rb", "module Admin::Deep\nend\n");
        harness.write(
            "app/models/admin/deep/setting.rb",
            "class Admin::Deep::Setting < ApplicationRecord\n  has_many :users\nend\n",
        );
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let rbs = harness.generated_rbs("app/models/admin/deep/setting.rb");
        assert!(
            rbs.contains("module Admin::Deep\nclass Setting\n"),
            "the parent is a module the application writes down: {rbs}"
        );
        assert!(harness.has("Admin::Deep::Setting#users()"), "{rbs}");
        assert!(
            harness.has("User::Relation"),
            "a relation class is named after the element and stays joined: {rbs}"
        );
        let card = card(&mut harness, &uri, source, "table_name_prefix");
        assert!(
            !card.contains("Guessed from name alone"),
            "the module keeps its own singleton: {card}"
        );
    }

    #[test]
    fn exactly_one_routes_file_declares_each_helper() {
        // Two routes files naming one helper would land in two generated documents, where `Facts`'
        // precedence cannot see them and RBS would read two `def story_path:` lines as an overload
        // set. The first in URI order writes it, as for a shared relation class.
        let (mut harness, _uri) = routes_project("");
        let engine = harness.write(
            "engines/blog/config/routes.rb",
            "Rails.application.routes.draw do\n  resources :stories, only: [:index]\n  resources :posts, only: [:index]\nend\n",
        );
        harness.watch(&[&engine]);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("RouteHelpers#stories_path()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "one declaration, however many files name the route"
        );
        // Whichever file sorts first keeps it; here, the application's own.
        let engine_rbs = harness.generated_rbs("engines/blog/config/routes.rb");
        let main = harness.generated_rbs("config/routes.rb");
        assert!(main.contains("def stories_path:"), "{main}");
        assert!(main.contains("def story_path:"), "{main}");
        assert!(engine_rbs.contains("def posts_path:"), "{engine_rbs}");
        assert!(
            !engine_rbs.contains("def stories_path:"),
            "`config/` sorts before `engines/`, so the application's own file keeps it: {engine_rbs}"
        );
    }

    #[test]
    fn an_application_that_declares_the_module_itself_keeps_it() {
        // The `Comment::Relation` rule, costing the whole feature here: a project that spelled
        // `RouteHelpers` meant something by it, and a module this pass wrote into would answer with
        // its members and ours at once.
        let (mut harness, _uri) = routes_project("");
        assert!(harness.has("RouteHelpers#story_path()"));
        let theirs = harness.write(
            "app/models/route_helpers.rb",
            "module RouteHelpers\n  def story_path\n  end\nend\n",
        );
        harness.watch(&[&theirs]);
        assert_eq!(
            harness
                .analysis
                .graph
                .get("RouteHelpers#story_path()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "theirs, and only theirs"
        );
        assert!(!harness.has("RouteHelpers#admin_flags_path()"));
    }

    #[test]
    fn a_routes_file_that_stops_declaring_takes_its_helpers_with_it() {
        // The pruning rule, on the one generator whose document also carries the `include`s: an
        // empty routes file must leave neither a helper nor a host behind.
        let (mut harness, _uri) = routes_project("");
        assert!(harness.has("RouteHelpers#story_path()"));
        let routes = harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\nend\n",
        );
        harness.watch(&[&routes]);
        assert!(!harness.has("RouteHelpers#story_path()"));
        assert!(!harness.has("RouteHelpers#admin_flags_path()"));
    }

    /// A project that declares `ActiveRecordRelation` itself keeps it, and loses the feature.
    ///
    /// The `Comment::Relation` and `ROUTE_HELPERS` rule, asked of the one class the query interface
    /// invents. What it withdraws is the **relations**, by the same collision rule: with no
    /// relation class there is nothing for a `has_many` to return, so that whole half declines
    /// together instead of leaving a `-> Comment::Relation` naming an undeclared class, or a
    /// relation inheriting whatever the user meant by the name.
    #[test]
    fn a_project_that_declares_the_relation_base_itself_is_not_shadowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        let own = harness.write(
            "app/lib/active_record_relation.rb",
            "class ActiveRecordRelation\n  def mine\n  end\nend\n",
        );
        harness.index();

        assert!(
            harness.has("ActiveRecordRelation#mine()"),
            "their class stands"
        );
        // And a reader sees it under the name they wrote: it is not the class ya-lsp would have
        // shown as `ActiveRecord::Relation`.
        assert_eq!(
            harness.hover_at(
                &own,
                "class ActiveRecordRelation\n  def mine\n  end\nend\n",
                "mine"
            )["contents"]["value"],
            "```ruby\nActiveRecordRelation#mine\n```"
        );
        assert!(
            !harness.has("ActiveRecordRelation#where()"),
            "and this pass writes nothing into it"
        );
        let rbs = harness.generated_rbs("app/models/story.rb");
        assert!(!rbs.contains("Relation"), "{rbs}");
        assert!(
            !rbs.contains("def comments:"),
            "the whole half declines together: {rbs}"
        );

        // Remove the name and the whole feature returns, showing the decline is about the collision
        // and nothing else in the fixture.
        std::fs::remove_file(own.to_file_path().unwrap()).unwrap();
        harness.watch(&[&own]);
        assert!(harness.has("ActiveRecordRelation#where()"));
        let rbs = harness.generated_rbs("app/models/story.rb");
        assert!(
            rbs.contains("class Comment::Relation < ActiveRecordRelation\n"),
            "{rbs}"
        );
        assert!(
            rbs.contains("def comments: () -> Comment::Relation\n"),
            "{rbs}"
        );
    }

    #[test]
    fn an_association_added_after_the_index_answers_and_a_deleted_one_stops() {
        // Both directions of the schema pass's property: generators run before every resolve, so a
        // macro typed into a model answers on the next settle, and deleting it removes its member.
        let (mut harness, _story, _uri) = models_project("");
        assert!(!harness.has("User#stories()"));

        let user = harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\n  has_many :stories\nend\n",
        );
        harness.watch(&[&user]);
        assert!(harness.has("User#stories()"));

        let user = harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.watch(&[&user]);
        assert!(!harness.has("User#stories()"));
    }

    #[test]
    fn a_table_no_class_claims_declares_nothing() {
        // Class → table, in the deciding case: `widgets` has a column and there is no `Widget`, so
        // nothing is declared, rather than inventing a class or singularizing onto something that
        // exists.
        let (harness, _schema, _uri) = rails_project("Story.new\n");
        assert!(harness.has("Story#title()"));
        assert!(!harness.has("Widget#name()"));
        assert!(harness.analysis.graph.get("Widget").is_none());
    }

    #[test]
    fn a_model_added_after_the_index_makes_its_table_answer() {
        // Why generation runs before every resolve instead of when the schema changes: the *other*
        // input is which classes exist, and `rails generate model` writes a file unrelated to
        // `db/schema.rb`'s mtime.
        let (mut harness, _schema, _uri) = rails_project("Widget.new\n");
        assert!(!harness.has("Widget#name()"));

        let widget = harness.write("app/models/widget.rb", "class Widget\nend\n");
        harness.watch(&[&widget]);

        assert!(harness.has("Widget#name()"), "a new model was not noticed");
    }

    #[test]
    fn editing_the_schema_replaces_what_it_declared() {
        // The failure the side table exists to prevent, through the generator that fills it: a
        // renamed column must stop answering under its old name, and silently would not.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        harness.open(&schema, SCHEMA_RB);
        harness.change(&schema, &SCHEMA_RB.replace("\"title\"", "\"headline\""));

        assert!(harness.has("Story#headline()"));
        assert!(
            !harness.has("Story#title()"),
            "the column that was renamed is still answering"
        );
        assert!(harness.definition_at(&uri, source, "title").is_null());
    }

    #[test]
    fn a_project_with_no_schema_generates_nothing_at_all() {
        // What a non-Rails project pays for all of the above, which must be nothing: one hash
        // lookup per settle, no document, no table entry.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "class Story\nend\n");
        harness.index();

        assert!(harness.analysis.synthesized.is_empty());
        assert!(
            harness.definition_at(&uri, "class Story\nend\n", "Story")[0]["targetUri"].is_string()
        );
    }

    #[test]
    fn a_keystroke_in_a_file_the_pass_does_not_read_does_not_run_the_pass() {
        // `settle` runs this pass before every `resolve`, and a forced settle precedes every
        // graph-reading request, so without a gate a keystroke in a file with no macros would pay
        // for a whole-workspace regeneration in front of every completion.
        //
        // Three assertions, the middle one being the gate: nothing is skipped until a pass has run,
        // a keystroke in a file no generator opens is skipped, and every answer survives.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        let plain = "class Plain\n  def a\n  end\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        let before = harness.analysis.passes;

        harness.open(&uri, plain);
        harness.change(&uri, "class Plain\n  def ab\n  end\nend\n");
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.passes, before,
            "the generators ran for a file none of them opens"
        );
        assert!(
            harness.has("Story#title()"),
            "and the columns are still there"
        );

        // What a naive "does *this* document declare anything" test gets wrong: a new class name is
        // a new answer for every macro anywhere that names it, so the projection moves and the pass
        // runs.
        harness.change(&uri, "class Renamed\n  def a\n  end\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.passes > before,
            "a class the workspace did not have before is not nothing"
        );
    }

    #[test]
    fn a_keystroke_in_a_file_the_pass_does_not_read_does_not_walk_the_workspace_either() {
        // The property the per-document gate rests on: what is held must be a function of **what
        // the document contributes**, not of the document, proven by changing a body without
        // changing its contribution.
        //
        // `passes` cannot see this (the outer gate already stops the generators; what remains is
        // rebuilding the projection to decide that). So `walks` is the instrument, and the two
        // assertions are the point: an edit rewriting most of a file walks nothing, and one
        // renaming the class it defines walks everything.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);

        let plain = "class Plain\n  def a\n  end\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        let walks = harness.analysis.walks;

        harness.open(&uri, plain);
        // A comment, a method rename, a local, a whole second method and a string literal: what a
        // file is mostly made of, and none of it read by any `Contribution` field.
        harness.change(
            &uri,
            "# what this class is for\nclass Plain\n  def ab\n    here = \"and gone\"\n    here\n  end\n\n  def second\n  end\nend\n",
        );
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.walks, walks,
            "the workspace was walked for a change no projection of it can see"
        );
        assert!(
            harness.has("Story#title()"),
            "and the columns are still there"
        );

        // The other half, same file: the one line the walk *does* read.
        harness.change(&uri, "class Renamed\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.walks > walks,
            "a class the workspace did not have before is not nothing"
        );
    }

    #[test]
    fn a_keystroke_in_a_model_file_runs_the_generators_and_does_not_walk_the_workspace() {
        // **The two gates must not share a clause.** "Would the walk produce the projection already
        // held" and "has a file some generator reads changed" are different questions:
        // `has_many :comments` becoming `has_many :tags` changes what the file says but nothing in
        // the projection, so the generators must run and the walk must not.
        //
        // `walks` and `passes` together show it, and both assertions must be made together: a pass
        // that did not run would satisfy the first for the wrong reason.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        let comment = harness.write("app/models/comment.rb", "class Comment\nend\n");
        let tag = harness.write("app/models/tag.rb", "class Tag\nend\n");
        harness.watch(&[&story, &comment, &tag]);
        harness.open(
            &story,
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        assert!(harness.has("Story#comments()"));

        let (walks, passes) = (harness.analysis.walks, harness.analysis.passes);
        harness.change(
            &story,
            "class Story < ApplicationRecord\n  has_many :tags\nend\n",
        );

        assert_eq!(
            harness.analysis.walks, walks,
            "the workspace was walked for an edit that changed nothing the walk reads"
        );
        assert!(
            harness.analysis.passes > passes,
            "and the generators did not run for an edit that changed what a file declares"
        );
        assert!(harness.has("Story#tags()"), "the new macro took effect");
        assert!(
            !harness.has("Story#comments()"),
            "and the one it replaced stopped answering"
        );
    }

    #[test]
    fn a_struct_or_define_method_file_is_read_again_only_when_it_or_a_name_it_asked_moved() {
        // The two readers' memos. A settle over files that did not move reads none of them, an
        // edit reads the file edited, and a class a struct's reader could not spell before is
        // asked again once another file declares its namespace: the text did not move, the answer
        // it rests on did.
        let mut harness = Harness::new();
        let point = harness.write(
            "lib/point.rb",
            "class Wrapper::Point < Struct.new(:x)\nend\n",
        );
        let shape = harness.write(
            "lib/shape.rb",
            "class Wrapper::Shape\n  define_method(:area) { 1 }\nend\n",
        );
        harness.index();
        assert!(
            !harness.has("Wrapper::Point#x()"),
            "nothing declares `Wrapper` yet"
        );
        assert!(!harness.has("Wrapper::Shape#area()"));

        let reads = harness.analysis.reads();
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.reads(),
            reads,
            "a settle re-read unmoved files"
        );

        let wrapper = harness.write("lib/wrapper.rb", "module Wrapper\nend\n");
        harness.watch(&[&wrapper]);
        assert!(harness.has("Wrapper::Point#x()"));
        assert!(harness.has("Wrapper::Shape#area()"));
        assert_eq!(
            harness.analysis.reads(),
            reads + 1,
            "only the struct file, whose answer moved, is read again"
        );

        harness.write(
            "lib/shape.rb",
            "class Wrapper::Shape\n  define_method(:size) { 1 }\nend\n",
        );
        harness.watch(&[&shape]);
        assert!(harness.has("Wrapper::Shape#size()"));
        assert!(!harness.has("Wrapper::Shape#area()"));
        assert!(
            harness.has("Wrapper::Point#x()"),
            "{point:?} still declares what it did"
        );
        assert_eq!(harness.analysis.reads(), reads + 2);

        // Gone from disk before anything says so: the pass finds it unreadable, and what it
        // declared goes with it.
        std::fs::remove_file(harness.root.path().join("lib/point.rb")).unwrap();
        std::fs::remove_file(harness.root.path().join("lib/shape.rb")).unwrap();
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert!(!harness.has("Wrapper::Point#x()"));
        assert!(!harness.has("Wrapper::Shape#size()"));
    }

    #[test]
    fn a_keystroke_in_one_model_file_reads_that_file_and_no_other() {
        // The parse memo: a keystroke in a model file costs reading *that* file and nothing else.
        // Without it, one changed file makes the pass re-read and re-parse every file on every
        // list, to learn what all but one said last time.
        //
        // `reads` is the instrument, like `passes` and `walks`: the memo changes no answer, and a
        // file re-parsed into the same tree is indistinguishable from one not opened.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        let comment = harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\n  belongs_to :story\nend\n",
        );
        harness.watch(&[&story, &comment]);
        assert!(harness.has("Story#comments()"));

        // A settle over an untouched workspace reads nothing, the wider claim: the memo is keyed by
        // file, not by which file was edited.
        let reads = harness.analysis.reads();
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.reads(),
            reads,
            "a settle over an unchanged workspace re-read files"
        );

        // Opening is not a keystroke and costs one read of one file: a stamp and a buffer cannot be
        // compared, so the authority changing hands is a miss by construction. Once per open, one
        // file; the alternative is hashing the disk contents, which is the read.
        harness.open(
            &story,
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        let reads = harness.analysis.reads();

        // *Then* one keystroke in one of them is one read. The edit changes what the file declares,
        // so the pass is not gated out: this is the full pass opening exactly one file.
        harness.change(
            &story,
            "class Story < ApplicationRecord\n  has_many :comments\n  has_many :tags\nend\n",
        );
        assert_eq!(
            harness.analysis.reads(),
            reads + 1,
            "a keystroke in one model file read more than that file"
        );
        assert!(harness.has("Story#tags()"), "and the new macro took effect");
        assert!(
            harness.has("Comment#story()"),
            "and the file that was not re-read still declares what it declared"
        );
    }

    #[test]
    fn a_file_that_changes_on_disk_with_nobody_watching_is_read_again() {
        // The guarantee a memo most easily loses: the pass claims its answer is a function of what
        // is **on disk**, earned by re-reading everything each time, which is exactly what a memo
        // stops doing.
        //
        // The `stat` replaces the re-read (`Fresh::Disk` is the gate's own stamp, not a second
        // mechanism), so a `git checkout` rewriting a schema takes effect at the next settle with
        // no watcher notification. Nothing here calls `watch`; that is the test.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        // No sleeping needed: `stamp_of` reads length as well as modification time, because
        // filesystem clocks are coarse and two writes in one tick are a real edit.
        std::fs::write(
            schema.to_file_path().expect("a file uri"),
            SCHEMA_RB.replace("\"title\"", "\"headline\""),
        )
        .expect("rewrite the schema");

        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert!(
            harness.has("Story#headline()"),
            "a schema rewritten behind the server's back never took effect"
        );
        assert!(
            !harness.has("Story#title()"),
            "and the column it replaced is still answering"
        );

        // A file that is *gone* takes its parse with it, rather than leaving one nothing can
        // refresh: the same guarantee at the other end, and the one that would otherwise leave a
        // column answering forever.
        std::fs::remove_file(schema.to_file_path().expect("a file uri"))
            .expect("delete the schema");
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert!(
            !harness.has("Story#headline()"),
            "a schema deleted behind the server's back is still declaring columns"
        );
    }

    /// A file is stamped once, and only where a generator reads it from disk. RSpec reads a spec
    /// only while the editor holds it, from the buffer, so a closed spec's stamp said nothing and
    /// cost a `stat` a spec file a settle. A spec another module reads from disk stays stamped.
    #[test]
    fn only_what_a_generator_reads_from_disk_is_stamped() {
        let (mut harness, story) = bundle_in(Harness::configured("[rspec]\nenabled = true\n"));
        harness.write("lib/rspec.rb", RSPEC_CORE);
        // On the model list and the `Struct` list both.
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  Point = Struct.new(:x)\nend\n",
        );
        let spec = harness.write("spec/models/story_spec.rb", "describe Story do\nend\n");
        let pair = harness.write(
            "spec/models/pair_spec.rb",
            "Pair = Struct.new(:left)\n\ndescribe Pair do\nend\n",
        );
        harness.index();
        let path = |uri: &DocUri| uri.to_file_path().expect("a file uri");
        let stamped = |harness: &Harness, uri: &DocUri| {
            harness
                .analysis
                .stamps
                .iter()
                .filter(|(held, _)| *held == path(uri))
                .count()
        };
        assert!(harness.has("Story::Point#x()") && harness.has("Pair#left()"));
        assert_eq!(stamped(&harness, &story), 1, "once, on two lists");
        assert_eq!(
            stamped(&harness, &pair),
            1,
            "the `Struct` is read from disk"
        );
        assert_eq!(stamped(&harness, &spec), 0);

        // Rewritten closed, with nobody watching: nothing the pass reads moved.
        let passes = harness.analysis.passes;
        std::fs::write(path(&spec), "describe Story do\n  let(:x) { 1 }\nend\n")
            .expect("rewrite the spec");
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert_eq!(harness.analysis.passes, passes);
        // A file the memo stamped is still checked: the stamp is the one the read took.
        std::fs::write(
            path(&story),
            "class Story < ApplicationRecord\n  Point = Struct.new(:y)\nend\n",
        )
        .expect("rewrite the model");
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert_eq!(harness.analysis.passes, passes + 1);
        assert!(harness.has("Story::Point#y()"));
    }

    #[test]
    fn a_file_whose_name_ends_schema_rb_and_is_not_a_schema_is_never_read_as_one() {
        // The suffix is the cheap half; the module's own reader is the rule: a dump lives in `db/`,
        // so `legacy_schema.rb` anywhere else is ordinary code ending in those nine characters. It
        // is on `rails_lists::SCHEMAS` (the `Wants` row matches the name) but never handed to the
        // reader, which is why the filter sits where the pass decides what to *open*, not where it
        // declares.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        // The decoy is a **copy of the real schema**, so one assertion suffices: a table two schema
        // sources declare is declared by neither, so reading the decoy would remove `stories` from
        // both and `Story#title` would stop answering. Asserting that a decoy-only table is absent
        // would prove nothing, since no class claims it either way.
        let decoy = harness.write("lib/legacy_schema.rb", SCHEMA_RB);
        harness.watch(&[&decoy]);

        assert!(
            harness.has("Story#title()"),
            "a file named like a schema and living outside db/ was read as one"
        );
    }

    #[test]
    fn a_file_that_joins_a_list_without_changing_is_read_for_the_reader_it_joined_for() {
        // The memo compares which readers an entry was built for, not only its text, and this is
        // the one shape that needs it. Every list but one depends only on a document's own content.
        // `rails_lists::MODELS` does not: `Analysis::walk` adds, after the walk, every **model**
        // that writes no macro, and being a model depends on a superclass chain through *other
        // files*. So `class Widget < Base` joins the model list as soon as another file makes
        // `Base` a model, with `widget.rb` unchanged.
        //
        // A text-only memo would then serve an entry read for the `@return` tag that never ran the
        // model reader, and `Widget` would silently get no relation class.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let base = harness.write("app/models/base.rb", "class Base\nend\n");
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget < Base\n  # @return [String]\n  def label\n    \"x\"\n  end\nend\n",
        );
        harness.watch(&[&base, &widget]);
        assert!(harness.has("Widget#label()"), "the tag declared its method");
        assert!(
            !harness.has("Widget::Relation#first()"),
            "and nothing has made Widget a model yet"
        );

        harness.open(&base, "class Base\nend\n");
        let reads = harness.analysis.reads();
        harness.change(&base, "class Base < ApplicationRecord\nend\n");

        assert!(
            harness.has("Widget::Relation"),
            "the file that joined the model list was not read for the reader it joined for"
        );
        assert!(
            harness.has("Widget#label()"),
            "and what it was already read for is still declared"
        );
        // Two files: the edited one, and the one that joined a list because of it.
        assert_eq!(harness.analysis.reads(), reads + 2);
    }

    #[test]
    fn a_document_the_walk_never_visits_is_never_skipped_by_it() {
        // The clause making one `Contribution` per document enough, and the one case it cannot
        // cover. `Analysis::contribution` declines an `.rbs` outright (it would be handed to a Ruby
        // parser), but `bundle_namespaces` reads **every** definition in the graph, so a `module`
        // in the project's `sig/` can still move a `Context`. A document with no contribution is
        // not one with an unchanged contribution, so the gate refuses it.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);

        let signature = "class Plain\nend\n";
        let uri = harness.write("sig/plain.rbs", signature);
        harness.index();
        harness.analysis.settle();
        let walks = harness.analysis.walks;

        harness.open(&uri, signature);
        harness.change(&uri, "class Plain\n  def a: () -> String\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.walks > walks,
            "a signature file was treated as contributing nothing rather than as unreadable"
        );
    }

    #[test]
    fn the_gate_still_looks_at_the_disk_rather_than_trusting_a_notification() {
        // The guarantee the gate must not weaken: the pass's answer is a function of what is on
        // disk, earned by re-reading everything. A `git checkout` deleting `db/schema.rb` sends no
        // notification until the watcher catches up, so a gate trusting notifications alone would
        // keep answering with the deleted file's columns.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        let plain = "class Plain\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();

        // Behind the server's back, then a keystroke somewhere unrelated.
        std::fs::remove_file(schema.to_file_path().unwrap()).unwrap();
        harness.open(&uri, plain);
        harness.change(&uri, "class Plain\n  def a\n  end\nend\n");
        harness.analysis.settle();

        assert!(
            !harness.has("Story#title()"),
            "a schema that is gone still declares its columns"
        );
    }

    #[test]
    fn a_structure_dump_that_appears_is_not_a_document_and_is_looked_for_anyway() {
        // The one pass input that is **not** a projection of the graph (`db/*structure.sql`), so
        // the one thing contributions cannot cover: a `.sql` is not a document, so no `WANTS` row
        // or contribution can see one appear. A `git checkout` bringing one in sends no
        // notification the gate may trust, as with the deletion above.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        std::fs::remove_file(schema.to_file_path().unwrap()).unwrap();

        let plain = "class Plain\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        assert!(!harness.has("Story#title()"), "the schema is gone");

        // Behind the server's back, then a keystroke in a file that declares nothing.
        std::fs::write(
            harness.root.path().join("db/structure.sql"),
            "CREATE TABLE stories (\n  title character varying\n);\n",
        )
        .unwrap();
        harness.open(&uri, plain);
        harness.change(&uri, "class Plain\n  def a\n  end\nend\n");
        harness.analysis.settle();

        assert!(
            harness.has("Story#title()"),
            "a dump that appeared was never looked for"
        );
    }

    #[test]
    fn a_schema_that_vanished_under_the_index_declares_nothing() {
        // Indexed and readable are different questions, with a real gap: a `git checkout` removes
        // the file, and the next settle runs before the watcher notification. Nothing to read means
        // nothing to say, not the last thing said.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        std::fs::remove_file(schema.to_file_path().unwrap()).unwrap();
        harness.open(&uri, source);
        harness.change(&uri, source);

        assert!(harness.analysis.synthesized.is_empty());
        assert!(!harness.has("Story#title()"));
    }

    /// A project with one file from every family the `[rails]` and `[types]` switches govern.
    ///
    /// One fixture for eight tests, because each test is really about the **other seven**: a switch
    /// that turns off more than its own family is the failure worth catching, visible only with all
    /// eight present.
    fn every_family(config: &str) -> Harness {
        let mut harness = Harness::configured(config);
        harness.write("db/schema.rb", SCHEMA_RB);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        // The association names a class the application defines, or it declines; that is the rule,
        // not fussiness.
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :stories\nend\n",
        );
        harness.write(
            "app/jobs/import_job.rb",
            "class ImportJob < ApplicationJob\n  def perform\n  end\nend\n",
        );
        harness.write("app/models/point.rb", "Point = Struct.new(:x)\n");
        harness.write(
            "app/models/annotated.rb",
            "class Annotated\n  # @return [Story]\n  def story\n  end\nend\n",
        );
        harness.write(
            "app/helpers/story_helper.rb",
            "module StoryHelper\n  def shout\n  end\nend\n",
        );
        // The framework family is two files: the `config/application.rb` it is declared from, and
        // the bundle whose constants both ends of every row are checked against.
        harness.write(
            "config/application.rb",
            "module Shop\n  class Application < Rails::Application\n  end\nend\n",
        );
        harness.write("lib/bundle.rb", FRAMEWORK_BUNDLE);
        harness.index();
        harness
    }

    /// What a bundle declares of the four constants the framework table names.
    ///
    /// `module Rails` and classes for the rest, because which keyword opens each body is read from
    /// the graph, not assumed; a fixture spelling them all the same would not test that.
    pub(crate) const FRAMEWORK_BUNDLE: &str = "\
module Rails
  def self.root; end
  def self.cache; end
  def self.application; end
  class Application
    def routes; end
  end
end
class Pathname
  def join(*args); end
end
module ActiveSupport
  class TimeZone
    def now; end
  end
  module Cache
    class Store
      def fetch(name); end
    end
  end
end
class Time
  def self.zone; end
end
";

    /// What each family writes when on, named once so eight tests cannot disagree.
    ///
    /// The **generated RBS**, not `Harness::has`, because the question is whether this generator
    /// ran at all. A declaration can go missing for a dozen downstream reasons, each of which would
    /// look like a switch working.
    const FAMILIES: [(&str, &str, &str); 7] = [
        ("schema", "db/schema.rb", "def title:"),
        ("models", "app/models/story.rb", "def comments:"),
        ("routes", "config/routes.rb", "def story_path:"),
        (
            "entrypoints",
            "app/jobs/import_job.rb",
            "def self.perform_later:",
        ),
        ("structs", "app/models/point.rb", "def x:"),
        ("annotations", "app/models/annotated.rb", "def story:"),
        // The one family without its own key: gated by the umbrella, so it is absent from the
        // per-switch loop and present in the umbrella's test. Hosted on the file declaring `Rails`,
        // which here is the fixture's bundle.
        ("framework", "lib/bundle.rb", "def self.root:"),
    ];

    /// Every family but `absent` asserted present; `absent` asserted gone.
    fn only_missing(harness: &Harness, absent: &str) {
        for (family, source, declared) in FAMILIES {
            let rbs = harness.generated_rbs(source);
            if family == absent {
                assert!(
                    !rbs.contains(declared),
                    "{family} is off and {source} still declares {declared}: {rbs}"
                );
            } else {
                assert!(
                    rbs.contains(declared),
                    "{absent} is off and it took {family}'s {declared} with it: {rbs}"
                );
            }
        }
    }

    #[test]
    fn every_family_declares_something_with_nothing_configured() {
        // The guard the two tests below need: if a family declared nothing anyway, every one of
        // them would pass by accident.
        let harness = every_family("");
        for (family, source, declared) in FAMILIES {
            let rbs = harness.generated_rbs(source);
            assert!(rbs.contains(declared), "{family}: {declared} in {rbs}");
        }
    }

    #[test]
    fn each_switch_turns_off_its_own_family_and_nothing_else() {
        // The claim all of `[rails]` rests on. A switch that took a neighbour with it would be
        // invisible in any project not using the neighbour, which is most projects.
        for (key, family) in [
            ("[rails]\nschema = false\n", "schema"),
            ("[rails]\nmodels = false\n", "models"),
            ("[rails]\nroutes = false\n", "routes"),
            ("[rails]\nentrypoints = false\n", "entrypoints"),
            ("[types]\nstructs = false\n", "structs"),
            ("[types]\nannotations = false\n", "annotations"),
        ] {
            let harness = every_family(key);
            only_missing(&harness, family);
        }
    }

    #[test]
    fn rails_off_takes_all_six_rails_families_and_leaves_the_two_that_are_not_rails() {
        // `Struct.new` and a `@return` tag are plain Ruby. Putting either under a `rails` table
        // would leak the first Rails word where it does not belong, and this shows it is more than
        // naming.
        let harness = every_family("[rails]\nenabled = false\n");
        for (family, source, declared) in FAMILIES {
            let rbs = harness.generated_rbs(source);
            if family == "structs" || family == "annotations" {
                assert!(rbs.contains(declared), "{declared} is not Rails: {rbs}");
            } else {
                assert!(!rbs.contains(declared), "{family}: {declared}");
            }
        }
    }

    #[test]
    fn a_schema_the_configuration_excludes_is_not_read() {
        // Not indexed and not read are the same thing. A project that narrowed `index.include` has
        // said what it wants looked at, and this is not the place to overrule it.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\nenabled = false\n\n[index]\ninclude = [\"app/**/*.rb\"]\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write("db/schema.rb", SCHEMA_RB);
        harness.index();

        assert!(harness.analysis.synthesized.is_empty());
        assert!(!harness.has("Story#title()"));
    }

    /// What one pass over the graph collects, including the two projections nothing reads yet.
    ///
    /// The registry is the point: four lists, three predicates, one loop, so a reader wanting a
    /// fifth list adds a row, not a walk. `modules` and `superclasses` are here because the
    /// concern, entry-point and routes readers need them at no extra loop cost; a projection added
    /// later would be a second walk over every document.
    #[test]
    fn what_one_pass_over_the_graph_collects() {
        let mut harness = Harness::new();
        harness.write("db/schema.rb", "ActiveRecord::Schema[8.0].define do\nend\n");
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        harness.write("app/models/concerns/storyish.rb", "module Storyish\nend\n");
        harness.write(
            "app/models/legacy.rb",
            "class Legacy < ActiveRecord::Base\n  self.table_name = \"old\"\nend\n",
        );
        // Both annotation shapes in one file, the case the single loop is for: a `sig` *and* a YARD
        // tag put it on the annotated list once; twice would generate it twice.
        harness.write(
            "app/lib/widget.rb",
            "class Widget\n  sig { returns(String) }\n  def go\n  end\n\n  \
             # @return [Integer]\n  def size\n  end\nend\n",
        );
        harness.index();

        let context = harness.analysis.context();
        let named = |list: ListId| -> Vec<String> {
            context
                .documents(list)
                .iter()
                .map(|uri| {
                    uri.rsplit('/')
                        .take(2)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .collect()
        };
        assert_eq!(named(rails_lists::SCHEMAS), ["db/schema.rb"]);
        // `legacy.rb` writes no macro and is on the list anyway: it defines a model, so its
        // relation class and class side belong there. The only membership decided after the walk.
        assert_eq!(
            named(rails_lists::MODELS),
            ["models/legacy.rb", "models/story.rb"]
        );
        assert_eq!(named(rails_lists::RENAMED), ["models/legacy.rb"]);
        assert_eq!(named(annotations_list::ANNOTATED), ["lib/widget.rb"]);

        // Every class *and* module, the bound on what a macro may name; and modules alone, since a
        // module is a different thing to declare on.
        assert!(
            ["Story", "Storyish", "Legacy", "Widget"]
                .iter()
                .all(|name| context.classes.contains(*name)),
            "{:?}",
            context.classes
        );
        assert_eq!(
            context
                .modules
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["Storyish"]
        );

        // As written, not qualified: the test applied is a suffix, and an undefined superclass is
        // still read (`ApplicationRecord` is the base, `ActiveRecord::Base` a gem's).
        assert_eq!(
            context.superclasses.get("Story").map(String::as_str),
            Some("ApplicationRecord")
        );
        assert_eq!(
            context.superclasses.get("Legacy").map(String::as_str),
            Some("ActiveRecord::Base")
        );
        assert_eq!(context.superclasses.get("Widget"), None);

        // A table claimed by exactly one top-level class, which the schema generator then filters
        // for ambiguity.
        assert_eq!(
            rails_of(&context).claims.get("stories").map(Vec::as_slice),
            Some(["Story".to_owned()].as_slice())
        );
    }

    /// A class written inside a `class << self` body is not one this pass can name.
    ///
    /// Its lexical nesting is the singleton class, whose name is `ParentScope::Attached` (not a
    /// constant anyone writes), so `qualified_name` answers `None` and the walk moves on. Every
    /// generator's rule: an undefined class declares nothing, and "cannot be spelled" must reach
    /// the same answer as "is not defined".
    #[test]
    fn a_class_inside_a_singleton_class_body_is_not_a_class_this_pass_names() {
        let mut harness = Harness::new();
        harness.write(
            "app/models/outer.rb",
            "class Outer\n  class << self\n    class Inner\n    end\n\n    module Deeper\n    \
             end\n  end\nend\n",
        );
        harness.index();

        let context = harness.analysis.context();
        assert!(context.classes.contains("Outer"), "{:?}", context.classes);
        assert!(
            !context
                .classes
                .iter()
                .any(|name| name.contains("Inner") || name.contains("Deeper")),
            "{:?}",
            context.classes
        );
        assert!(context.modules.is_empty(), "{:?}", context.modules);
    }

    /// A class Ruby accepts but Rails' inflector does not claims no table.
    ///
    /// `class Ünicode` is legal Ruby (a constant must start with an uppercase letter, in Unicode's
    /// sense), and `underscore` deliberately requires an *ASCII* capital: there is no acronym table
    /// and no way to guess the table's name. The failure direction is the schema's intended one: no
    /// claim, rather than a claim on a nonexistent table.
    #[test]
    fn a_class_whose_name_is_not_ascii_claims_no_table() {
        let mut harness = Harness::new();
        harness.write("app/models/unicode.rb", "class Ünicode\nend\n");
        harness.index();

        let context = harness.analysis.context();
        assert!(context.classes.contains("Ünicode"), "{:?}", context.classes);
        assert!(
            rails_of(&context).claims.is_empty(),
            "{:?}",
            rails_of(&context).claims
        );
    }

    /// A workspace that stops declaring anything still gets pruned.
    ///
    /// `synthesize`'s first line returns early only when there is nothing to read **and** nothing
    /// written last time. Deleting every model makes the first true and the second false: the one
    /// shape where an early return would leave a `Comment::Relation` in the graph forever, with no
    /// file left to edit to fix it.
    #[test]
    fn a_workspace_that_stops_declaring_anything_is_still_pruned() {
        let mut harness = Harness::new();
        let story = harness.write(
            "app/models/story.rb",
            "class Story\n  has_many :comments\nend\n",
        );
        let comment = harness.write("app/models/comment.rb", "class Comment\nend\n");
        harness.index();
        assert!(harness.has("Comment::Relation"), "nothing generated");

        std::fs::remove_file(story.to_file_path().unwrap()).unwrap();
        std::fs::remove_file(comment.to_file_path().unwrap()).unwrap();
        harness.watch(&[&story, &comment]);
        assert!(!harness.has("Comment::Relation"), "left behind");
        assert!(!harness.has("Story#comments()"), "left behind");
    }

    /// A file that vanishes between the walk and the read costs only its own declarations.
    ///
    /// `synthesize` runs before every resolve and reads the graph's document list, which a watcher
    /// event may not have updated yet. Nothing to wait for or recover: the file is skipped, the
    /// rest runs, and the next event prunes it.
    #[test]
    fn a_file_that_vanished_under_the_pass_is_skipped() {
        let mut harness = Harness::new();
        let widget = harness.write(
            "app/lib/widget.rb",
            "class Widget\n  # @return [String]\n  def go\n  end\nend\n",
        );
        harness.write(
            "app/lib/gadget.rb",
            "class Gadget\n  # @return [Integer]\n  def size\n  end\nend\n",
        );
        harness.index();
        assert!(harness.has("Widget#go()"));

        // Deleted on disk, *unannounced*, so the document is still in the graph but its text is
        // not.
        std::fs::remove_file(widget.to_file_path().unwrap()).unwrap();
        harness.analysis.synthesize();
        harness.analysis.resolve();
        assert!(harness.has("Gadget#size()"), "the pass stopped early");
    }

    #[test]
    fn a_second_pass_does_not_derive_from_what_the_first_one_delegated() {
        // The phase boundary, asserted *across settles*, the shape that could rot silently.
        // `synthesize` runs before every resolve, so if the union held phase two's own output,
        // answers would change on the second keystroke and keep changing: a `delegate` through a
        // `delegate` would type on run two, a three-link chain on run three. It cannot, because the
        // union comes from a fresh map built before anything is merged into it, and this proves it.
        //
        // `Story#profile` is a delegation that *does* type (through `belongs_to :user` and
        // `has_one :profile`). `Story#bio` delegates through it and must stay the two calls, not a
        // type phase two derived, however many passes run. Those calls answer where `bio` is
        // called, so the chain through it resolves either way; the generated text is the proof.
        let mut harness = Harness::new();
        // All write `< ApplicationRecord` because of the host test: a class inheriting nothing is
        // not an ActiveRecord model, and its association macros declare nothing.
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  belongs_to :user\n  \
             delegate :profile, to: :user\n  delegate :bio, to: :profile\nend\n",
        );
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\n  has_one :profile\nend\n",
        );
        harness.write(
            "app/models/profile.rb",
            "class Profile < ApplicationRecord\n  has_one :bio\nend\n",
        );
        harness.write(
            "app/models/bio.rb",
            "class Bio < ApplicationRecord\n  has_one :photo\nend\n",
        );
        harness.write(
            "app/models/photo.rb",
            "class Photo < ApplicationRecord\nend\n",
        );
        let source = "Story.new.profile.bio\nStory.new.bio.photo\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let typed = card(&mut harness, &uri, source, "bio\n");
        assert!(
            typed.contains("Profile#bio"),
            "the delegation that can type did not: {typed}"
        );
        let through = card(&mut harness, &uri, source, "photo");
        assert!(
            through.contains("Bio#photo"),
            "the calls a delegate hands on were not made: {through}"
        );
        let forwarded = "def bio: (*untyped) -> ForwardedToItsTarget[\"profile\", \"bio\"]";
        let rbs = harness.generated_rbs("app/models/story.rb");
        assert!(
            rbs.contains(forwarded),
            "phase two derived from itself: {rbs}"
        );

        let documents = harness.analysis.synthesized.len();
        harness.analysis.synthesize();
        harness.analysis.resolve();
        assert_eq!(harness.analysis.synthesized.len(), documents);
        assert_eq!(harness.generated_rbs("app/models/story.rb"), rbs);
        assert_eq!(card(&mut harness, &uri, source, "bio\n"), typed);
        assert_eq!(card(&mut harness, &uri, source, "photo"), through);
    }

    #[test]
    fn a_caption_names_the_gem_a_file_came_from_or_falls_back_to_the_file() {
        // An engine's `config/routes.rb` can declare helpers, so root-relative captions would label
        // its card "From `routes.rb`", the same as the project's own routes file. The marker is a
        // directory named `gems`, which every layout `gems::gem_roots` knows ends in.
        let gem = |path: &str| {
            synthesize::gem_relative(Path::new(path)).map(|it| it.to_string_lossy().into_owned())
        };
        assert_eq!(
            gem("/home/me/.gem/ruby/4.0.0/gems/shouty-1.2.3/config/routes.rb"),
            Some("shouty-1.2.3/config/routes.rb".to_owned())
        );
        // A git source unpacks under `bundler/gems`, and the same marker finds it.
        assert_eq!(
            gem("/w/vendor/bundle/ruby/4.0.0/bundler/gems/shouty-abc123/app/models/m.rb"),
            Some("shouty-abc123/app/models/m.rb".to_owned())
        );
        // A path with no such ancestor gets nothing, so the caller keeps its own fallback instead
        // of this guessing a name.
        assert_eq!(gem("/somewhere/else/config/routes.rb"), None);
    }

    /// A minimal bundle with the right shape: `ActiveRecord::Relation` reached through an
    /// `include`, as `relation.rb` writes it.
    fn with_a_bundle() -> (Harness, crate::workspace::DocUri) {
        bundle_in(Harness::new())
    }

    /// The same bundle, written into a harness of the caller's.
    fn bundle_in(harness: Harness) -> (Harness, crate::workspace::DocUri) {
        harness.write(
            "lib/active_record/relation/query_methods.rb",
            "module ActiveRecord\n  module QueryMethods\n    # Filters the rows.\n    def where(*args)\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/relation.rb",
            "module ActiveRecord\n  class Relation\n    include QueryMethods\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/persistence.rb",
            "module ActiveRecord\n  module Persistence\n    module ClassMethods\n      def create(*args)\n      end\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/base.rb",
            "module ActiveRecord\n  class Base\n  end\nend\n",
        );
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        (harness, story)
    }

    /// The row [`Owners`] exists for: a member ya-lsp declared, answering with the `def` Rails
    /// really wrote.
    #[test]
    fn a_query_method_ya_lsp_wrote_answers_with_rails_own_def() {
        let (mut harness, _) = with_a_bundle();
        let source = "Story.where(id: 1)\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let card = harness.hover_at(&uri, source, "where(id");
        let card = card["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            card.contains("ActiveRecord::Base.where"),
            "the card is still ya-lsp's own declaration: {card}"
        );
        let answer = harness.definition_at(&uri, source, "where(id");
        let places = answer.as_array().map(Vec::len).unwrap_or_default();
        assert_eq!(places, 1, "one place, and it is Rails': {answer}");
        assert!(
            answer[0]["targetUri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("lib/active_record/relation/query_methods.rb"),
            "{answer}"
        );
    }

    /// The class side takes its own `def` where Rails writes one, instead of the relation's
    /// fall-through.
    #[test]
    fn the_class_side_prefers_the_def_rails_writes_for_a_class_object() {
        let (mut harness, _) = with_a_bundle();
        let source = "Story.create(title: 1)\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let answer = harness.definition_at(&uri, source, "create(title");
        assert!(
            answer[0]["targetUri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("lib/active_record/persistence.rb"),
            "{answer}"
        );
    }

    /// An owner is the class the list names, never a class *inside* it.
    ///
    /// `ActiveRecord::Relation` holds `ExplainProxy` (`relation.rb`'s first lines, with its own
    /// `count`), `Merger#merge` and `WhereClause#+`. A lookup that matched names containing the
    /// owner's sent every corpus's `count` to `explain.count`, and `merge` to the merger: the
    /// wrong `def` from the right gem. `+` is delegated to the records and has no `def` at all.
    #[test]
    fn a_class_nested_in_an_owner_places_nothing() {
        let (mut harness, _) = with_a_bundle();
        harness.write(
            "lib/active_record/relation/calculations.rb",
            "module ActiveRecord\n  module Calculations\n    def count(column_name = nil)\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/relation/spawn_methods.rb",
            "module ActiveRecord\n  module SpawnMethods\n    def merge(other, *rest)\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/relation/nested.rb",
            "module ActiveRecord\n  class Relation\n    include Calculations\n    include SpawnMethods\n\n    \
             class ExplainProxy\n      def count(column_name = nil)\n      end\n    end\n\n    \
             class Merger\n      def merge\n      end\n    end\n\n    \
             class WhereClause\n      def +(other)\n      end\n    end\n  end\nend\n",
        );
        // `.+` spelled as a call: an operator's receiver is not typed, so `a + b` is answered by the
        // name guess, which says so, and never reaches this lookup.
        let source = "Story.count\nStory.where(id: 1).count\nStory.where(id: 2).merge(nil)\n\
                      Story.where(id: 3).+([])\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let mut file = |needle: &str| {
            let answer = harness.definition_at(&uri, source, needle);
            answer[0]["targetUri"]
                .as_str()
                .map(|target| target.rsplit('/').next().unwrap_or_default().to_owned())
        };
        assert_eq!(
            file("count\nStory.where"),
            Some("calculations.rb".to_owned())
        );
        assert_eq!(
            file("count\nStory.where(id: 2)"),
            Some("calculations.rb".to_owned())
        );
        assert_eq!(file("merge(nil)"), Some("spawn_methods.rb".to_owned()));
        assert_eq!(file("+([])"), None);
    }

    /// `all` is on both sides in Rails 8, in two files: `Story.all` is `Scoping::Named`'s and
    /// `relation.all` is `QueryMethods`'. `unscoped` has a class-side `def` and only a `delegate`
    /// on the relation, which leaves nothing to point at.
    #[test]
    fn all_and_unscoped_take_the_def_ruby_calls_on_each_side() {
        let (mut harness, _) = with_a_bundle();
        harness.write(
            "lib/active_record/relation/query_methods_all.rb",
            "module ActiveRecord\n  module QueryMethods\n    def all\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/scoping/named.rb",
            "module ActiveRecord\n  module Scoping\n    module Named\n      module ClassMethods\n        def all(all_queries: nil)\n        end\n      end\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/scoping/default.rb",
            "module ActiveRecord\n  module Scoping\n    module Default\n      module ClassMethods\n        def unscoped(&block)\n        end\n      end\n    end\n  end\nend\n",
        );
        let source =
            "Story.all\nStory.where(id: 1).all\nStory.unscoped\nStory.where(id: 2).unscoped.to_a\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let mut file = |needle: &str| {
            let answer = harness.definition_at(&uri, source, needle);
            answer[0]["targetUri"]
                .as_str()
                .map(|target| target.rsplit('/').next().unwrap_or_default().to_owned())
        };
        assert_eq!(file("all\n"), Some("named.rb".to_owned()));
        assert_eq!(
            file("all\nStory.unscoped"),
            Some("query_methods_all.rb".to_owned())
        );
        assert_eq!(file("unscoped\n"), Some("default.rb".to_owned()));
        assert_eq!(file("unscoped.to_a"), None);
    }

    /// A module's `thread_mattr_accessor` hands back what its writer is given: every
    /// `Current.account = …` and `self.account ||= …` the application writes, joined, and `nil`
    ///.
    ///
    /// A spec's write is not the application's, and another module's writer of the same name is
    /// not this one's. A value nothing types refuses the accessor, and so does a module a class
    /// `include`s, whose instances hold a writer too, or one with a `default:` or a block. A plain
    /// `mattr_accessor` keeps a class variable, and answers until something else can fill it: an
    /// `@@spelled =` in any reopening, or a `class_variable_set` that can name it. A write of the
    /// accessor to itself is a loop, and refuses; so does `+=`, whose value is the operator's, and
    /// a value only a name guesses. A template's write counts.
    #[test]
    fn a_module_accessor_is_what_its_writer_is_given() {
        let draw = |extra: &[(&str, &str)]| {
            let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
            harness.write(
                "lib/current.rb",
                "module Current\n  thread_mattr_accessor :account\n  thread_mattr_accessor :user\n  \
                 thread_mattr_accessor :inbox\n  thread_mattr_accessor :label\n  \
                 thread_mattr_accessor :guessy\n  thread_mattr_accessor :nobody\n  \
                 thread_mattr_accessor :counter\n  thread_mattr_accessor :defaulted, default: 1\n  \
                 thread_mattr_accessor(:blocked) { 1 }\n  mattr_accessor :plain\n  \
                 mattr_accessor :spelled\n  cattr_accessor :reflected\n  \
                 mattr_accessor :elsewhere\n\n  \
                 def self.reset\n    self.account = nil\n  end\nend\n\n\
                 module Other\n  thread_mattr_accessor :account\nend\n",
            );
            harness.write(
                "app/models/account.rb",
                "class Account\n  def owner_name\n    \"x\"\n  end\nend\n",
            );
            harness.write("app/models/admin.rb", "class Admin < Account\nend\n");
            harness.write(
                "app/controllers/accounts_controller.rb",
                "class AccountsController\n  def show\n    Current.account = Account.new\n    \
                 Current.account ||= Admin.new\n    Current.account &&= Admin.new\n    \
                 Current.user = params[:user]\n    Current.user = Admin.new\n    \
                 Current.plain = Account.new\n    Current.spelled = Account.new\n    \
                 Current.reflected = Account.new\n    Current.elsewhere = Account.new\n    \
                 Other.account = \"not Current's\"\n    \
                 Current.inbox = Current.inbox\n    Current.label = \"x\"\n    \
                 ::Current.label = \"y\"\n    Current.guessy = mystery.owner_name\n    \
                 Current.counter += 1\n    Current.defaulted = Account.new\n    \
                 Current.blocked = Account.new\n    Account.new.account = 1\n    \
                 Current[1] = 2\n  end\nend\n",
            );
            harness.write(
                "app/views/accounts/show.html.erb",
                "<% Current.label = 1 %>\n",
            );
            harness.write("spec/current_spec.rb", "Current.account = \"a double\"\n");
            // A plain accessor's class variable, filled without its writer: spelled in another
            // reopening, and set by name. A name that is another variable's refuses nothing.
            harness.write(
                "lib/current/spelled.rb",
                "module Current\n  def self.fill\n    @@spelled = 1\n  end\nend\n",
            );
            harness.write(
                "app/models/reflecting.rb",
                "class Reflecting\n  def fill\n    Current.class_variable_set(:@@reflected, 1)\n    \
                 Current.class_variable_set(\"@@unrelated\", 1)\n  end\nend\n",
            );
            for (path, text) in extra {
                harness.write(path, text);
            }
            let source = "account = Current.account\nagain = Current.account\nuser = Current.user\n\
                          inbox = Current.inbox\nlabel = Current.label\nguessy = Current.guessy\n\
                          nobody = Current.nobody\ncounter = Current.counter\n\
                          defaulted = Current.defaulted\nblocked = Current.blocked\n\
                          plain = Current.plain\nspelled = Current.spelled\n\
                          reflected = Current.reflected\nelsewhere = Current.elsewhere\n";
            let uri = harness.write("app/main.rb", source);
            harness.index();
            drawn_hints(source, &harness.hints_in(&uri))
        };
        assert_eq!(
            draw(&[]),
            "\
account: Account? = Current.account
again: Account? = Current.account
label: String? | Integer = Current.label
plain: Account? = Current.plain
elsewhere: Account? = Current.elsewhere"
        );
        assert_eq!(
            draw(&[(
                "app/models/story.rb",
                "class Story\n  include Current\nend\n"
            )]),
            "null"
        );
    }

    /// A `delegate` this pass could not type is the two calls Rails writes, made where the member
    /// is called: the target asked of the receiver (a private `def` too), then the name asked of
    /// that. A constant target is spelled from the class, `to: :class` is its class object, and
    /// `allow_nil:` adds `nil`. A private second hop raises in Ruby, and a target nothing types
    /// answers nothing. One only a name guesses (`@order`, which nothing writes) makes a guess,
    /// which the margin never draws.
    #[test]
    fn a_delegate_is_the_calls_it_hands_on() {
        // `to: :class` is `self.class`, which core RBS declares on `Kernel`.
        let mut harness = signed(
            &[
                ("core/core.rbs", TYPED_RBS),
                (
                    "core/kernel.rbs",
                    "module Kernel\n  def class: () -> Class\nend\n",
                ),
            ],
            "",
        );
        harness.write(
            "app/models/order.rb",
            "class Order\n  def total\n    1\n  end\n\n  private\n\n  def secret\n    1\n  end\nend\n",
        );
        harness.write(
            "app/services/settings.rb",
            "module Settings\n  def self.host\n    \"example.com\"\n  end\nend\n",
        );
        harness.write(
            "app/services/updater.rb",
            "class Updater\n  delegate :total, to: :order\n  \
             delegate :total, to: :order, prefix: :maybe, allow_nil: true\n  \
             delegate :total, to: :hidden, prefix: true\n  delegate :secret, to: :order\n  \
             delegate :host, to: Settings\n  delegate :label, to: :class\n  \
             delegate :total, to: :given, prefix: true\n  delegate :total, to: :guessy, prefix: true\n\n  \
             attr_reader :given\n\n  def initialize(given)\n    @given = given\n  end\n\n  \
             def guessy\n    @order\n  end\n\n  \
             def self.label\n    \"updater\"\n  end\n\n  def order\n    Order.new\n  end\n\n  \
             private\n\n  def hidden\n    Order.new\n  end\nend\n",
        );
        let source = "updater = Updater.new(1)\ntotal = updater.total\nmaybe = updater.maybe_total\n\
                      hidden = updater.hidden_total\nsecret = updater.secret\nhost = updater.host\n\
                      label = updater.label\ngiven = updater.given_total\n\
                      guessy = updater.guessy_total\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
total: Integer = updater.total
maybe: Integer? = updater.maybe_total
hidden: Integer = updater.hidden_total
host: String = updater.host
label: String = updater.label"
        );
    }

    /// A `delegate` whose target is the receiver again asks itself: refused where it comes back,
    /// directly or through another class's `delegate`, instead of recursing until the stack
    /// overflows. a `JsonApiKit::Reflection::Through` did this through a name guess.
    #[test]
    fn a_delegate_that_comes_back_to_itself_answers_nothing() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "app/services/looping.rb",
            "class Looping\n  delegate :value, to: :me\n  delegate :other, to: :partner\n\n  \
             def me\n    self\n  end\n\n  def partner\n    Partner.new\n  end\nend\n\n\
             class Partner\n  delegate :other, to: :back\n\n  def back\n    Looping.new\n  end\nend\n",
        );
        let source = "value = Looping.new.value\nother = Looping.new.other\nme = Looping.new.me\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "me: Looping = Looping.new.me"
        );
    }

    /// A concern's class method answers from its own `def`, read as the including class's method:
    /// `self` there is the class object, so a receiverless call reaches the class's own class
    /// methods and the concern's other ones. Both spellings, `class_methods do` and a hand-written
    /// `module ClassMethods`.
    ///
    /// An instance variable read in the block's `def`s refuses, as in any block straight in a
    /// module body: the class object's `@memo` is not the one `fill` writes on an instance.
    #[test]
    fn a_concerns_class_method_answers_from_its_own_def() {
        let draw = |finder: &str| {
            let (mut harness, _, _) = models_project("");
            harness.write("app/models/concerns/finder.rb", finder);
            harness.write(
                "app/models/concerns/countable.rb",
                "module Countable\n  extend ActiveSupport::Concern\n\n  module ClassMethods\n    \
                 def counted\n      [1]\n    end\n  end\nend\n",
            );
            harness.write(
                "app/models/account.rb",
                "class Account < ApplicationRecord\n  include Finder\n  include Countable\n\n  \
                 def self.own_label\n    \"x\"\n  end\n\n  def fill\n    @memo = 1\n  end\nend\n",
            );
            let source = "local = Account.find_local(\"a\")\nbang = Account.find_local!(\"a\")\n\
                          labelled = Account.labelled\ncounted = Account.counted\n\
                          memo = Account.memo\n";
            let uri = harness.write("app/main.rb", source);
            harness.index();
            drawn_hints(source, &harness.hints_in(&uri))
        };
        assert_eq!(
            draw(
                "module Finder\n  extend ActiveSupport::Concern\n\n  class_methods do\n    \
                 def find_local(name)\n      find_remote(name, nil)\n    end\n\n    \
                 def find_local!(name)\n      find_local(name) || raise(ArgumentError)\n    end\n\n    \
                 def find_remote(name, domain)\n      42\n    end\n\n    \
                 def labelled\n      own_label\n    end\n\n    \
                 def memo\n      @memo ||= \"cached\"\n    end\n  end\nend\n",
            ),
            "\
local: Integer = Account.find_local(\"a\")
bang: Integer = Account.find_local!(\"a\")
labelled: String = Account.labelled
counted: Array[Integer] = Account.counted"
        );
    }

    /// `ActiveRecord::Inheritance::ClassMethods`' shape: an `attr_accessor` in a concern's
    /// `module ClassMethods` is a class method of every includer, so `self.abstract_class = true`
    /// has a card and a place, the accessor's line. Before, only a `def` there was read.
    #[test]
    fn an_attr_accessor_in_a_concerns_class_methods_is_a_class_method_of_every_includer() {
        let (mut harness, _, _) = models_project("");
        harness.write(
            "app/models/concerns/abstractable.rb",
            "module Abstractable\n  extend ActiveSupport::Concern\n\n  module ClassMethods\n    \
             attr_accessor :abstract_class\n  end\nend\n",
        );
        let source = "class Shelf\n  include Abstractable\n  self.abstract_class = true\nend\n";
        let uri = harness.write("app/models/shelf.rb", source);
        harness.index();

        let found = harness.definition_at(&uri, source, "abstract_class =");
        assert_eq!(linked(&found), ["abstractable.rb:4:19"], "{found}");
        let card = harness.hover_at(&uri, source, "abstract_class =")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(card, "```ruby\nShelf.abstract_class=(value)\n```");
    }

    /// `Post.prepend(Shouting)` in a boot block (an application's `Paperclip::Attachment.prepend`, a
    /// plugin's `after_initialize do … Post.include`) puts the module in `Post`'s ancestors: its
    /// method is `Post`'s own, not a name match, and its `@title` is the one `Post` writes. A
    /// spec's `Post.include(Helping)` does not: the application never runs it.
    #[test]
    fn a_module_mixed_in_from_outside_the_class_is_one_of_its_ancestors() {
        let mut harness = Harness::new();
        harness.write(
            "app/models/post.rb",
            "class Post\n  def title\n    @title = \"x\"\n  end\nend\n",
        );
        let shouting = "module Shouting\n  def shout\n    @title.upcase\n  end\nend\n";
        let module = harness.write("lib/shouting.rb", shouting);
        let init = harness.write(
            "config/initializers/shouting.rb",
            "Rails.application.config.to_prepare do\n  Post.prepend(Shouting)\nend\n",
        );
        let helping = harness.write(
            "spec/support/helping.rb",
            "module Helping\n  def helped\n  end\nend\n\nPost.include(Helping)\n",
        );
        let source = "Post.new.shout\nPost.new.helped\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let card = |harness: &mut Harness, uri: &DocUri, source: &str, at: &str| {
            harness.hover_at(uri, source, at)["contents"]["value"]
                .as_str()
                .unwrap_or_default()
                .replace('\n', " / ")
        };
        assert_eq!(
            card(&mut harness, &uri, source, "shout"),
            "```ruby / Shouting#shout / ```"
        );
        assert_eq!(
            card(&mut harness, &module, shouting, "title.upcase"),
            "```ruby / Post#@title / ```"
        );
        assert_eq!(
            harness.generated_for(&init).as_deref(),
            Some("class Post\n  prepend ::Shouting\nend\n")
        );
        // Nothing is written for the spec's call. Its member would be fenced from the application
        // anyway (`types::member_of`), but the ancestor would not: `resolve` walks it unfenced.
        assert_eq!(harness.generated_for(&helping), None);
    }

    /// The same call typed into an open buffer, then deleted: the indexer's note of what the text
    /// spells follows every version the graph takes, so the list, and the ancestor, follow the
    /// edit both ways.
    #[test]
    fn a_mixin_typed_into_a_buffer_joins_and_leaves_with_the_edit() {
        let mut harness = Harness::new();
        harness.write("app/models/post.rb", "class Post\nend\n");
        harness.write(
            "lib/shouting.rb",
            "module Shouting\n  def shout\n  end\nend\n",
        );
        let init = harness.write("config/initializers/shouting.rb", "# nothing yet\n");
        let source = "Post.new.shout\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        let guessed = |harness: &mut Harness| {
            harness.hover_at(&uri, source, "shout")["contents"]["value"]
                .as_str()
                .unwrap_or_default()
                .contains("Guessed from name alone")
        };
        assert!(guessed(&mut harness));

        harness.open(&init, "# nothing yet\n");
        harness.change(&init, "Post.include(Shouting)\n");
        assert!(
            !guessed(&mut harness),
            "the call joined the class's ancestors"
        );
        harness.change(&init, "# nothing yet\n");
        assert!(guessed(&mut harness), "and left them when it was deleted");
    }

    /// A column's writer answers as its reader does: `self.title = x` in the model jumps to the
    /// schema's line and has a card saying Rails defines it. Before, the reader beside it answered
    /// and the assignment said nothing.
    #[test]
    fn a_column_s_writer_has_the_reader_s_place_and_a_card() {
        let source = "class Story\n  def rename\n    self.title = \"x\"\n  end\nend\n";
        let (mut harness, schema, _) = rails_project("");
        let model = harness.write("app/models/story.rb", source);
        harness.watch(&[&model]);

        let found = harness.definition_at(&model, source, "title =");
        assert_eq!(
            found[0]["targetUri"],
            serde_json::json!(schema.as_str()),
            "{found}"
        );
        let card = harness.hover_at(&model, source, "title =")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(card, "```ruby\nStory#title=(value)\n```");
    }

    /// A bare `where` is ActiveRecord's `WhereChain`, holding the relation it was made from, so
    /// `not`, `missing` and `associated` hand that relation back and the chain goes on. A bundle
    /// without the class keeps the bare arm unreadable, and the chain answers nothing.
    #[test]
    fn a_bare_where_is_a_where_chain_that_hands_back_its_relation() {
        let source = "\
chain = Story.where
kept = Story.where.not(id: 1)
missing = Story.where(a: 1).where.missing(:author)
first = Story.all.where.associated(:author).first
";
        let drawn = |chain: bool| {
            let (mut harness, _) = with_a_bundle();
            if chain {
                harness.write(
                    "lib/active_record/relation/where_chain.rb",
                    "module ActiveRecord\n  module QueryMethods\n    class WhereChain\n      \
                     def not(opts, *rest)\n        @scope\n      end\n\n      \
                     def missing(*associations)\n        @scope\n      end\n\n      \
                     def associated(*associations)\n        @scope\n      end\n    end\n  \
                     end\nend\n",
                );
            }
            let uri = harness.write("app/main.rb", source);
            harness.index();
            drawn_hints(source, &harness.hints_in(&uri))
        };
        assert_eq!(
            drawn(true),
            "\
chain: ActiveRecord::QueryMethods::WhereChain[Story::Relation] = Story.where
kept: Story::Relation = Story.where.not(id: 1)
missing: Story::Relation = Story.where(a: 1).where.missing(:author)
first: Story? = Story.all.where.associated(:author).first"
        );
        // No hints at all: nothing in the chain has a type.
        assert_eq!(drawn(false), "null");
    }

    /// With no bundle there is nothing to find: the same answer as without this feature, not a
    /// worse one.
    #[test]
    fn a_query_method_with_no_bundle_indexed_is_still_no_place_and_no_guess() {
        let mut harness = Harness::new();
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        let source = "Story.where(id: 1)\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        assert!(
            harness
                .definition_at(&uri, source, "where(id")
                .as_array()
                .is_none_or(Vec::is_empty),
            "no mapping means no place, and a name match is not a mapping"
        );
    }

    /// A name `Kernel` also declares.
    #[test]
    fn a_query_method_ruby_s_own_root_also_declares_is_not_answered_with_the_root() {
        let (mut harness, _) = with_a_bundle();
        // `select` is the shape: `Kernel#select` is `IO.select`, an ancestor of everything, and
        // spelled without parentheses, so the walk finds it one spelling before
        // `ActiveRecord::QueryMethods#select`.
        harness.write(
            "lib/core_ext.rb",
            "class Object\n  def select\n  end\nend\n",
        );
        let source = "Story.select(:id)\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let answer = harness.definition_at(&uri, source, "select(:id");
        assert!(
            answer.as_array().is_none_or(Vec::is_empty),
            "a `def` on `Object` is a member of every receiver there is, so it is evidence \
             about nothing: {answer}"
        );

        // The other half: rejecting the root must not take the real answer with it. Where Rails
        // writes the name, the reader gets that, with the root's `def` in the same walk.
        harness.write(
            "lib/active_record/relation/query_methods_select.rb",
            "module ActiveRecord\n  module QueryMethods\n    def select(*fields)\n    end\n  end\nend\n",
        );
        harness.index();
        let answer = harness.definition_at(&uri, source, "select(:id");
        assert!(
            answer[0]["targetUri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("query_methods_select.rb"),
            "{answer}"
        );
    }

    /// One `def` claimed once, however many generated declarations borrow it.
    #[test]
    fn one_def_borrowed_by_every_relation_class_is_still_one_place() {
        let (mut harness, _) = with_a_bundle();
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        harness.write("app/models/tag.rb", "class Tag < ApplicationRecord\nend\n");
        // An untyped receiver, so the answer is the name rung's list, which holds the query
        // interface once per relation class and once per base.
        let source = "def run(thing)\n  thing.where(id: 1)\nend\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let answer = harness.definition_at(&uri, source, "where(id");
        let targets: Vec<String> = answer
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| row["targetUri"].as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let mut once = targets.clone();
        once.sort();
        once.dedup();
        assert_eq!(
            targets.len(),
            once.len(),
            "`Defined in N places` may not count one `def` more than once: {targets:?}"
        );
    }

    /// One `def` that **two generators** both name is still one place.
    ///
    /// The test above is several declarations borrowing one place, deduplicated where they meet.
    /// This is one declaration with two definitions from two generators, one step further in:
    /// `hover` counts with [`locator::places`], which takes a single declaration, so the count
    /// would be 2 before the list reached anywhere they could be compared. `find_by` on a model is
    /// the real case: a row on Rails' query interface *and* a `def` in a hand-written
    /// `module ClassMethods` the base includes.
    #[test]
    fn one_def_two_generators_both_name_is_still_one_place() {
        let (mut harness, _) = with_a_bundle();
        harness.write(
            "lib/active_record/core.rb",
            "module ActiveRecord\n  module Core\n    module ClassMethods\n      def find_by(*args)\n      end\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/base.rb",
            "module ActiveRecord\n  class Base\n    include Core\n  end\nend\n",
        );
        let source = "Story.find_by(id: 1)\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let answer = harness.definition_at(&uri, source, "find_by(id");
        assert_eq!(
            answer.as_array().map(Vec::len),
            Some(1),
            "one `def`, however many generators named it: {answer}"
        );
        assert!(
            answer[0]["targetUri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("lib/active_record/core.rb"),
            "{answer}"
        );

        // The card counts with the same list, so it must not say 2 either.
        let card = harness.hover_at(&uri, source, "find_by(id");
        let card = card["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !card.contains("Defined in"),
            "the count is the list, and the list is one: {card}"
        );
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod framework_tests {
    use crate::analysis::testing::*;

    /// A Rails application with the four framework-table constants declared as a bundle declares
    /// them: `module Rails`, and classes for the rest.
    const BUNDLE: &str = "\
module Rails
  def self.root; end
  def self.cache; end
  def self.application; end
  class Application
    def routes; end
  end
end
class Pathname
  def join(*args); end
end
module ActiveSupport
  class TimeZone
    def now; end
  end
  module Cache
    class Store
      def fetch(name); end
    end
  end
end
class Time
  def self.zone; end
end
";

    const APPLICATION_RB: &str = "\
module Shop
  class Application < Rails::Application
    def domain; end
  end
end
";

    fn project(caller: &str) -> (Harness, crate::workspace::DocUri) {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write("config/application.rb", APPLICATION_RB);
        let uri = harness.write("app/use.rb", caller);
        harness.index();
        (harness, uri)
    }

    /// The four chains the table is for, each resolved to the class it names.
    #[test]
    fn a_framework_singletons_return_types_the_chain_written_on_it() {
        let source = "\
Rails.root.join(\"config\")\nRails.cache.fetch(\"k\")\nTime.zone.now\n";
        let (mut harness, uri) = project(source);
        for (needle, expected) in [
            ("join", "Pathname#join"),
            ("fetch", "ActiveSupport::Cache::Store#fetch"),
            ("now", "ActiveSupport::TimeZone#now"),
        ] {
            let card = card(&mut harness, &uri, source, needle);
            assert!(card.contains(expected), "{needle}: {card}");
        }
    }

    /// `Rails.application` is the project's own class, so what the project hung off it answers.
    #[test]
    fn rails_application_is_the_projects_own_application_class() {
        let source = "Rails.application.domain\n";
        let (mut harness, uri) = project(source);
        let card = card(&mut harness, &uri, source, "domain");
        assert!(card.contains("Shop::Application#domain"), "{card}");
    }

    /// It still reaches what `Rails::Application` itself declares.
    #[test]
    fn the_frameworks_own_members_are_reached_through_the_projects_class() {
        let source = "Rails.application.routes\n";
        let (mut harness, uri) = project(source);
        let card = card(&mut harness, &uri, source, "routes");
        assert!(card.contains("Rails::Application#routes"), "{card}");
    }

    /// actionpack as far as the controller table reads it: the four modules and the class a row is
    /// on, `Base` including them, and the classes the rows return, each with one member to reach.
    const ACTIONPACK: &str = "\
module ActionController
  module StrongParameters
    def params; end
  end
  module Cookies
    private
    def cookies; end
  end
  module Flash
  end
  class Metal
  end
  class Base < Metal
    include StrongParameters
    include Cookies
    include Flash
  end
  class Parameters
    def permit(*filters); end
  end
end
module ActionDispatch
  class Request
    def original_url; end
  end
  class Response
  end
  module Flash
    class FlashHash
      def notice; end
    end
  end
  module Cookies
    class CookieJar
      def signed; end
    end
  end
end
";

    /// A project with no `config/application.rb`, an engine or a gem monorepo, still gets every
    /// row its bundle backs: each is hosted on the file declaring its owner, not on the
    /// application.
    #[test]
    fn the_tables_need_no_application_only_the_components() {
        let source = "\
class PostsController < ActionController::Base
  def create
    params.permit(:title)
    request.original_url
    session.id
    flash.notice
    cookies.signed
    Rails.root.join(\"config\")
  end
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write("lib/actionpack.rb", ACTIONPACK);
        harness.write(
            "lib/session.rb",
            "module ActionDispatch\n  class Request\n    class Session\n      def id; end\n    end\n  end\nend\n",
        );
        let uri = harness.write("app/controllers/posts_controller.rb", source);
        harness.index();
        for (needle, expected) in [
            ("permit", "ActionController::Parameters#permit"),
            ("original_url", "ActionDispatch::Request#original_url"),
            ("id\n", "ActionDispatch::Request::Session#id"),
            ("notice", "ActionDispatch::Flash::FlashHash#notice"),
            ("signed", "ActionDispatch::Cookies::CookieJar#signed"),
            ("join", "Pathname#join"),
        ] {
            let card = card(&mut harness, &uri, source, needle);
            // Each method is the only one of its name here, so a name match would find it too:
            // the answer must be the receiver's, not a guess.
            assert!(card.contains(expected), "{needle}: {card}");
            assert!(
                !card.contains("Guessed from name alone"),
                "{needle}: {card}"
            );
        }
    }

    /// An API controller's `params` is `StrongParameters`', as a `Base` controller's is, though
    /// `ActionController::API` includes its modules in a loop over `MODULES` and `Metal` writes a
    /// `params` of its own behind them.
    #[test]
    fn an_api_controller_s_params_is_strong_parameters_like_a_base_controller_s() {
        let source = "\
class PostsController < ActionController::API
  def create
    params.permit(:title)
  end
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write("lib/actionpack.rb", ACTIONPACK);
        harness.write(
            "lib/api.rb",
            "module AbstractController\n  module Rendering\n  end\n  module Callbacks\n  end\nend\n\
             module ActionController\n  \
             module UrlFor\n  end\n  module Redirecting\n  end\n  module ApiRendering\n  end\n  \
             module Renderers\n    module All\n    end\n  end\n  module ConditionalGet\n  end\n  \
             module BasicImplicitRender\n  end\n  module RateLimiting\n  end\n  \
             module Caching\n  end\n  module DataStreaming\n  end\n  module DefaultHeaders\n  end\n  \
             module Logging\n  end\n  module Rescue\n  end\n  module Instrumentation\n  end\n  \
             module ParamsWrapper\n  end\n\n  \
             class Metal\n    def params; end\n  end\n\n  \
             class API < Metal\n    MODULES = [StrongParameters, Rescue]\n\n    \
             MODULES.each do |mod|\n      include mod\n    end\n  end\nend\n",
        );
        let uri = harness.write("app/controllers/posts_controller.rb", source);
        harness.index();
        let card = card(&mut harness, &uri, source, "permit");
        assert!(
            card.contains("ActionController::Parameters#permit"),
            "{card}"
        );
        assert!(!card.contains("Guessed from name alone"), "{card}");
    }

    /// A key read straight off a controller's `params` is what a parsed request can carry, `nil`
    /// where it may be absent, joined with what the project writes there and what a route gives
    /// it. `params[:locale] = :en` adds `Symbol` to `locale`, and `defaults: { format: :json }` to
    /// `format`. `fetch` with an empty hash for a default reads as `fetch`: the default comes back
    /// an empty `Parameters`, which the union holds. Any other default, a `Parameters` the code
    /// built, or a local holding `params`, answers as before.
    #[test]
    fn a_key_read_off_the_request_is_what_a_request_can_carry() {
        let source = "\
class PostsController < ActionController::Base
  def show
    id = params[:id]
    post = params.require(:post)
    page = params.fetch(:page)
    emptied = params.fetch(:page, {})
    routed = params.fetch(:id, {})
    defaulted = params.fetch(:page, 1)
    deep = params.dig(:a, :b)
    format = params[:format]
    locale = params[:locale]
    upcased = params[:name].upcase
    other = ActionController::Parameters.new[:id]
    held = params
    through = held[:id]
    nil
  end

  def set_locale
    params[:locale] = :en
  end
end
";
        let drawn = |routes: &str, extra: &str| {
            let (mut harness, _schema, _uri) = rails_project("");
            harness.write("lib/bundle.rb", BUNDLE);
            harness.write(
                "lib/actionpack.rb",
                &format!(
                    "{ACTIONPACK}module ActionDispatch\n  module Http\n    class UploadedFile\n    \
                     end\n  end\nend\nclass Symbol\nend\n{extra}"
                ),
            );
            harness.write("config/routes.rb", routes);
            let uri = harness.write("app/controllers/posts_controller.rb", source);
            harness.index();
            drawn_hints(source, &harness.hints_in(&uri))
        };
        // `true` and `false` are put back after the other classes (`Sides`), so `bool` comes last.
        let union = "Integer | Float | Array | ActionController::Parameters | \
                     ActionDispatch::Http::UploadedFile";
        let routes = "Rails.application.routes.draw do\n  resources :posts, defaults: { format: :json }\nend\n";
        assert_eq!(
            drawn(routes, ""),
            format!(
                "  def show -> nil
    id: String = params[:id]
    post: String | {union} | bool = params.require(:post)
    page: String? | {union} | bool = params.fetch(:page)
    emptied: String? | {union} | bool = params.fetch(:page, {{}})
    routed: String = params.fetch(:id, {{}})
    deep: String? | {union} | bool = params.dig(:a, :b)
    format: Symbol | String = params[:format]
    locale: String? | {union} | Symbol | bool = params[:locale]
    upcased: String = params[:name].upcase
    held: ActionController::Parameters = params
  def set_locale -> Symbol"
            )
        );
        // A parser of the project's own can hand back anything, so every key is left to Rails.
        let parsed = drawn(
            routes,
            "ActionDispatch::Request.parameter_parsers[:xml] = ->(raw) { raw }\n",
        );
        assert!(!parsed.contains("id:"), "{parsed}");
        assert!(
            parsed.contains("held: ActionController::Parameters"),
            "{parsed}"
        );
    }

    /// A key read off what `permit` or `expect` hands back, from the request's own `params`, is
    /// what its filter lets through of what the request carries there: `title` a scalar, `tags: []`
    /// an array, `meta: {}` a hash, `address: [:street]` a hash or an array of them, `nil` where
    /// the key never came (`fetch` raises there instead, `require` on a blank value). Kept through
    /// a `def` that hands it back and a local nothing writes into or hands on. A local written
    /// into or handed on, an instance variable, a key the filter does not name, a filter that is
    /// no literal and a `Parameters` the code built answer as before.
    #[test]
    fn a_permitted_key_is_what_its_filter_lets_through() {
        let source = "\
class PostsController < ActionController::Base
  def show
    title = post_params[:title]
    fetched = post_params.fetch(:title)
    required = post_params.require(:title)
    dug = post_params.dig(:title)
    named = post_params[\"title\"]
    tags = post_params[:tags]
    listed = post_params.fetch(:tags)
    meta = post_params[:meta]
    address = post_params[:address]
    unnamed = post_params[:other]
    kept = params.permit(:q)
    q = kept[:q]
    written = params.permit(:q)
    written[:q] = 1
    lost = written[:q]
    handed = params.permit(:q)
    use(handed)
    gone = handed[:q]
    @held = params.permit(:q)
    ivar = @held[:q]
    fields = [:q]
    dynamic = params.permit(fields)[:q]
    built = ActionController::Parameters.new.permit(:q)[:q]
    expected = params.expect(post: [:title, tags: []])
    expected_title = expected[:title]
    expected_tags = expected[:tags]
    several = params.expect(:id, post: [:title])
    listed_out = params.expect(tags: [])
    one = params.expect(:id)
    options = {}
    spread = params.expect(**options)
    unread = params.expect(post: fields)
    splat = params.permit(*fields)
    other_key = kept[fields]
    two = kept.fetch(:q, 1)
    helped = helper(:x).permit(:q)[:q]
    nil
  end

  private

  def helper(key) = params

  def post_params
    params.require(:post).permit(:title, tags: [], meta: {}, address: [:street])
  end
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write(
            "lib/actionpack.rb",
            &format!(
                "{ACTIONPACK}module ActionController\n  class Parameters\n    \
                 def expect(*filters); end\n  end\n  class ExpectedParameterMissing\n  end\nend\n\
                 module ActionDispatch\n  module Http\n    class UploadedFile\n    end\n  end\nend\n\
                 class Symbol\nend\n"
            ),
        );
        let uri = harness.write("app/controllers/posts_controller.rb", source);
        harness.index();
        let scalar = "String | Integer | Float | ActionDispatch::Http::UploadedFile | bool";
        let maybe = "String? | Integer | Float | ActionDispatch::Http::UploadedFile | bool";
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            format!(
                "  def show -> nil
    title: {maybe} = post_params[:title]
    fetched: {maybe} = post_params.fetch(:title)
    required: {scalar} = post_params.require(:title)
    dug: {maybe} = post_params.dig(:title)
    named: {maybe} = post_params[\"title\"]
    tags: Array? = post_params[:tags]
    listed: Array = post_params.fetch(:tags)
    meta: ActionController::Parameters? = post_params[:meta]
    address: Array? | ActionController::Parameters = post_params[:address]
    kept: ActionController::Parameters = params.permit(:q)
    q: {maybe} = kept[:q]
    written: ActionController::Parameters = params.permit(:q)
    handed: ActionController::Parameters = params.permit(:q)
    expected: ActionController::Parameters = params.expect(post: [:title, tags: []])
    expected_title: {maybe} = expected[:title]
    expected_tags: Array? = expected[:tags]
    several: Array = params.expect(:id, post: [:title])
    listed_out: Array = params.expect(tags: [])
    one: {scalar} = params.expect(:id)
    spread: ActionController::Parameters | Array = params.expect(**options)
    unread: ActionController::Parameters | Array = params.expect(post: fields)
    splat: ActionController::Parameters = params.permit(*fields)
  def helper(key) -> ActionController::Parameters = params
  def post_params -> ActionController::Parameters"
            )
        );
    }

    /// What could have changed a permitted value before the read refuses it: a write into the
    /// local the `def` hands back, a memoized instance variable any method may write into, a
    /// `tap` whose block may, a second name an assignment read as a value gives the object, and a
    /// write into the request under the key the filter reads below.
    /// A write of a scalar under a key `permit` reads at the top joins its class. The receiver is
    /// read off the params by literal keys alone: `fetch`, `[]` and `dig` count, and so does an
    /// empty hash for `fetch`'s default, which holds no key; any other default does not.
    #[test]
    fn a_permitted_key_refuses_what_may_have_been_written_since() {
        let source = "\
class PostsController < ActionController::Base
  def show
    local = written_params[:title]
    memo = memo_params[:title]
    tapped = params.permit(:q).tap { |held| held[:q] = 1 }[:q]
    nested = params.require(:post).permit(:title)[:title]
    locale = params.permit(:locale)[:locale]
    chained = (aliased = params.permit(:q))[:q]
    first = second = params.permit(:q)
    first_q = first[:q]
    second_q = second[:q]
    stored = stored_params[:q]
    nil
  end

  private

  def written_params
    held = params.require(:item).permit(:title)
    held[:title] = 1
    held
  end

  def memo_params
    @memo_params ||= params.require(:item).permit(:title)
  end

  def stored_params
    @stored_params = params.permit(:q)
  end

  def set_post
    params[:post] = { title: 1 }
    params[:locale] = :en
  end
end

class ItemsController < ActionController::Base
  def show
    fetched = params.fetch(:item).permit(:title)[:title]
    indexed = params[:item].permit(:title)[:title]
    dug = params.dig(:item, :inner).permit(:title)[:title]
    defaulted = params.fetch(:item, other).permit(:title)[:title]
    emptied = params.fetch(:item, {}).permit(:title)[:title]
    nil
  end
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write(
            "lib/actionpack.rb",
            &format!(
                "{ACTIONPACK}module ActionDispatch\n  module Http\n    class UploadedFile\n    \
                 end\n  end\nend\nclass Symbol\nend\n"
            ),
        );
        let uri = harness.write("app/controllers/posts_controller.rb", source);
        harness.index();
        let maybe = "String? | Integer | Float | ActionDispatch::Http::UploadedFile | bool";
        let symbol =
            "String? | Integer | Float | ActionDispatch::Http::UploadedFile | Symbol | bool";
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            format!(
                "  def show -> nil
    tapped = params.permit(:q).tap {{ |held: ActionController::Parameters| held[:q] = 1 }}[:q]
    locale: {symbol} = params.permit(:locale)[:locale]
    chained = (aliased: ActionController::Parameters = params.permit(:q))[:q]
    first: ActionController::Parameters = second = params.permit(:q)
    first = second: ActionController::Parameters = params.permit(:q)
  def written_params -> ActionController::Parameters
    held: ActionController::Parameters = params.require(:item).permit(:title)
  def memo_params -> ActionController::Parameters
  def stored_params -> ActionController::Parameters
  def set_post -> Symbol
  def show -> nil
    fetched: {maybe} = params.fetch(:item).permit(:title)[:title]
    indexed: {maybe} = params[:item].permit(:title)[:title]
    dug: {maybe} = params.dig(:item, :inner).permit(:title)[:title]
    emptied: {maybe} = params.fetch(:item, {{}}).permit(:title)[:title]"
            )
        );
    }

    /// A filter held in a constant is the list its one assignment writes, frozen as written:
    /// written plainly, under a path, nested, or as a keyword's value, and still where a call on it
    /// elsewhere made rubydex take it for a namespace (`FIELDS.map`). A list left unfrozen, one
    /// written again with an operator anywhere, a constant assigned twice and a list with
    /// anything but names in it answer as a filter that is no literal does.
    #[test]
    fn a_filter_held_in_a_frozen_constant_is_read_as_written() {
        let source = "\
class PostsController < ActionController::Base
  FIELDS = %i[title body].freeze
  NESTED = [:title, { tags: [] }].freeze
  LOOSE = %i[title]
  GROWN = %i[title].freeze
  TWICE = %i[title].freeze
  MIXED = [:title, :body.to_s].freeze

  def show
    title = params.permit(FIELDS)[:title]
    body = params.require(:post).permit(FIELDS).fetch(:body)
    tags = params.permit(NESTED)[:tags]
    pathed = params.permit(Admin::Keys::NAMES)[:name]
    expected = params.expect(post: FIELDS)
    expected_title = expected[:title]
    nested = params.permit(post: NESTED)
    loose = params.permit(LOOSE)[:title]
    grown = params.permit(GROWN)[:title]
    twice = params.permit(TWICE)[:title]
    mixed = params.permit(MIXED)[:title]
    nil
  end
end

class PostsController
  TWICE = %i[body].freeze
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write(
            "lib/actionpack.rb",
            &format!(
                "{ACTIONPACK}module ActionController\n  class Parameters\n    \
                 def expect(*filters); end\n  end\n  class ExpectedParameterMissing\n  end\nend\n\
                 module ActionDispatch\n  module Http\n    class UploadedFile\n    end\n  end\nend\n\
                 class Symbol\nend\n"
            ),
        );
        harness.write(
            "app/models/admin/keys.rb",
            "module Admin\n  module Keys\n    NAMES = %i[name].freeze\n  end\nend\n",
        );
        harness.write(
            "config/initializers/grown.rb",
            "PostsController::GROWN += [{ title: [] }]\nPostsController::FIELDS.map(&:to_s)\n",
        );
        let uri = harness.write("app/controllers/posts_controller.rb", source);
        harness.index();
        let scalar = "String | Integer | Float | ActionDispatch::Http::UploadedFile | bool";
        let maybe = "String? | Integer | Float | ActionDispatch::Http::UploadedFile | bool";
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            format!(
                "  def show -> nil
    title: {maybe} = params.permit(FIELDS)[:title]
    body: {maybe} = params.require(:post).permit(FIELDS).fetch(:body)
    tags: Array? = params.permit(NESTED)[:tags]
    pathed: {maybe} = params.permit(Admin::Keys::NAMES)[:name]
    expected: ActionController::Parameters = params.expect(post: FIELDS)
    expected_title: {maybe} = expected[:title]
    nested: ActionController::Parameters = params.permit(post: NESTED)"
            )
        );
        let _ = scalar;
    }

    /// Every pass asks which project files write a method a routes file calls, and what each says it
    /// does. Both are held: a pass after an edit reads only the document that moved, and the
    /// macro's file is parsed again only when its own text does. What they answer still follows
    /// the files.
    #[test]
    fn a_routes_macro_is_read_again_only_when_its_file_moves() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        let drawn = "Rails.application.routes.draw do\n  admin_resources :posts\nend\n";
        let routes = harness.write("config/routes.rb", drawn);
        let written = "def admin_resources(name)\n  resources name\nend\n";
        let helpers = harness.write("config/route_helpers.rb", written);
        harness.index();
        let reads = |harness: &Harness| {
            let rails = harness
                .analysis
                .knowledge
                .of::<crate::knowledge::rails::Rails>()
                .expect("the Rails body is registered");
            (rails.macro_reads, harness.analysis.defining.borrow().reads)
        };
        let wanted = std::collections::BTreeSet::from(["admin_resources".to_owned()]);
        let (macros, defining) = reads(&harness);
        assert_eq!(macros, 1, "the one file writing the macro");
        assert!(defining > 1, "every own document, once");
        assert_eq!(
            harness.analysis.defining_documents(&wanted)["admin_resources"],
            std::slice::from_ref(&helpers)
        );

        // The routes file moves: one document read again, the macro's file not parsed.
        harness.open(&routes, drawn);
        let (macros, defining) = reads(&harness);
        harness.change(
            &routes,
            "Rails.application.routes.draw do\n  admin_resources :posts\n  admin_resources :tags\nend\n",
        );
        assert_eq!(reads(&harness), (macros, defining + 1));

        // The macro's file moves alone, on no list: the pass runs, parses it again and reads it
        // alone.
        harness.open(&helpers, written);
        let (macros, defining) = reads(&harness);
        harness.change(&helpers, &format!("# Routing helpers.\n{written}"));
        assert_eq!(reads(&harness), (macros + 1, defining + 1));

        // Its `def` gone, nothing writes the name any more.
        harness.change(&helpers, "# Routing helpers.\n");
        assert!(harness.analysis.defining_documents(&wanted).is_empty());
    }

    /// What a project helper a routes file calls does bounds the route it draws: a helper handing
    /// its argument to `resources` may reach only that resource's controller, so `comments`' own
    /// route still proves `:id`. An edit to the helper's file alone, which no list names, moves
    /// that at the next settle: a helper that is no macro may reach any controller.
    #[test]
    fn an_edit_to_a_routes_helper_alone_moves_the_proof() {
        let source = "\
class CommentsController < ActionController::Base
  def show
    id = params[:id]
  end
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write(
            "lib/actionpack.rb",
            &format!(
                "{ACTIONPACK}module ActionDispatch\n  module Http\n    class UploadedFile\n    \
                 end\n  end\nend\nclass Symbol\nend\n"
            ),
        );
        harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :comments, only: :show\n  \
             admin_resources :posts\nend\n",
        );
        let written = "def admin_resources(name)\n  resources name\nend\n";
        let helpers = harness.write("config/route_helpers.rb", written);
        let uri = harness.write("app/controllers/comments_controller.rb", source);
        harness.index();
        let proven = "    id: String = params[:id]";
        let before = drawn_hints(source, &harness.hints_in(&uri));
        assert!(before.contains(proven), "{before}");

        harness.open(&helpers, written);
        harness.change(&helpers, "def admin_resources(name)\n  get name\nend\n");
        let after = drawn_hints(source, &harness.hints_in(&uri));
        assert!(!after.contains(proven), "{after}");
    }

    /// A key every route reaching the read requires is a `String`, and one a route's default gives
    /// is that default's class: `show` runs only under `/posts/:id`, `feed` under a default
    /// `id: :latest`. `index` has no `:id`, and neither has a helper `index` calls.
    #[test]
    fn a_required_route_segment_is_a_string() {
        let source = "\
class PostsController < ActionController::Base
  before_action :load, only: :show

  def show
    id = params[:id]
    required = params.require(:id)
    other = params[:page]
  end

  def feed
    latest = params[:id]
  end

  def index
    helper
  end

  private

  def load
    loaded = params[:id]
  end

  def helper
    unproven = params[:id]
  end
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/bundle.rb", BUNDLE);
        harness.write(
            "lib/actionpack.rb",
            &format!(
                "{ACTIONPACK}module ActionDispatch\n  module Http\n    class UploadedFile\n    \
                 end\n  end\nend\nclass Symbol\nend\n"
            ),
        );
        harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :posts, only: %i[index show]\n  \
             get \"feed\", to: \"posts#feed\", id: :latest\nend\n",
        );
        let uri = harness.write("app/controllers/posts_controller.rb", source);
        harness.index();
        let union = "String? | Integer | Float | Array | ActionController::Parameters | \
                     ActionDispatch::Http::UploadedFile | bool";
        let defaulted = "String? | Integer | Float | Array | ActionController::Parameters | \
                         ActionDispatch::Http::UploadedFile | Symbol | bool";
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // A key nothing proves is the union with every class a route's default gives it.
            format!(
                "  def show -> {union}
    id: String = params[:id]
    required: String = params.require(:id)
    other: {union} = params[:page]
  def feed -> Symbol
    latest: Symbol = params[:id]
  def index -> {defaulted}
  def load -> String
    loaded: String = params[:id]
  def helper -> {defaulted}
    unproven: {defaulted} = params[:id]"
            )
        );
    }

    /// What can give a key something else is read only from files the application loads (a
    /// spec's write is the suite's), a module's write counts in the controllers that include it,
    /// and routes are read through the files a `draw` names and the project's own routing
    /// macros. With routes off nothing a route gives a key is read, so nothing answers.
    #[test]
    fn the_request_gates_read_what_the_application_loads() {
        let source = "\
module Admin
  class PostsController < ApplicationController
    include Loading

    def show
      id = params[:id]
      kind = params[:kind]
    end
  end
end
";
        let drawn = |config: &str| {
            let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], config);
            harness.write("lib/bundle.rb", BUNDLE);
            harness.write(
                "lib/actionpack.rb",
                &format!(
                    "{ACTIONPACK}module ActionDispatch\n  module Http\n    class UploadedFile\n    \
                     end\n  end\nend\nclass Symbol\nend\n"
                ),
            );
            harness.write(
                "config/routes.rb",
                "Rails.application.routes.draw do\n  draw :admin\n  listed :reports\nend\n",
            );
            harness.write(
                "config/routes/admin.rb",
                "namespace :admin do\n  resources :posts, only: :show\nend\n",
            );
            harness.write(
                "lib/routing.rb",
                "module Routing\n  def listed(name)\n    resources name, only: :index\n  end\nend\n",
            );
            harness.write(
                "app/controllers/concerns/loading.rb",
                "module Loading\n  def load_it\n    params[:kind] = :loaded\n  end\nend\n",
            );
            harness.write(
                "spec/controllers/posts_spec.rb",
                "class Admin::PostsController\n  def tweak\n    params[:id] = :x\n  end\nend\n",
            );
            harness.write(
                "app/controllers/application_controller.rb",
                "class ApplicationController < ActionController::Base\nend\n",
            );
            let uri = harness.write("app/controllers/admin/posts_controller.rb", source);
            harness.index();
            drawn_hints(source, &harness.hints_in(&uri))
        };
        let union = "String? | Integer | Float | Array | ActionController::Parameters | \
                     ActionDispatch::Http::UploadedFile | Symbol | bool";
        assert_eq!(
            drawn(""),
            format!(
                "    def show -> {union}
      id: String = params[:id]
      kind: {union} = params[:kind]"
            )
        );
        assert_eq!(drawn("[rails]\nroutes = false\n"), "null");
    }

    /// A controller's `cookies` is private in actionpack, and its signature says so, so it stays
    /// private whichever definition rubydex reads its visibility from.
    /// A controller's `helpers`, and its class object's, reach the application's helper modules
    /// and the framework's alike, through the class ya-lsp writes for the view object Rails makes.
    /// A jump on `helpers` still lands on Rails' own `def`: the rows say only what it returns.
    #[test]
    fn a_controllers_helpers_reaches_the_applications_helpers() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write(
            "lib/helpers.rb",
            "module ActionView\n  module Helpers\n    module SanitizeHelper\n      \
             def strip_tags(html)\n        html\n      end\n    end\n    \
             include SanitizeHelper\n  end\n\n  class Base\n    include Helpers\n  end\nend\n\n\
             module ActionController\n  module Helpers\n    def helpers\n      \
             @_helper_proxy ||= view_context\n    end\n  end\n\n  class Base\n    \
             include Helpers\n  end\nend\n",
        );
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def cover_url(image)\n    image.to_s\n  end\nend\n",
        );
        let controller = "\
class ApplicationController < ActionController::Base
  def show
    cover = helpers.cover_url(1)
  end
end
";
        let controller_uri = harness.write("app/controllers/application_controller.rb", controller);
        let source = "\
proxy = ApplicationController.helpers
cover = ApplicationController.helpers.cover_url(1)
stripped = ActionController::Base.helpers.strip_tags(\"<b>x</b>\")
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
proxy: ActionView::Base = ApplicationController.helpers
cover: String = ApplicationController.helpers.cover_url(1)
stripped: String = ActionController::Base.helpers.strip_tags(\"<b>x</b>\")"
        );
        // The framework's own helper, through `ActionView::Base`. Its body hands back what the
        // call passes. Its one caller names neither `ActionView::Base` nor the helper, so the card
        // types no parameter.
        let stripped = card(&mut harness, &uri, source, "strip_tags");
        assert!(
            stripped.contains("ActionView::Helpers::SanitizeHelper#strip_tags(html) -> String"),
            "{stripped}"
        );
        assert!(!stripped.contains("Guessed from name alone"), "{stripped}");
        assert_eq!(
            drawn_hints(controller, &harness.hints_in(&controller_uri)),
            "  def show -> String\n    cover: String = helpers.cover_url(1)"
        );
        let definition = harness.definition_at(&controller_uri, controller, "helpers");
        assert!(
            definition.to_string().contains("lib/helpers.rb"),
            "{definition}"
        );
    }

    /// A Sidekiq worker's `perform` runs with what JSON hands back and a channel's actions with
    /// what a client sends, so their callers say nothing about their parameters; a helper beside
    /// them is what its callers pass.
    #[test]
    fn a_method_a_framework_calls_is_not_typed_by_its_callers() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write(
            "lib/frameworks.rb",
            "module Sidekiq\n  module Job\n  end\nend\n\n\
             module ActionCable\n  module Channel\n    class Base\n    end\n  end\nend\n",
        );
        let source = "\
class HardJob
  include Sidekiq::Job

  def perform(count)
    count
  end

  def helper(count)
    count
  end
end

class ChatChannel < ActionCable::Channel::Base
  def speak(data)
    data
  end
end

HardJob.new.perform(1)
HardJob.new.helper(1)
ChatChannel.new.speak(1)
";
        let uri = harness.write("app/jobs/hard_job.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def helper(count) -> Integer"
        );
    }

    /// `Rails.logger` is the `BroadcastLogger` Rails wraps every logger in, and its `class_eval`'d
    /// `info` is a member. An application assigning it after boot adds what it assigns: a
    /// `Logger` joins, `nil` adds the mark, and a value nothing types refuses the whole. `Rails`'
    /// own `@@logger` refuses nothing: railties keeps the logger in an instance variable.
    #[test]
    fn rails_logger_is_the_broadcast_logger_or_what_the_application_assigns() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write(
            "lib/bundle_logger.rb",
            "module Rails\n  class << self\n    attr_accessor :logger\n  end\nend\n\n\
             module ActiveSupport\n  class BroadcastLogger\n    def broadcasts\n      []\n    \
             end\n  end\nend\n\nclass Logger\n  def info(message)\n    true\n  end\nend\n",
        );
        // `Rails`' own `@@logger`: where a `mattr_accessor` would keep its value, and
        // `Rails.logger` does not.
        harness.write(
            "lib/rails_patch.rb",
            "module Rails\n  @@logger = nil\nend\n",
        );
        let source = "logger = Rails.logger\nRails.logger.info(\"x\")\n";
        let uri = harness.write("app/main.rb", source);
        let initializer = harness.write("config/initializers/logging.rb", "# nothing yet\n");
        harness.index();
        let label = |harness: &mut Harness| drawn_hints(source, &harness.hints_in(&uri));
        assert_eq!(
            label(&mut harness),
            "logger: ActiveSupport::BroadcastLogger = Rails.logger"
        );
        let info = card(&mut harness, &uri, source, "info");
        assert!(
            info.contains("ActiveSupport::BroadcastLogger#info"),
            "{info}"
        );
        assert!(!info.contains("Guessed from name alone"), "{info}");

        for (assigned, expected) in [
            (
                "Rails.logger = Logger.new\n",
                "logger: ActiveSupport::BroadcastLogger | Logger = Rails.logger",
            ),
            (
                "Rails.logger = nil\n",
                "logger: ActiveSupport::BroadcastLogger? = Rails.logger",
            ),
            ("Rails.logger = mystery\n", "null"),
        ] {
            harness.write("config/initializers/logging.rb", assigned);
            harness.watch(&[&initializer]);
            assert_eq!(label(&mut harness), expected, "{assigned}");
        }
    }

    /// A connection is the adapter `config/database.yml` names: the environment's primary database
    /// for every model, and the database a model's `connects_to` names for that model. The pool
    /// carries its adapter, so `with_connection` hands the block the same class. Editing the file
    /// changes the answer.
    #[test]
    fn a_connection_is_the_adapter_the_database_configuration_names() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write(
            "lib/activerecord.rb",
            "module ActiveRecord\n  module ConnectionHandling\n    def connection; end\n    \
             def connection_pool; end\n    def lease_connection; end\n    \
             def with_connection(prevent_permanent_checkout: false); end\n  end\n\n  \
             module ConnectionAdapters\n    class TransactionManager\n      \
             def open_transactions\n        0\n      end\n    end\n\n    \
             module DatabaseStatements\n      attr_reader :transaction_manager\n\n      \
             def initialize\n        @transaction_manager = TransactionManager.new\n      end\n    \
             end\n\n    class AbstractAdapter\n      include DatabaseStatements\n    end\n\n    \
             class PostgreSQLAdapter < AbstractAdapter\n    end\n\n    \
             class SQLite3Adapter < AbstractAdapter\n    end\n\n    class ConnectionPool\n      \
             class LeaseRegistry\n      end\n    end\n  end\n\n  class Base\n    \
             extend ConnectionHandling\n  end\nend\n\nmodule PG\nend\n\nmodule SQLite3\nend\n",
        );
        harness.write(
            "config/application.rb",
            "module App\n  class Application < Rails::Application\n  end\nend\n",
        );
        let config = harness.write(
            "config/database.yml",
            "default: &default\n  adapter: postgresql\n\ndevelopment:\n  primary:\n    \
             <<: *default\n  animals:\n    adapter: sqlite3\n",
        );
        harness.write(
            "app/models/animal_record.rb",
            "class AnimalRecord < ActiveRecord::Base\n  connects_to database: { writing: :animals }\nend\n",
        );
        let source = "\
main = ActiveRecord::Base.connection
leased = ActiveRecord::Base.lease_connection
animals = AnimalRecord.connection
ActiveRecord::Base.with_connection { |conn| conn }
AnimalRecord.connection_pool.with_connection { |conn| conn }
depth = ActiveRecord::Base.connection.open_transactions
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        let pg = "ActiveRecord::ConnectionAdapters::PostgreSQLAdapter";
        let sqlite = "ActiveRecord::ConnectionAdapters::SQLite3Adapter";
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            format!(
                "\
main: {pg} = ActiveRecord::Base.connection
leased: {pg} = ActiveRecord::Base.lease_connection
animals: {sqlite} = AnimalRecord.connection
ActiveRecord::Base.with_connection {{ |conn: {pg}| conn }}
AnimalRecord.connection_pool.with_connection {{ |conn: {sqlite}| conn }}
depth: Integer = ActiveRecord::Base.connection.open_transactions"
            )
        );
        // 7.2 deprecates the pool's own `connection` and 8.0 removes it: no row invents it.
        assert!(!harness.has("ActiveRecord::ConnectionAdapters::ConnectionPool#connection()"));

        harness.write("config/database.yml", "development:\n  adapter: sqlite3\n");
        harness.watch(&[&config]);
        let hints = drawn_hints(source, &harness.hints_in(&uri));
        assert!(
            hints.starts_with(&format!("main: {sqlite} = ActiveRecord::Base.connection")),
            "{hints}"
        );
        // `animals` names no database any more: whatever the bundle loads, PostgreSQL and SQLite.
        assert!(
            hints.contains("animals: ActiveRecord::ConnectionAdapters::AbstractAdapter"),
            "{hints}"
        );
    }

    /// An adapter a gem registers is read from its `*adapter.rb`, and an engine, which has no
    /// `config/application.rb`, is connected by its host: any adapter.
    #[test]
    fn a_registered_adapter_is_read_and_an_engine_s_connection_is_any_adapter() {
        let bundle = "module ActiveRecord\n  module ConnectionHandling\n    def connection; end\n  \
                      end\n\n  module ConnectionAdapters\n    class AbstractAdapter\n    end\n\n    \
                      class PostgreSQLAdapter < AbstractAdapter\n    end\n  end\n\n  class Base\n    \
                      extend ConnectionHandling\n  end\nend\n\nmodule PG\nend\n";
        let postgis = "module ActiveRecord\n  module ConnectionAdapters\n    \
                       class PostGISAdapter < PostgreSQLAdapter\n    end\n  end\nend\n\n\
                       ActiveRecord::ConnectionAdapters.register(\"postgis\", \
                       \"ActiveRecord::ConnectionAdapters::PostGISAdapter\", \"x\")\n";
        let source = "main = ActiveRecord::Base.connection\n";
        for (application, expected) in [
            (true, "ActiveRecord::ConnectionAdapters::PostGISAdapter"),
            (false, "ActiveRecord::ConnectionAdapters::AbstractAdapter"),
        ] {
            let (mut harness, _schema, _uri) = rails_project("");
            harness.write("lib/activerecord.rb", bundle);
            harness.write("lib/postgis_adapter.rb", postgis);
            if application {
                harness.write(
                    "config/application.rb",
                    "module App\n  class Application < Rails::Application\n  end\nend\n",
                );
            }
            harness.write("config/database.yml", "development:\n  adapter: postgis\n");
            let uri = harness.write("app/main.rb", source);
            harness.index();
            assert_eq!(
                drawn_hints(source, &harness.hints_in(&uri)),
                format!("main: {expected} = ActiveRecord::Base.connection"),
                "an application: {application}"
            );
            // No `LeaseRegistry` in this bundle, so it is older than 7.2: no `lease_connection`.
            assert!(!harness.has("ActiveRecord::ConnectionHandling#lease_connection()"));
        }
    }

    #[test]
    fn a_private_framework_method_stays_private() {
        let source = "\
PostsController.new.cookies

class PostsController < ActionController::Base
  def show
    cookies
  end
end
";
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/actionpack.rb", ACTIONPACK);
        let uri = harness.write("app/controllers/posts_controller.rb", source);
        let session =
            "module ActionDispatch\n  class Request\n    class Session\n    end\n  end\nend\n";
        let elsewhere = harness.write("lib/session.rb", session);
        harness.index();
        // Twice: as first indexed, and after the host's generated document alone is indexed again,
        // which changes the order its definitions sit in. Emptying another file drops the `session`
        // row, so the host's generated text changes while its Ruby does not.
        for round in 0..2 {
            let inside = card(&mut harness, &uri, source, "cookies\n  end");
            assert!(
                inside.contains("private ActionController::Cookies#cookies"),
                "{round}: {inside}"
            );
            // Written on a receiver, Ruby refuses it, so nothing answers.
            let outside = harness.hover_at(&uri, source, "cookies\n\nclass");
            assert!(outside.is_null(), "{round}: {outside}");
            harness.open(&elsewhere, session);
            harness.change(&elsewhere, "# nothing here now\n");
        }
    }

    /// railties, activesupport, actionmailer and actionpack as far as the block and `config` rows
    /// need them.
    const RAILTIES: &str = "\
module Rails
  def self.application; end
  class Railtie
    def configure(&block); end
    class Configuration
    end
  end
  class Engine < Railtie
    def routes(&block); end
    class Configuration < Railtie::Configuration
    end
  end
  class Application < Engine
    def config; end
    class Configuration < Engine::Configuration
      attr_accessor :hosts
    end
  end
end
module ActiveSupport
  class OrderedOptions
  end
end
module ActionMailer
end
module ActionDispatch
  module Routing
    class RouteSet
      def draw(&block); end
    end
    class Mapper
      def resources(*names); end
    end
  end
end
";

    /// `configure`'s block runs against the application, so `config` is its configuration and a
    /// framework's namespace is the options its railtie assigns; `routes.draw`'s runs against the
    /// route mapper, where `resources` is.
    #[test]
    fn configure_and_draw_run_their_blocks_against_the_application_and_the_mapper() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/railties.rb", RAILTIES);
        harness.write("config/application.rb", APPLICATION_RB);
        let source = "\
Rails.application.configure do
  mailer = config.action_mailer
  hosts = config.hosts
end

Rails.application.routes.draw do
  resources :stories
end
";
        let uri = harness.write("config/environments/production.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  mailer: ActiveSupport::OrderedOptions = config.action_mailer
  hosts: Array = config.hosts"
        );
        let card = card(&mut harness, &uri, source, "resources");
        assert!(
            card.contains("ActionDispatch::Routing::Mapper#resources"),
            "{card}"
        );
    }

    /// A slice of activerecord as it writes what a migration reaches: a statement module the
    /// connection includes, the column module `define_column_methods` fills, and the two table
    /// classes that include it.
    const ACTIVERECORD: &str = "\
module ActiveRecord
  class Migration
    def method_missing(name, *arguments, &block); end
  end
  module ConnectionAdapters
    module SchemaStatements
      def create_table(table_name, **options, &block); end
      def add_index(table_name, column_name, **options); end
    end
    module ColumnMethods
      define_column_methods :string, :integer
    end
    class TableDefinition
      include ColumnMethods
      def timestamps(**options); end
    end
    class Table
      include ColumnMethods
    end
  end
end
";

    /// Where a jump from `needle` lands, as `file.rb:line`.
    fn landed(harness: &mut Harness, uri: &DocUri, source: &str, needle: &str) -> Vec<String> {
        let found = harness.definition_at(uri, source, needle);
        found
            .as_array()
            .into_iter()
            .flatten()
            .map(|link| {
                let file = link["targetUri"].as_str().unwrap_or_default();
                let line = link["targetSelectionRange"]["start"]["line"]
                    .as_u64()
                    .unwrap_or_default();
                format!(
                    "{}:{}",
                    file.rsplit('/').next().unwrap_or_default(),
                    line + 1
                )
            })
            .collect()
    }

    /// A migration's own calls go through `method_missing` to the connection, so each is the
    /// statement module's `def`; `create_table` hands its block a table definition, whose column
    /// methods `define_column_methods` wrote.
    #[test]
    fn a_migration_s_own_calls_are_the_connection_s_statements() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("lib/active_record.rb", ACTIVERECORD);
        let source = "\
class CreateWidgets < ActiveRecord::Migration[8.0]
  def change
    create_table :widgets do |t|
      t.string :name
      t.timestamps
    end
    add_index :widgets, :name
  end
end
";
        let uri = harness.write("db/migrate/20260926000000_create_widgets.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    create_table :widgets do |t: ActiveRecord::ConnectionAdapters::TableDefinition|"
        );
        assert_eq!(
            landed(&mut harness, &uri, source, "add_index"),
            ["active_record.rb:8"]
        );
        assert_eq!(
            landed(&mut harness, &uri, source, "string"),
            ["active_record.rb:11"]
        );
        let card = card(&mut harness, &uri, source, "create_table");
        assert!(
            card.contains("ActiveRecord::Migration#create_table"),
            "{card}"
        );
    }

    /// A workspace whose bundle declares none of it declares nothing, and the chain stays on the
    /// name rung: an honest miss, not a `Rails` this crate invented.
    #[test]
    fn a_project_with_no_framework_indexed_declares_nothing() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("config/application.rb", APPLICATION_RB);
        let source = "Rails.root.join(\"config\")\n";
        let uri = harness.write("app/use.rb", source);
        harness.index();
        let card = card(&mut harness, &uri, source, "join");
        assert!(!card.contains("Pathname#join"), "{card}");
        assert!(card.contains("Guessed from name alone"), "{card}");
    }
}
