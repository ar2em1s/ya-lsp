//! The pass that reads what the workspace declares about itself and writes the RBS it implies.
//!
//! Every generator meets here: [`rails`] reads a `db/schema.rb` or a model's macros,
//! [`annotations`] reads a Sorbet `sig` or a YARD tag, each ends at a [`Facts`], and one document
//! per source file is handed to [`Synthesized::record`](synthesized::Synthesized::record).
//!
//! # What the generators may read, and what they may never wait for
//!
//! This runs **immediately before** [`Resolver::resolve`](rubydex::analysis::Resolver), so the
//! declarations it writes are linked by the same resolve rather than by a second one. The price
//! is the bounding rule every generator inherits: **declarations do not exist yet, and
//! definitions are the only thing there is to read.** A generator may ask which classes the
//! application defines and what a file's text says; it may not ask what `User#name` resolves to,
//! because nothing has resolved.
//!
//! # Two phases, and the second is `delegate`'s
//!
//! `delegate :name, to: :user` needs `Story#user -> User` and then `User#name -> String`, both
//! written in this same pass into other files' documents. [`Facts::returns`] is the answer: the
//! facts exist before any of them is rendered, so a *second* phase can ask the first what it
//! said.
//!
//! The loop is [`Analysis::delegate_declarations`] — the union of every fact phase one stated,
//! asked twice per `delegate` — and two things bound it. It is built **only when some file writes
//! a `delegate`**, so a project with none pays nothing; and it is built **once**, after every
//! phase-one generator has spoken, so no generator ordering is assumed and none can be. What
//! phase two writes is not in it, which is why a `delegate` whose target is another `delegate`
//! answers `untyped`: the alternative is iterating to a fixed point over a graph a user can write
//! a cycle into.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Instant;

use rubydex::model::{
    definitions::{Definition, Mixin, Receiver},
    document::Document,
    graph::Graph,
    ids::{DeclarationId, NameId, StringId, UriId},
    name::ParentScope,
};
use rubydex::query::{self, MatchMode};

use super::{
    Analysis, environment, locator, locator::Site, render, synthesized, synthesized::Synthesized,
    views::Views,
};
use crate::generated::{At, Named, candidates};
use crate::knowledge::{self, Context, Contribution, ListId, Registry, Seen, Wants};
use crate::workspace::DocUri;

impl Analysis {
    /// Find the file Rails writes each generated query method in, now the graph can say.
    ///
    /// **Run after the resolve and never inside the pass**, which is not a preference: the
    /// generators write the text rubydex is about to link, so while one is running the graph
    /// holds four declarations — `Object`, `BasicObject`, `Module` and `Class` — and every
    /// question about a gem's classes answers nothing. The pass therefore states *names* and
    /// this answers them, one step later in the same settle.
    ///
    /// Asked again on every settle rather than cached across one, for the reason the rest of
    /// this module gives about stale answers: a bundle can change under a workspace, and a jump
    /// into the version that went away is silent. What bounds the cost is that the answers are
    /// memoised **within** a settle — the class side is written onto every base in the project
    /// and states the same names on each, so a project with six bases asks the graph once and
    /// reads the memo five times.
    pub(super) fn place_generated_members(&mut self) {
        if self.synthesized.named().next().is_none() {
            return;
        }
        let started = Instant::now();
        let mut rails = Owners::new(&self.graph, &self.knowledge);
        let layout = self.layout();
        // Collected before anything is written, because resolving reads the table a place is
        // written into — `locator::places` consults it to tell a generated definition from one
        // on disk, and it must see the same table for every member in one settle.
        let placed: Vec<(UriId, Vec<synthesized::Mapping>)> = self
            .synthesized
            .named()
            .map(|(document, named)| {
                (
                    document,
                    rails.mappings(&self.graph, &self.synthesized, layout, named),
                )
            })
            .collect();
        let found = placed.iter().map(|(_, found)| found.len()).sum::<usize>();
        let asked = placed.len();

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

/// Where a generated member is really defined, for the ones whose place is somebody else's file.
///
/// # The one kind of generated member whose place is not a line the generator read
///
/// Every other generator's place is a line in the document it just read: a column's is the
/// `t.string "title"` in `db/schema.rb`, an association's the `has_many :comments`. Some members
/// have no such line — nothing in a project declares `Story.where` — which is why they were
/// written with no span at all and why a *Resolved* card over one sent a reader nowhere.
///
/// They do have a definition, and the bundle is already indexed: `where` is a `def` in
/// activerecord's `relation/query_methods.rb`. So the place is found rather than read, by asking
/// the graph the question Ruby asks — walk this class' ancestors for this name — and the answer
/// is `Method#owner`'s by construction, because rubydex built those ancestors out of the
/// framework's own `include` line.
///
/// **This is a lookup and never a name match.**
/// [`Knowledge::places_members_on`](crate::knowledge::Knowledge::places_members_on) is where the
/// classes come from, one list per module and per side; a member is found on one of them or it is
/// not found, and not found means no place, exactly as before. Two things follow that are worth
/// saying out loud. A framework that moves a name loses its place and gains nothing wrong; and a
/// project with no bundle indexed is where it was.
///
/// # What is deliberately not looked up
///
/// The callbacks. `before_save` is built by `define_model_callbacks`, and a jump into the
/// machinery that defines a *family* of methods tells a reader nothing about the one they asked
/// about — `workspace/rails/relations.rs`'s own argument, written before this existed and not
/// overturned by it. The line is between *the file that defines this method* and *the file that
/// defines methods*.
struct Owners {
    /// The owners to walk, in order, for a member on each side: instance, then class object.
    owners: [Vec<DeclarationId>; 2],
    /// `(singleton, name)` -> where Rails writes it, or that nothing does.
    ///
    /// A miss is cached as eagerly as a hit. Most of what arrives here is a miss — a project's
    /// bases are many and its query interface is one list — and re-asking the graph for a name
    /// that was not there last time is the commonest thing this could get wrong about cost.
    found: HashMap<(bool, String), Option<Site>>,
}

impl Owners {
    /// Every registered module's own, resolved once per settle.
    ///
    /// The names come from [`knowledge::Knowledge::places_members_on`] and the lookup is core's:
    /// this runs **after** the resolve, so the graph can answer, which is the one thing a module
    /// may not assume while it is declaring.
    fn new(graph: &Graph, knowledge: &Registry) -> Self {
        let resolve = |names: &[&str]| -> Vec<DeclarationId> {
            names
                .iter()
                .flat_map(|name| query::declaration_search(graph, &[name], &MatchMode::Exact))
                .collect()
        };
        let mut owners: [Vec<DeclarationId>; 2] = [Vec::new(), Vec::new()];
        for module in knowledge.modules() {
            let [instance, singleton] = module.places_members_on();
            owners[0].extend(resolve(instance));
            owners[1].extend(resolve(singleton));
        }
        Self {
            owners,
            found: HashMap::new(),
        }
    }

    /// One `Mapping` per member this can place, and nothing at all for the rest.
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
        // rubydex spells a method that takes anything at all `where()` and one that takes
        // nothing `first`, and the spelling is not derivable from this side: what ya-lsp wrote is
        // an RBS parameter list and what Rails wrote is a `def`. Both are asked, which is what
        // `references` already does with the same two spellings for the same reason.
        let spellings = [
            StringId::from(member.name.as_str()),
            StringId::from(&*format!("{}()", member.name)),
        ];
        self.owners[usize::from(member.singleton)]
            .iter()
            .find_map(|owner| {
                // **A hit on the object model is not an answer**, the same rule and the same
                // reason as [`locator::ruby_s_own`]: every ancestor walk terminates at `Object`,
                // and `Kernel` alone declares `select`, `format`, `open` and `test`. The
                // corpora found this and the suite could not have: four positions over the six
                // answered `select` with `IO.select` in `core/kernel.rbs`, each of them a
                // *Resolved* or *Derived* card pointing at a method nobody was asking about —
                // the one failure this whole lookup has to be incapable of.
                //
                // Written inside the spelling loop rather than around it so that a root hit for
                // one spelling cannot suppress the other's real answer. No fixture distinguishes
                // the two placements; this is the conservative one.
                let found = spellings.iter().find_map(|spelling| {
                    query::find_member_in_ancestors(graph, *owner, *spelling, false)
                        .ok()
                        .filter(|found| !locator::ruby_s_own(graph, *found))
                })?;
                // The same list a jump would offer, so a borrowed place obeys every rule a read
                // one does: an `.rbs` stub loses to the source beside it, and a second copy of
                // one library under a later load path is not a second place. No cursor, because
                // this is decided once for the project and not per request — which leaves the
                // test-tree fence off, the direction that keeps more rather than less.
                locator::places(graph, synthesized, layout, found, None)
                    .into_iter()
                    .next()
            })
    }
}

/// The `StringId`s one document is filtered against, hashed once.
///
/// A struct rather than two locals inside the loop because the loop body is callable for
/// **one** document: the gate re-asks [`Analysis::contribution`] of the document a keystroke
/// touched, and building the filter per call would hash every registered row's names to answer
/// a single file.
struct Filters {
    /// Per registered row, the hashes of its `calls`.
    calls: Vec<Vec<StringId>>,
    /// Per registered row, the hashes of its `modules`.
    modules: Vec<Vec<StringId>>,
    /// Per registered row, the hashes of its `constants`.
    constants: Vec<Vec<StringId>>,
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
        }
    }
}

/// How many documents each generator was handed, for the one line that says the pass ran.
///
/// Only the lists that have anything on them. A project with no `db/schema.rb` and no
/// `config/routes.rb` should read as *models 412* rather than as five zeroes, because the
/// zeroes are the ordinary case and the non-zeroes are the answer.
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

/// What a template can call, out of the walk the generators already made.
///
/// A free function rather than a method for the reason [`models_of`] is one: nothing in it
/// reads the graph, and both of its inputs are already gathered. It is **not** a generator —
/// nothing here is rendered, nothing is recorded in [`super::synthesized::Synthesized`], and no
/// declaration is written — which is why it sits beside the pass rather than in it: what
/// `helper_method` hands over is a permission, and the `def` it names is already indexed.
///
impl Analysis {}

/// Which classes each concern's macros really land on, resolved and then closed over.
///
/// Two steps, and both are somebody else's rule copied rather than invented. An `include` names
/// a constant, and Ruby resolves it against the nesting of the body that wrote it — which is
/// [`candidates`], the same list `compute_type` gives an association's `class_name`. A module the application does not define resolves to nothing and
/// contributes nothing, which is the decline every reader in this pass makes and is why
/// `include Sidekiq::Worker` adds no row here.
///
/// Then the closure. `ActiveSupport::Concern` hands an inner concern's `included` block on to
/// whatever includes the outer one, so `Poll` including `Bigger` including `Expireable` really
/// does get `Expireable`'s `scope` — and a module is walked *through* rather than recorded,
/// because `Bigger.expired` is not something anybody can call. `seen` is the cycle guard, and
/// it is about *source* rather than about Ruby: a module that includes itself is a
/// `NoMethodError` at run time and an infinite loop here.
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

/// Every name a generated one could hang a segment off, so the walk below has a bound.
///
/// The proper prefixes of everything the application declares, minus the names it declares
/// itself — plus the four constants this crate invents or looks for by name. That is exactly the
/// set of namespaces a generated owner can *introduce*: an owner is either a name `Context`
/// already holds, a name derived from one (`Comment::Relation`), or
/// a module's own spellable names, and no other segment can appear above one.
///
/// Bounding it is the whole reason this is affordable. Measured over lobsters, spelling **every**
/// class and module in the graph costs 20.7 ms a settle against a resolve of 19–23; filtering on
/// the last segment first, and spelling only what matches, costs **2.5 ms** — because the filter
/// is a `StringId` compare and the walk never touches the name of the 26,514 namespaces nobody
/// asked about. Discourse is 27.2 ms against 3.6.
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
    // Whatever each module writes onto that is not the application's own — the names it needs
    // spellable and cannot get from `classes`. Asked of the registry, so a build with nothing
    // registered asks the bundle about nothing but its own prefixes.
    for module in knowledge.modules() {
        for name in module.spellable_names() {
            prefixes(name, &mut wanted);
            wanted.insert(name.to_owned());
        }
    }
    // **Every class a concern's class methods are written onto**, which is where this pass reaches
    // furthest outside the application: `ActionController::Base` includes a dozen concerns and no
    // file of the user's declares it, so without this its namespace is never asked about and
    // `Namespaces::spellable` declines the owner — silently, and for every member of every concern
    // it includes. Only the *prefixes* are needed, because `spellable` asks about the namespaces
    // above a name and never about the name itself.
    for includers in context.includers.values() {
        for name in includers {
            prefixes(name, &mut wanted);
        }
    }
    // A name the application declares needs no second opinion, and asking for one would let a
    // gem's `class Story` overrule the `module Story` this workspace wrote.
    wanted.retain(|name| !context.classes.contains(name));
    wanted
}

/// A path inside an unpacked gem, from the gem's own directory down.
///
/// `…/gems/shouty-1.2.3/config/routes.rb` becomes `shouty-1.2.3/config/routes.rb`. The marker is
/// a directory literally named `gems`, which every layout `gems::gem_roots` knows ends in — a
/// RubyGems root, a vendored bundle, and `bundler/gems` for a git source. `None` for a path that
/// has no such ancestor, so the caller keeps its own fallback rather than this one guessing.
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

/// What one file looked like when the pass last read it: when it was written, and how long.
///
/// `None` for a file that is not there — which is a value rather than an error, because a
/// schema that has been deleted and a schema that never existed have to compare unequal to one
/// that did. Length as well as time because a filesystem's modification time is coarse and two
/// writes inside one tick are a real edit.
fn stamp_of(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

impl Analysis {
    /// Re-read everything the workspace declares about itself, and re-write the RBS it implies.
    ///
    /// Runs immediately before every `resolve` — the two call sites are the cold index and the
    /// debounce — because the inputs move independently and almost none of them is watched: an
    /// edit to a schema changes the columns, a `belongs_to` changes a member, and a *new model
    /// file* changes which of them belong to a class anyone can name. (The one input that **is**
    /// watched is `db/*structure.sql`, and it is watched because it is the one input
    /// that is not a graph document — nothing else would ever notice it had changed.) Regenerating from all of it,
    /// every time, is what makes "the answer is a function of what is on disk" true instead of
    /// nearly true. It is also what makes it *cheap*, because
    /// [`synthesized::Synthesized::record`] hands nothing over when neither the text nor the
    /// mappings changed.
    ///
    /// **Six generators, one pass, one [`Facts`] per source file — and one generated document
    /// per *body* out the far end.** The schema — `db/*schema.rb` and `db/*structure.sql`, which
    /// are one generator because they end at one syntax type — the model macros, the
    /// annotations somebody wrote by hand, the mailer and job conventions, the routing DSL and
    /// `delegate` all end at a [`Facts`], and a file that feeds two of them has both merged into
    /// the one table its URI names. The last is a second *phase* and runs last, because what it
    /// derives from is what the other five said.
    ///
    /// The cut into documents is [`Facts::split`], and it happens **after** all of that, at the
    /// render step: precedence is settled on the whole file's facts and only the rendering is
    /// partitioned, so no generator knows about it. It is there because
    /// [`synthesized::Synthesized::record`] is charged per declaration it re-indexes, which makes
    /// the document the unit of invalidation — 2.2 s of a 2.49 s keystroke on discourse when a
    /// schema is one document, and one table's worth when it is one per body.
    ///
    /// A project with none of the three pays one pass over its own documents for the guarantee.
    /// A project whose files are deliberately outside `index.include` pays the same: not
    /// indexed and not read are the same sentence, and this is not the place to overrule the
    /// configuration.
    pub(super) fn synthesize(&mut self) {
        let started = Instant::now();
        // **Two questions, and they must stay two.** "Would the walk produce the same
        // projection" and "has a file some generator reads changed" are different questions,
        // and answering the first with the second is right about the generators and says
        // nothing about the walk: a keystroke in
        // a model file changes what the file *says* and not what the projection *is*, so the
        // generators have to run and the walk does not. On discourse the walk is 70 ms of
        // every such keystroke and on lobsters 22.
        // Both questions are asked of the projection already held, and the borrow of it has to
        // end before either branch below writes to `self` — so they are answered first and
        // acted on after. `None` is the first pass, where nothing is known about either.
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
            return;
        }
        let walked = Instant::now();
        let context = if same {
            // The projection is the one already held. **Taken and not cloned**: every path out
            // of this function from here on ends at `remember`, which puts it back. The
            // per-document contributions stay where they are — the gate has just proved that
            // every document it could have asked about contributes what it contributed last
            // time, which is the same sentence as "the memo is still good".
            self.generated_from
                .take()
                .expect("`context_would_be_the_same` answered about a projection it holds")
        } else {
            self.walk()
        };
        let walk = walked.elapsed();
        // Only worth asking after a walk. Where the walk was reused the projection is equal by
        // construction, and the other half of this gate is what has just said no.
        if !same && self.pass_would_repeat_itself(&context) {
            // Not a cache and not an incremental pass: the projection above is rebuilt
            // in full every time and compared, so what is skipped is only work whose *inputs*
            // are provably the same as last time's. See [`Analysis::pass_would_repeat_itself`].
            tracing::debug!(
                "nothing the pass reads changed, in {:.2?} ({walk:.2?} of it the walk, {} touched)",
                started.elapsed(),
                self.touched.len()
            );
            // The walk just ran, so the per-document contributions are fresh and the `Context`
            // they belong to is the one already held — `previous == context` is what got us
            // here. `Analysis::walk` has already put them back, which is what lets the *next*
            // keystroke take the gate above: a document whose contribution moved without moving
            // the merged answer would otherwise re-fail the cheap comparison for the rest of the
            // session.
            self.touched.clear();
            return;
        }
        self.passes += 1;
        // Every file any generator is about to open, read and parsed here and only here — and
        // only where the text has moved since the last pass.
        self.refresh_sources(&context);
        if context.is_empty(&self.knowledge) && self.generated.is_empty() {
            // The view context is rebuilt here too, and with no sources rather than not at all:
            // its helpers half is a projection of the walk above and costs nothing, and a
            // workspace that has just had its last macro deleted has to *lose* the exports it had
            // rather than keep them.
            self.views = self.view_context(&context);
            self.remember(context);
            return;
        }

        let mut generated = knowledge::Declared::new();
        self.views = self.view_context(&context);
        // **Every registered module, through the three phases**, and core knows nothing else about
        // the order: what a module's own generators owe each other is the module's business, and
        // what they owe *another* module's is what the phases are. Taken out and put back for
        // `refresh_sources`' reason — the view below borrows the rest of `self` while a module
        // writes to itself.
        let mut knowledge = std::mem::take(&mut self.knowledge);
        let counted = {
            let context = &context;
            let declaring = knowledge::Declaring {
                context,
                features: self.workspace.features(),
                text: &|uri| self.with_text(uri, |text| text.text().to_owned()),
                caption: &|uri| self.workspace_relative(uri),
                own: &|uri| self.is_own_code(uri),
                declares: &|wanted| self.declaring_documents(wanted),
            };
            let mut counted: knowledge::Counted = Vec::new();
            for module in knowledge.modules_mut() {
                counted.extend(module.conjure(&declaring, &mut generated));
            }
            for module in knowledge.modules_mut() {
                counted.extend(module.declare(&declaring, &mut generated));
            }
            // Nothing after this may declare, which is what phase three means.
            for module in knowledge.modules_mut() {
                counted.extend(module.derive(&declaring, &mut generated));
            }
            counted
        };
        self.knowledge = knowledge;

        let mut kept: HashSet<String> = HashSet::new();
        let mut documents = 0;
        for (uri, facts) in generated.into_values() {
            // **Split, then rendered, and one document per body out the other end.** The split
            // is of the rendering and never of the generation: every generator above has spoken
            // and every collision is settled, so nothing here can re-decide a rank — see
            // [`Facts::split`]. What it buys is the unit of invalidation, because
            // [`Synthesized::record`] is charged per declaration it re-indexes: a column that
            // changed type re-indexes one table rather than every table in the schema.
            //
            // Rendered here and only here: every span in a document is computed against the
            // text as it is finally written, so no generator's offsets have to be shifted by
            // the length of another's.
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
            documents += self
                .synthesized
                .record(&mut self.graph, &mut self.types, &uri, parts)
                .len();
            kept.insert(uri.as_str().to_owned());
        }
        self.forget_stale(&kept);

        // Debug rather than info: this runs before every resolve, so it is one line per settle
        // and it would drown the log of an ordinary editing session. It is the only place these
        // numbers exist, and they are what a report about this feature needs.
        // `lists` is what the walk handed the generators; the counts after it are what they made
        // of it. The two together are the whole of "did a generator run, and on what" — which is
        // the question a switched-off generator has to be answerable in, and the question nobody
        // could ask before, because the only numbers here were the outputs.
        let lists = asked_for(context.documents.iter());
        let declared = counted
            .iter()
            .map(|(what, how_many)| format!("{how_many} {what}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::debug!(
            "{} files declare {declared}, into {documents} generated documents, from {lists}, in \
             {:.2?} ({walk:.2?} of it the walk)",
            kept.len(),
            started.elapsed()
        );
        self.remember(context);
    }

    /// Whether the generators would write exactly what they wrote last time.
    ///
    /// `settle` calls this pass before every `resolve` and a forced settle sits in front of
    /// every graph-reading request, so without this gate one keystroke in a file with no macro
    /// in it pays for a whole-workspace regeneration: **63 ms on discourse**, in front of
    /// completion's own 10 — measured 2026-09-16 as a pass whose every one of 977 generated
    /// documents came out byte-identical, which is what a file no generator reads produces. What
    /// makes the number an order of magnitude larger is a document whose text really moved:
    /// [`synthesized::Synthesized::record`] re-indexing one model's 18 KB of RBS into a settled
    /// graph is 148 ms on its own, and the schema's 450 KB is 2.2 s. `synthesized.md` has the
    /// breakdown.
    ///
    /// **Two questions, and both have to be no.** The projection above is what the generators
    /// are handed, so an equal `Context` means equal arguments — that half catches a new class
    /// being defined somewhere else, which is what makes a naive "did *this* document declare
    /// anything" test unsound: `has_many :widgets` declines until some other file writes
    /// `class Widget`. The second half is the files themselves, because a `Context` says which
    /// documents a generator opens and never what is in them: `has_many :comments` becoming
    /// `has_many :notes` is one document on one list either way.
    ///
    /// What is skipped is the reading, parsing, rendering and recording of every listed file.
    /// The **walk is not skipped here**, because building the evidence for *this* gate is the
    /// walk — 70 ms of a 132 ms pass on discourse, measured 2026-09-16, and 275 ms of a 340 ms
    /// pass before [`Analysis::walk`] started keeping what each document contributed.
    /// [`Analysis::context_would_be_the_same`] runs before this one and needs no walk at all.
    /// This gate stays because it is strictly wider — it catches a document whose contribution
    /// moved without moving the merged answer, which a per-document comparison cannot — and it
    /// is only ever asked when a walk really happened, because an equal projection makes the
    /// comparison in it trivially true.
    fn pass_would_repeat_itself(&self, context: &Context) -> bool {
        let Some(previous) = self.generated_from.as_ref() else {
            return false;
        };
        // A bulk index — the workspace walk, a gem batch, a watched file, a rebuild — says
        // nothing about *which* documents moved, so it is never skipped. Only the buffer path
        // reports one document, which is the keystroke path this gate is for.
        if self.touched_all || previous != context {
            return false;
        }
        self.generators_would_repeat_themselves(previous)
    }

    /// Whether nothing a generator *reads* has changed — the half of the gate that is about
    /// files rather than about the projection, and a function of its own because both gates ask
    /// it.
    ///
    /// Two clauses. A `Context` says which documents a generator opens and never what is in
    /// them, so `has_many :comments` becoming `has_many :notes` is one document on one list
    /// either way and the file being touched at all is the whole answer. And every file it read
    /// is still there, unchanged — **not belt and braces**: the pass's own claim is that the
    /// answer is a function of what is on disk, and it earns that by re-reading everything, so
    /// a `git checkout` that deletes a `db/schema.rb` has to stop the columns answering at the
    /// very next settle, before any watcher notification arrives. A gate that trusted
    /// notifications alone would keep them. One `stat` per file read is what buys the guarantee
    /// back, and the parse memo rests on the same `stat`.
    fn generators_would_repeat_themselves(&self, previous: &Context) -> bool {
        if self
            .touched
            .iter()
            .any(|uri| previous.read_by_a_generator().any(|read| read == uri))
        {
            return false;
        }
        self.stamps.iter().all(|(path, was)| stamp_of(path) == *was)
    }

    /// Whether the walk would produce the `Context` already held — the gate that runs *before*
    /// the walk rather than after it.
    ///
    /// [`Analysis::pass_would_repeat_itself`] compares the whole merged `Context`, which is what
    /// the walk **builds** — so it can only be asked after paying for the walk, and on discourse
    /// that is 70 ms. This asks the same question of **one document**, and pays for one.
    ///
    /// **It must not borrow the other gate's file clause.** "Is a touched file on a generator's
    /// list" is about what a generator *reads* and says nothing at all about what the walk
    /// *builds*; asked on its own, this answers yes for the commonest edit in a Rails
    /// application and the projection is reused while the generators run — 70 ms of every
    /// keystroke in a discourse model file, and 22 of one in a lobsters model file.
    ///
    /// **Three things make one document's `Contribution` enough**, and each of them is a
    /// property something else in this module had to be given:
    ///
    /// 1. The merged `Context` is a function of the *set* of contributions and not of the order
    ///    the graph hands them over in. [`Context::absorb`] and [`Context::settle`] are where
    ///    that is paid for, and two fields had to change to make it true. [`Analysis::walk`]
    ///    rests on the same property, for the same reason read the other way round.
    /// 2. A document nothing re-indexed cannot have contributed anything different. Only
    ///    `Analysis::index_buffer` names a document; every bulk route sets `touched_all` and is
    ///    refused here, which is the other gate's own narrowing re-used rather than restated.
    /// 3. A document the walk does not *visit* is refused outright, because
    ///    [`Analysis::bundle_namespaces`] reads every definition in the graph — an `.rbs` in the
    ///    project's own `sig/` contributes nothing to the walk and can still move a `Context`.
    ///
    /// The one input that is **not** a projection of the graph is asked rather than inferred:
    /// `db/*structure.sql`, which is not a document at all, so no `Contribution` can reach
    /// one. It is a `read_dir` and not a walk. What is deliberately *not* asked here is
    /// the stamp of every file a generator read — that is a question about the files and
    /// belongs to [`Analysis::generators_would_repeat_themselves`], which is asked beside this
    /// one rather than inside it.
    fn context_would_be_the_same(&self, previous: &Context) -> bool {
        if self.touched_all {
            return false;
        }
        let filters = Filters::new(&self.knowledge);
        for uri in &self.touched {
            // One `let … else` and not two, because "the graph has no such document" and "the
            // walk does not visit it" are the same answer here: no contribution was stored, so
            // there is nothing to compare against and the pass has to run. Asking them
            // separately would also make the comparison below unsound on its own — two `None`s
            // are equal, and a document with no entry would compare *the same* as one that
            // contributed nothing.
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
        // Not a projection of the graph and therefore not covered by anything above — a file
        // nothing indexed is one no `Contribution` can be about, which is what
        // `Knowledge::discover` exists for.
        let reading = knowledge::Reading {
            root: self.workspace.root(),
            admits: &|path| self.workspace.admits(path),
            features: self.workspace.features(),
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

    fn remember(&mut self, context: Context) {
        self.stamps = context
            .read_by_a_generator()
            .filter_map(|uri| DocUri::from_uri_str(uri)?.to_path())
            .map(|path| {
                let stamp = stamp_of(&path);
                (path, stamp)
            })
            .collect();
        self.generated_from = Some(context);
        self.touched.clear();
        self.touched_all = false;
    }

    /// Everything the generators need from the graph, in one pass over the user's code.
    ///
    /// One pass and not one per generator, because each of them wants a different projection of
    /// the same documents and every projection is a filter over what indexing already recorded.
    /// The projections are [`WANTS`]; the two that no generator reads yet — `modules` and
    /// `superclasses` — are collected here because they cost the same loop, and because a
    /// projection added later is a second loop nobody notices.
    /// The walk's answer on its own, **projected cold**, for the tests that assert on what the
    /// walk builds rather than on what a generator did with it.
    ///
    /// The memo is dropped first, so this is the walk with nothing held — which is exactly the
    /// answer a memoised walk has to equal, and what lets a test assert that it does.
    #[cfg(test)]
    pub(super) fn context(&mut self) -> Context {
        self.contributions.clear();
        self.walk()
    }

    /// The same walk, keeping every document's projection so the next one need not build it
    /// again.
    ///
    /// **The loop is incremental and nothing after it is.** [`Analysis::contribution`] was the
    /// walk's whole cost, and for a document rubydex has not re-indexed its answer is the answer
    /// it gave last time. What a memo cannot touch is everything below the loop:
    /// [`Context::absorb`] is a fold with no inverse, and [`includers_of`] and
    /// [`Analysis::bundle_namespaces`] are folds over the whole projection rather than
    /// per-document reads — so what is left is a floor rather than a curve.
    ///
    /// **Measured 2026-09-16**, release build, median of three keystrokes that each add a *new
    /// class name* to a model file, which is what makes the projection move and so what makes
    /// the walk run at all:
    ///
    /// | | lobsters, 9,015 documents | discourse, 25,900 documents |
    /// | --- | ---: | ---: |
    /// | the walk, projecting every document | 52 ms | 275 ms |
    /// | the walk, holding them | **22 ms** | **70 ms** |
    /// | the pass around it | 64 ms → **34** | 340 ms → **132** |
    ///
    /// **The walk is over half of that pass, which is why the pass halves with it.** A new class
    /// name moves the projection and almost never moves a generated document's *text*, so the
    /// generators all run and every one of discourse's 977 generated documents comes out
    /// byte-identical: `Synthesized::record` hands nothing over and the walk is what is left.
    /// The other shape of keystroke is the opposite and this does not touch it — an `attribute`
    /// added to a model re-declares one document, and re-indexing that one document's 18 KB of
    /// RBS into a settled graph is 148 ms of a 224 ms pass while the walk does not run at all.
    ///
    /// **The key is "has rubydex re-indexed this document", and it must not be a file stamp.**
    /// [`Analysis::contribution`] reads the graph and never a file, which is the bound this whole
    /// pass inherits; keying its memo on a `stat` would break that bound and cost one syscall per
    /// document per settle, which is a large fraction of what the memo is here to remove.
    /// `self.touched` and `self.touched_all` already carry the right question — so this is not a
    /// second mechanism beside [`Analysis::context_would_be_the_same`], it is the same one asked
    /// of every document rather than of the one a keystroke named, and it needs no invalidation
    /// rule that did not already exist.
    ///
    /// **The one input that is not a document is named rather than inferred.** `is_own_code` and
    /// `is_generator_source` read `engine_prefixes`, which is a property of the bundle and not of
    /// the graph, so gem discovery drops the whole map where it writes them: a memo that quietly
    /// survived that would answer with a former engine's `app/` still admitted.
    fn walk(&mut self) -> Context {
        self.walks += 1;
        let started = Instant::now();
        // The invalidation, and both halves of it are the gate's own narrowing re-used. A bulk
        // route says nothing about *which* documents moved, so the whole map goes; only
        // `Analysis::index_buffer` names one, and that is the keystroke path.
        if self.touched_all {
            self.contributions.clear();
        } else {
            for uri in &self.touched {
                self.contributions.remove(&UriId::from(uri.as_str()));
            }
        }
        let filters = Filters::new(&self.knowledge);
        let mut context = Context::new(&self.knowledge);
        // `(the body that wrote the `include`, the constant it spelled)`, resolved after the
        // loop — see [`Context::includers`].
        let mut included: Vec<(String, String)> = Vec::new();
        // Drained rather than written through, so a document the graph no longer holds takes its
        // entry with it. Everything still in `held` when the loop ends is a document that was
        // removed, and it is dropped.
        let mut held = std::mem::take(&mut self.contributions);
        let mut contributions: HashMap<UriId, Contribution> = HashMap::with_capacity(held.len());
        let mut projecting = std::time::Duration::ZERO;
        let mut projected = 0_usize;
        for (uri_id, document) in self.graph.documents() {
            let contribution = match held.remove(uri_id) {
                Some(contribution) => contribution,
                None => {
                    // Timed around the miss and not around the loop, so the line below costs two
                    // clock reads per *re-projected* document rather than two per document: on a
                    // keystroke that is two, and on the cold walk it is 0.6% of a walk that has
                    // nothing held anyway.
                    let at = Instant::now();
                    let fresh = self.contribution(document, &filters);
                    projecting += at.elapsed();
                    projected += 1;
                    match fresh {
                        Some(contribution) => contribution,
                        // Not memoised, and absence is why: a document the walk does not
                        // **visit** must have no entry, because that is the case the gate cannot
                        // reason about. See [`Analysis::context_would_be_the_same`].
                        None => continue,
                    }
                }
            };
            context.absorb(document.uri(), contribution.clone(), &mut included);
            // Every document the walk **visits** gets an entry, including one that contributes
            // nothing.
            contributions.insert(*uri_id, contribution);
        }
        let visited = contributions.len();
        self.contributions = contributions;
        let absorbed = started.elapsed();
        // The one place a gem's own names are read. A concern's includer is very often a class
        // in a gem — `ActiveRecord::Base` includes `ActiveModel::API` — so the chain this walks
        // runs through names no file of the user's writes.
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
        // document, so no contribution can be about it and no list can name it. Taken out and put
        // back because the discovery reads the workspace while the module writes to itself.
        let mut knowledge = std::mem::take(&mut self.knowledge);
        {
            let reading = knowledge::Reading {
                root: self.workspace.root(),
                admits: &|path| self.workspace.admits(path),
                features: self.workspace.features(),
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
        // **What only the whole walk decides**, and each module's own: whether a class is a model
        // is a question about the chain above it, and the file that defines one joins the list
        // that opens it. Before the feature gate below, so a switched-off list drops whatever
        // joined it here too.
        for module in self.knowledge.modules() {
            module.after_the_walk(&mut context);
        }
        // **The same gate as `contribution`'s, applied where that one cannot reach.** The hook
        // above is where a membership is decided *after* the walk rather than during it, so a
        // document that writes no macro at all lands on the model list without ever passing the
        // per-document test — and a `[rails] models = false` that only filtered the rows would go
        // on reading every model in the project. The loop's gate is kept because it is the cheap
        // half: it is what stops the predicates running per document per switched-off list.
        context
            .documents
            .retain(|list, _| self.knowledge.wanted(*list, self.workspace.features()));
        // One walk rather than three lookups, because `Graph::get` reads the
        // **declarations**, which `Resolver::resolve` builds and this
        // pass runs before — so it answers a settle late, and over lobsters it holds 2,431
        // entries against 174,919 definitions at the moment it is asked. The definitions are
        // there; only the index over them is not.
        let at = Instant::now();
        let namespaces = self.bundle_namespaces(&wanted_namespaces(&context, &self.knowledge));
        let bundled = at.elapsed();
        for (name, module) in namespaces {
            context.namespaces.declare(name, module);
        }
        // What a directory conjures is only conjured where **nothing else declares the name** —
        // a `user.rb` beside the `user/` directory, a `module Chat` in a plugin, a gem. So the
        // filter runs here, after the application's own names and the bundle's have both been
        // recorded, and never in the loop above where neither set is complete.
        //
        // The survivors are then declared, which is what makes every prefix of a conjured name
        // spellable: `Chat::Thread::Policy` needs `Chat::Thread`, and `Chat::Thread` is either
        // declared already or is itself in this map, because the module that conjured it
        // answers the whole chain rather than its last link.
        // And what only the **bundle's** answer decides, each module's own again: a namespace a
        // directory conjures is only conjured where nothing else declares the name, and which
        // framework classes may be written onto is exactly which of them the bundle holds.
        let mut conjured: Vec<String> = Vec::new();
        for module in self.knowledge.modules() {
            conjured.extend(module.after_the_bundle(&mut context));
        }
        for name in conjured {
            context.namespaces.declare(name, true);
        }
        context.settle();
        // The split, and it is the instrument the memo is answerable in: `projected` is how many
        // of the visited documents the memo could not answer for, and `projecting` is what they
        // cost — 866 and 0.2 ms on a discourse keystroke, which is the 865 `.rbs` the walk
        // refuses and therefore never holds, plus the one document that was edited. The three
        // figures after it are the folds no memo reaches, 32 + 9 + 18 ms, which is the floor.
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
    /// The loop body of [`Analysis::walk`], made callable for a single document so the gate can
    /// re-project one. It reads the graph and never a file, which is the bounding rule this whole pass inherits.
    ///
    /// **`None` is load-bearing rather than tidy.** A document this declines still has
    /// definitions, and [`Analysis::bundle_namespaces`] reads *every* definition in the graph —
    /// so an `.rbs` in the project's own `sig/`, or a gem file the user opened and typed in,
    /// can move a `Context` while contributing nothing here. The gate refuses a touched
    /// document that is not visited for exactly that reason, which is what lets the gate and the
    /// memo both stop at this function's own outputs.
    fn contribution(&self, document: &Document, filters: &Filters) -> Option<Contribution> {
        // Ruby only, and the exclusion is a real one rather than a tidiness: an `.rbs` file
        // has `def`s and doc comments like any other document, so a `@return` tag in one
        // put it on the annotated list — where it was handed to a reader that parses Ruby.
        // The generated RBS then failed to parse, which is the gate in
        // `Synthesized::record` catching a bug rather than a bug not existing.
        // A Rails engine's `app/` is read too, and only there does this differ from
        // `is_own_code`. Which of the six lists an engine's document may go on
        // is `Wants::engines` below, so the difference is one flag per list and never a
        // second loop.
        let own = self.is_own_code(document.uri());
        // **Three widths, and only the widest one is new.** `own` is the user's code; `generator`
        // adds a Rails engine's `app/`, which is [`Analysis::is_generator_source`]'s whole
        // purpose; and everything else left after the `.rbs` test is a gem's own Ruby, which
        // exactly one list admits — see [`Wants::gems`]. A document outside all three does not
        // exist: the graph holds nothing that is not one of them.
        let generator = own || self.is_generator_source(document.uri());
        if document.uri().ends_with(".rbs") {
            return None;
        }
        let mut contribution = Contribution::default();
        let mut tagged = false;
        let rows = self.knowledge.wants().len();
        let mut defines = vec![false; rows];
        let mut declares = vec![false; rows];
        // One per **module** and not per row: `Wants::inherits` asks the module's own convention,
        // and two rows of one module share the answer while two modules never do.
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
                    // The two entry-point conventions, asked once and for both callers: the
                    // module's own `claims_by_ancestry`, so which documents are worth opening
                    // and which classes are worth reading cannot disagree. `include` only — an
                    // `extend Sidekiq::Worker` puts the hook nowhere and is not the shape.
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
                    // Asked of every registered module and of none of them by name: whether a
                    // class with this superclass and these mixins is one of *yours* is the one
                    // predicate in the table whose question core cannot ask.
                    for (found, module) in inherits.iter_mut().zip(self.knowledge.modules()) {
                        *found |= module.claims_by_ancestry(superclass.as_deref(), &mixins);
                    }
                    // The includer edge, read here for `superclasses`' reason: an
                    // `include` is recorded on the definition at index time, so which
                    // module it names is a question about the graph rather than about the
                    // file the macro is written in. Spelled as written and resolved after
                    // the loop, because the set it is resolved against is only complete
                    // when every document has been seen.
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
                    // The sixth predicate, asked of the **last segment** and so of the name
                    // rubydex interned rather than of the joined one: `module ClassMethods` is
                    // one string compare per module definition, and the walk up its parents is
                    // the expensive half that only a match may reach.
                    if let Some(interned) = self.graph.names().get(module.name_id()) {
                        for (found, names) in declares.iter_mut().zip(&filters.modules) {
                            *found |= names.contains(interned.str());
                        }
                    }
                    if let Some(name) = self.qualified_name(module.name_id()) {
                        // A module's own `include`s, for the same edge: a concern that
                        // includes a concern is what the closure below walks through.
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
                // A YARD tag is a comment above a `def`, and indexing already carried the
                // comments into the graph — so which files are worth parsing for tags is a
                // question the graph answers without opening one. The name is read for
                // `def self.table_name_prefix`, which is the one thing on any of these lists
                // that a file *defines* rather than calls or references.
                // **Skipped outright for a gem**, which is most of what widening the walk
                // would otherwise have cost: a method definition is the commonest thing in any
                // document, and both questions asked here — a YARD tag and
                // `def self.table_name_prefix` — are about lists no gem may join.
                Definition::Method(method) if generator => {
                    if !tagged {
                        tagged = method.comments().iter().any(|comment| {
                            comment.string().contains("@return")
                                || comment.string().contains("@param")
                        });
                    }
                    // The lookup is a string rather than a hash of one, because rubydex
                    // records a `def` under its name *and* its parameter list —
                    // `table_name_prefix()` — and a `WANTS` row spelling that out would
                    // stop matching silently if the rendering ever moved. It costs one
                    // lookup per **singleton** method, which is the narrow half of the
                    // definitions.
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
        // The last segment of a constant reference, which is what `Struct` is in every
        // spelling of it: `Struct`, `::Struct`, and — the one that matters for a reference
        // written inside `module Admin` — the same `Struct` with a nesting rubydex records
        // separately from the name.
        let mentions = |names: &[StringId]| {
            !names.is_empty()
                && document
                    .constant_references()
                    .iter()
                    .filter_map(|id| self.graph.constant_references().get(id))
                    .filter_map(|reference| self.graph.names().get(reference.name_id()))
                    .any(|name| names.contains(name.str()))
        };
        let features = self.workspace.features();
        for ((((at, want), names), constants), (defined, declared)) in self
            .knowledge
            .wants()
            .iter()
            .enumerate()
            .zip(&filters.calls)
            .zip(&filters.constants)
            .zip(defines.into_iter().zip(declares))
        {
            let module = self.knowledge.behind(at);
            // **The one place a switched-off generator is switched off**, and it is this one
            // because the rows are the one table that decides which documents any generator ever
            // sees: an empty list is a generator that does nothing, with no removal path to
            // write and nothing downstream to teach. A configuration reload re-indexes the
            // workspace anyway (`analysis/mod.rs`), so declarations a generator produced before
            // it was turned off are dropped by that rebuild rather than by anything here.
            if !module.wanted(want.list, features) {
                continue;
            }
            if !own && !(if generator { want.engines } else { want.gems }) {
                continue;
            }
            // Once per document per list however many of the three tests say yes, which is
            // the whole reason they are one loop: a file with a `sig` *and* a `@return` tag
            // is one entry on the annotated list, and two would generate it twice.
            if calls(names)
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
        // **Every module's own half, and it is a fold over what is already in hand** — the names
        // this document declares, the superclass and the `include`s beside each, and the URI.
        // Nothing here goes back to the graph, which is what keeps a module's projection from
        // costing a second walk; the one thing that cannot be folded is a byte offset, and
        // `Seen::confirming` is the closure that answers those.
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

    /// What the **whole graph** — the bundle, Ruby's own signatures, everything indexed — says
    /// each of `wanted` is: a class, or a module.
    ///
    /// The one whole-graph lookup, and it reads the **definitions** rather than the declarations. That
    /// is not an implementation detail: `Graph::get` is the map `Resolver::resolve` builds, and
    /// this pass runs immediately before the resolve, so it answers about the *previous* settle.
    /// Measured over lobsters, at the moment this is called: 4 declarations against 1,968
    /// definitions on the cold settle, **2,431 against 174,919** on the settle the bundle lands,
    /// and 144,299 only on the settle after that. Every gem class is invisible for two settles
    /// and then appears, which is a different answer on each of the first three passes over an
    /// unchanged workspace.
    ///
    /// **A document this crate generated is never read**, and the filter is a property of the
    /// scheme rather than of a list, which is what a non-`file:` URI is for. Without
    /// it the pass would read its own previous output and the second settle could not equal the
    /// third.
    fn bundle_namespaces(&self, wanted: &BTreeSet<String>) -> Vec<(String, bool)> {
        // Hashes of the last segments, so a definition nobody asked about costs one compare and
        // never a walk up its parents.
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
            // A name two files spell differently is a `module` only if every one of them says
            // so: opening a body for a name somebody else declares as a class is the spelling
            // measured at 234 chatwoot positions, and `false` writes nothing.
            found
                .entry(spelled)
                .and_modify(|already| *already &= module)
                .or_insert(module);
        }
        found.into_iter().collect()
    }

    /// A class or module's name with its nesting, spelled the way Ruby writes it.
    ///
    /// Three spellings reach the same place and rubydex records them differently: `class Story`
    /// carries no parent and no nesting, `class ::Tag` says top level outright, and both
    /// `module Admin; class Story` and `class Admin::Post` are nested — the first in the
    /// lexical nesting, the second in the name's own parent. Walking both links is what makes
    /// this answer the same string a `class_name: "Admin::Setting"` would have to match.
    ///
    /// `None` for a singleton's attached name, which is not a constant anybody writes.
    /// Where each of `conjured`'s names is written on the line that confirmed the directory.
    ///
    /// The generated `module Api` has no member to hang a place on, so its place is the `Api` of
    /// the `class Api::V1::Foo` this document writes — *the source this generator read*, which
    /// is the rule every other span in this pass obeys, reached through a body rather than
    /// through a member.
    ///
    /// **Read out of the declaration the chain was confirmed on, and never out of the document
    /// at large.** A file that names `Api` inside a method body names a *use*, and offering one
    /// as a place would be a line nothing declared. So the window is that declaration's own
    /// construct, and within it the **earliest** reference to a name is the one the path wrote:
    /// a superclass and a body both come after it. `declared` is the parent the caller already
    /// matched to confirm the chain, passed rather than re-derived so that this function has no
    /// arm for a chain it cannot have been called with.
    ///
    /// Matched by **name** rather than by position. A nested spelling — `module Api` around a
    /// `class V1::Foo` — writes fewer references than the path has segments, and pairing those
    /// positionally would put `Api`'s place on `V1`'s line. A conjured name with no reference in
    /// the window is simply absent here, and [`Analysis::autoloaded_declarations`] then writes
    /// its body with no span, which is the no-mapping-no-place rule unchanged. That case is not
    /// hypothetical and it costs nothing: a document that spells the nesting out declares every
    /// name in the chain, so [`Context::autoloaded`]'s filter drops all of them anyway.
    ///
    /// `full` is the namespace the declaration opens — `Api::V1::Accounts` of a
    /// `class Api::V1::Accounts::CredentialsController` — and `selection` the one segment. The
    /// class's own name is deliberately outside it: what a reader is being sent to is where the
    /// namespace is written, and the constant that happens to live in it is a different name.
    fn confirming_path(
        &self,
        document: &Document,
        declared: &str,
        names: &BTreeSet<&str>,
    ) -> HashMap<String, At> {
        // The first declaration `declared` names, in document order: a second one in the same
        // file is the same namespace confirmed twice, and the earlier line is the one a reader
        // sent here would expect to land on.
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
                // The path's own segment is the **earliest** occurrence inside the construct,
                // and earliest is not first: rubydex indexes a `class Mod::A < Mod::B`'s
                // superclass before the name, so the first reference handed over here is the
                // one written second. `Ord::min` rather than a comparison, because a file
                // cannot write a superclass before the name it is a superclass of and an arm
                // for that would be an arm no fixture can reach.
                segments
                    .entry(spelled)
                    .and_modify(|held| *held = (*held).min(at))
                    .or_insert(at);
            }
            // One `full` serves every segment, because every one of them is part of the one
            // path: it opens at the outermost and closes at the innermost this file wrote.
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
    /// The difference from [`Self::qualified_name`] is the whole reason both exist. That one
    /// answers "which class is this", which needs the nesting; this one answers "what does this
    /// line say", which must not have it — `class Story < ApplicationRecord` inside
    /// `module Admin` names `ApplicationRecord` and not `Admin::ApplicationRecord`, and Ruby
    /// would resolve it to the top-level one. A reference is not a definition and cannot borrow
    /// a definition's nesting.
    fn spelled_name(&self, name_id: &NameId) -> Option<String> {
        let name = self.graph.names().get(name_id)?;
        let own = self.graph.strings().get(name.str())?.as_str();
        match name.parent_scope() {
            ParentScope::Some(parent) => Some(format!("{}::{own}", self.spelled_name(parent)?)),
            _ => Some(own.to_owned()),
        }
    }

    /// Read and parse every file the generators are about to open — and only the ones that moved.
    ///
    /// **The parse memo.** Without it, one keystroke in a model file re-reads and re-parses every
    /// file on every list, because one of them changed: on a large application that is thousands
    /// of files read and parsed to learn what all but one of them said last time. The memo is
    /// [`Cached`], keyed by document URI and valid exactly as long as [`Fresh`] says the text has
    /// not moved.
    ///
    /// **The disk guarantee is why this looks at the disk rather than at a notification.** The
    /// pass gate claims the answer is a function of what is on disk and earns it by re-reading
    /// everything; a memo is precisely the thing that stops doing that, so what replaces the
    /// re-read is the `stat` — one per wanted file, against the parse it takes away. A
    /// `git checkout` that rewrites a model still takes effect at the very next settle, with no
    /// watcher involved.
    ///
    /// **Two readers are deliberately not memoised**, and it is the same sentence for both:
    /// neither is a function of its own text. `structs::read` takes the `Context`'s namespaces,
    /// and a routes reader takes a prefix that another file's parse computed. A memo for
    /// either would need a second input compared, which is exactly the design this one rejects.
    /// Which document declares each of these namespaces, for the one generator that has to open a
    /// file it was not handed.
    ///
    /// [`Analysis::bundle_namespaces`]' walk, asked for a URI rather than for a kind, and bounded
    /// the same way: the last segments are hashed first, so a definition nobody asked about costs
    /// one compare and never a walk up its parents. A name several files reopen answers with the
    /// **first in URI order**, which is arbitrary and deterministic — what a module declares in
    /// its own body is what this reads, and a module spread over two files is one that would need
    /// both read whichever were picked. Six corpora hold none.
    ///
    /// A generated document is skipped, for `bundle_namespaces`' reason: this pass is what wrote
    /// those, and reading its own output back is how a fact starts deriving from itself.
    fn declaring_documents(&self, wanted: &BTreeSet<String>) -> BTreeMap<String, DocUri> {
        let last: HashSet<StringId> = wanted
            .iter()
            .map(|name| StringId::from(name.rsplit("::").next().unwrap_or(name)))
            .collect();
        let mut found: BTreeMap<String, (String, DocUri)> = BTreeMap::new();
        for definition in self.graph.definitions().values() {
            let Definition::Module(module) = definition else {
                continue;
            };
            let name_id = module.name_id();
            let Some(name) = self.graph.names().get(name_id) else {
                continue;
            };
            if !last.contains(name.str()) {
                continue;
            }
            let Some(document) = self.graph.documents().get(definition.uri_id()) else {
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
            let Some(uri) = DocUri::from_uri_str(document.uri()) else {
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

    /// What a template's implicit receiver can answer, asked of every module.
    ///
    /// The first answer wins and there is only ever one: this is not a declaration, so there is
    /// no rank to settle it with, and two modules both claiming to know what a bare word in a
    /// template means would be a design question rather than a precedence one.
    fn view_context(&self, context: &Context) -> Views {
        let declaring = knowledge::Declaring {
            context,
            features: self.workspace.features(),
            text: &|uri| self.with_text(uri, |text| text.text().to_owned()),
            caption: &|uri| self.workspace_relative(uri),
            own: &|uri| self.is_own_code(uri),
            declares: &|wanted| self.declaring_documents(wanted),
        };
        self.knowledge
            .modules()
            .find_map(|module| module.views(&declaring))
            .unwrap_or_default()
    }

    fn refresh_sources(&mut self, context: &Context) {
        // Taken out and put back, because the closures below borrow the rest of `self` — the open
        // buffers, the workspace root — while a module writes to itself. The same move
        // `Analysis::walk` makes with the contributions, for the same reason.
        let mut knowledge = std::mem::take(&mut self.knowledge);
        let sources = knowledge::Sources {
            context,
            features: self.workspace.features(),
            fresh: &|uri| self.freshness(uri),
            text: &|uri| self.with_text(uri, |text| text.text().to_owned()),
            caption: &|uri| self.workspace_relative(uri),
        };
        for module in knowledge.modules_mut() {
            module.refresh(&sources);
        }
        self.knowledge = knowledge;
    }

    /// How the text of one document is identified for a module's own memo — see
    /// [`knowledge::Fresh`] for why the two authorities cannot share one answer.
    fn freshness(&self, uri: &DocUri) -> knowledge::Fresh {
        match self.open.get(uri) {
            Some(open) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                open.text.text().hash(&mut hasher);
                knowledge::Fresh::Buffer(hasher.finish())
            }
            None => knowledge::Fresh::Disk(uri.to_path().as_deref().and_then(stamp_of)),
        }
    }

    /// Drop declarations a source no longer makes.
    ///
    /// Scoped to the sources *this pass* wrote last time, and the scoping is the point: the
    /// side table is a table, not this function's private state, and a pass that pruned by
    /// "everything I did not just write" would delete anything else that ever records into it —
    /// which is not hypothetical: this module's own tests play a generator and record from a
    /// file no rule here recognises.
    fn forget_stale(&mut self, kept: &HashSet<String>) {
        let stale: Vec<DocUri> = self
            .generated
            .iter()
            .filter(|source| !kept.contains(*source))
            .filter_map(|source| DocUri::from_uri_str(source))
            .collect();
        for source in stale {
            self.synthesized.forget(&mut self.graph, &source);
        }
        self.generated = kept.clone();
    }

    /// How a file inside the workspace should be spelled to a person: `db/animals_schema.rb`.
    ///
    /// **A gem's file is captioned from the gem directory down**, and that stopped being a
    /// nicety once an engine's `config/routes.rb` could declare a helper: captioned from the
    /// root, such a card reads "From `routes.rb`" — the same caption the *project's* own routes
    /// file gets. `shouty-1.2.3/config/routes.rb` says which of the two it was.
    ///
    /// The gem root is found by walking up to the directory whose parent is named `gems`, which
    /// is the one shape every layout in `gems::gem_roots` shares. Anything else falls back to
    /// the file's own name: this is a caption, and a caption is not the place to fail.
    fn workspace_relative(&self, uri: &DocUri) -> String {
        let Some(path) = uri.to_path() else {
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

    /// The registry the server really builds, for the fixtures that merge a `Context` by hand.
    fn registered() -> Registry {
        Registry::new(vec![
            Box::new(rails_lists::Rails::default()),
            Box::new(annotations_list::Annotations::default()),
            Box::new(structs_list::Structs),
        ])
    }

    /// One document's contribution, as [`Analysis::contribution`] would have built it.
    ///
    /// The Rails half goes in the module's slot, which is where the walk puts it: the slots are
    /// positional and `Rails` is registered first.
    fn declaring(name: &str, superclass: &str, table: &str) -> Contribution {
        claiming(name, superclass, &[(table, name)])
    }

    /// The same, for a document that claims a table under more than one name.
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
            context.absorb(uri, contribution, &mut included);
        }
        context.settle();
        context
    }

    /// Clause 2 of the item, end to end: core's only mention of a module is the line that
    /// registers it, and that line is not in the pass.
    ///
    /// **Enforced rather than asserted.** The registry is swapped for an empty one and the
    /// workspace is re-indexed: the pass still runs — the walk, both gates, the split, the
    /// record — and declares nothing at all. A seam that only looked right in the source would
    /// pass a reading of it and fail this.
    #[test]
    fn a_pass_with_no_body_of_knowledge_registered_runs_and_declares_nothing() {
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        assert!(harness.has("Story#title()"), "the fixture declares nothing");
        let passes = harness.analysis.passes;

        // The registry is swapped and the pass is forced, rather than `rebuild`ing: a rebuild is
        // a configuration change and puts the registered modules back, which is the right
        // behaviour and the wrong thing to test with.
        harness.analysis.knowledge = knowledge::Registry::empty();
        harness.analysis.mark_dirty();
        harness.settle();

        assert!(harness.analysis.passes > passes, "the pass did not run");
        assert!(
            harness.analysis.synthesized.is_empty(),
            "a build with nothing registered generated something"
        );
        assert!(!harness.has("Story#title()"));
        // And the walk still walked: what is empty is what the modules would have filled.
        let context = harness.analysis.context();
        assert!(context.classes.contains("Story"), "the walk stopped too");
        assert!(context.documents.is_empty(), "a list nobody registered");
        assert!(context.projections.is_empty());
    }

    /// Clause 3 of the item, end to end: a module that is not Rails declares through the same
    /// seam, and `environment.rs`'s fence is not in its way.
    #[test]
    fn a_body_of_knowledge_that_is_not_rails_declares_through_the_same_seam() {
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let spec = harness.write(
            "spec/models/story_spec.rb",
            "RSpec.describe Story do\n  let(:story) { Story.new }\n  let!(:other) { 1 }\nend\n",
        );

        harness.watch(&[&spec]);
        harness.analysis.knowledge =
            knowledge::Registry::new(vec![Box::new(crate::knowledge::rspec::RSpec)]);
        harness.analysis.mark_dirty();
        harness.settle();

        // The group is a name this module minted, because `RSpec.describe Foo do` is an anonymous
        // subclass and RBS cannot declare on one.
        assert!(
            harness.has("RSpecExampleGroup::StorySpec#story()"),
            "{:?}",
            harness.every_generated_document()
        );
        assert!(harness.has("RSpecExampleGroup::StorySpec#other()"));
        // Its declarations are mapped to the file that implied them like anybody else's, and the
        // provenance names it.
        let rbs = harness.generated_for(&spec).expect("the spec declared");
        assert!(rbs.contains("spec/models/story_spec.rb"), "{rbs}");
        // And Rails declares nothing, because Rails is not registered in this build.
        assert!(!harness.has("Story#title()"));
    }

    #[test]
    fn two_documents_merge_to_the_same_context_in_either_order() {
        // The property the per-document contributions stand on — the gate's and the memo's
        // alike — stated where it is decided rather than asserted through a walk that cannot be
        // made to change its order.
        //
        // Both fields here are easy to get wrong and by two different mechanisms:
        // `superclasses` is last-writer-wins over a `HashMap`'s iteration unless it takes the
        // lowest URI — which is what `defined_in`, filled by the same `if let Some(superclass)`
        // two lines away, already does — and `claims` is pushed once per definition.
        //
        // Solidus is the corpus that writes the first one: `class Spree::Product < Spree::Base`
        // in the library, and again in a spec.
        let model = "file:///p/app/models/double.rb";
        let spec = "file:///p/spec/support/double.rb";
        // The spec claims the same table under a second name, so the two documents disagree
        // about `superclasses` *and* push different strings into one `claims` list.
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
        // The `<=` in `Context::absorb`, and why it is not `<`. A file that writes `class Story`
        // twice is disagreeing with itself, and Ruby's answer is the last line — which is what the
        // walk produces, because within one document the loop runs in the order the definitions
        // were recorded.
        let uri = "file:///p/app/models/story.rb";
        let mut context = Context::new(&registered());
        let mut included = Vec::new();
        context.absorb(
            uri,
            Contribution {
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
        // **The memo's whole claim, and the only test that can state it as one sentence.** The
        // two tests below name the field a stale entry would be visible in; this one compares
        // the *whole* projection, so a field nobody thought of is covered by the same assertion.
        //
        // Both walks run over one graph with nothing in between, which is what makes them
        // comparable at all: the pass runs *before* the resolve and writes generated documents
        // the next walk can see, so two walks either side of a settle are legitimately allowed
        // to differ and would prove nothing.
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

        // The real mixture rather than the easy case: one document marked re-indexed, so the
        // walk drops that entry and re-projects it while taking every other one from the memo.
        harness.analysis.touched.insert(story.as_str().to_owned());
        let mixed = harness.analysis.walk();
        // And the same graph with nothing held at all.
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
        // The memo's invalidate arm, and the two routes into it are the two ways a document is
        // re-indexed: `touched`, which names one, and `touched_all`, which names none and drops
        // the map whole. Nothing else can make a held `Contribution` wrong.
        //
        // `classes` is the field to assert on because it is a set: a stale entry does not merely
        // fail to add the new name, it goes on asserting the old one — so both halves of each
        // assertion are the test.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let buffered = harness.write("app/lib/buffered.rb", "class First\nend\n");
        let watched = harness.write("app/lib/watched.rb", "class Third\nend\n");
        harness.index();
        harness.analysis.settle();

        // One named document, which only a keystroke ever is.
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

        // And a route that names none: the file system moved under a file nobody has open, so
        // `touched_all` is the whole of what the pass is told.
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
        // The two of `Context`'s six outputs that mean "the application" rather than "a class
        // the reader can name", and each is a way gate 2 could have made an answer worse.
        //
        // A table is claimed by pluralizing a top-level class's name, so an engine that defines
        // one would take a table the application's own model owns — or, as here, a table no
        // class of the user's has, which would put the schema's columns on somebody's gem. And
        // an engine's controllers are hosts in Rails and are not hosts here, because
        // `rails_lists::ROUTES` is closed to engines: there are no helpers for them to be given.
        let (dir, root, env) = project_with_engine(&[
            ("models/widget.rb", "class Widget\nend\n"),
            (
                "controllers/shouty/base_controller.rb",
                "class Shouty::BaseController < ActionController::Base\nend\n",
            ),
            ("models/shouty/message.rb", "class Shouty::Message\nend\n"),
            // A module whose name is the one spelling that makes a module a host. Rails does
            // give an engine's helper modules the application's route helpers; ya-lsp does not,
            // because `rails_lists::ROUTES` is closed to engines and there is nothing to give them.
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
        // The engine rule, asserted through the real path rather than by reading the table:
        // a list is open to an engine when what it reads declares members on a class the reader
        // can name, and closed when it declares something scoped to an application.
        //
        // the routes list is open, and what a gem's routes file is *allowed to say* is the
        // question rather than this list's — the list only decides which documents a generator
        // may open. The two closed ones are the ones with a consequence: a `schema.rb` is the
        // *application's* database and an engine ships migrations rather than one, and
        // `self.table_name=` is the input to a generator that is itself closed. Both files are
        // staged under `app/` precisely so the list rule is what is being measured rather than
        // the walk.
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
    /// **The whole claim of the text test, and both halves are needed to make it true.** A
    /// provenance comment names a file and the macro it read and never a line number, so an
    /// edit that pushes `t.string "title"` down a line re-derives RBS that is byte-identical
    /// and mappings that have all moved. Handing that to rubydex makes it drop the generated
    /// document, invalidate every declaration the old one touched and link it all again to
    /// arrive at the graph it already had: one keystroke in `app/models/user.rb` cost **940 ms**
    /// on discourse and 80 on lobsters, and the cost is a function of the workspace rather than
    /// of the file being edited.
    ///
    /// The counter is the only instrument that can say so, for the reason `Analysis::walks` is
    /// one: a graph rebuilt into exactly the shape it already had answers every question the
    /// same way. And the second assertion is why the mappings cannot simply be ignored too —
    /// what moved really did move, and a jump that lands one line high is worse than a slow one.
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

        // Opening it changes nothing at all, and then one blank first line puts every column
        // one line further down without changing a word any of them says.
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

    /// The whole of why a generated document is a **body** and not a file.
    #[test]
    fn a_column_that_changed_type_re_indexes_its_own_table_and_no_other() {
        // `Synthesized::record` is charged per declaration it re-indexes, so one document per
        // file makes a single column's type change cost every column in the project — 2.2 s of a
        // 2.49 s keystroke on discourse, where the schema is 450 KB. One document per body makes
        // it cost one table. `synthesized.md` has the measurement.
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
        // byte-identical, so `record` never hands it over. Before the split there was one
        // document for the file and every column in it was re-indexed for this one edit.
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

    #[test]
    fn a_second_database_s_schema_is_read_and_says_which_file_it_is() {
        // Rails has had several databases since 6.0, and a new Rails 8 application ships three
        // secondary schemas — solid_queue, solid_cache, solid_cable — before anybody writes a
        // line of it. lobsters, which every number in this section comes from, has three of its
        // own. Reading only `db/schema.rb` misses whole models silently.
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
        // column that is not in it would be a confidently wrong answer, which is the one thing
        // this half of the release is built not to give.
        let card = card(&mut harness, &uri, source, "name");
        assert!(card.contains("db/animals_schema.rb"), "{card}");
        assert!(!card.contains("`db/schema.rb`"), "{card}");
    }

    #[test]
    fn a_table_two_schemas_declare_is_declared_by_neither() {
        // The same rule as two classes claiming one table, for the same reason and one worse:
        // the model would answer with two schemas at once, and `Types::harvest` would keep
        // whichever column it read last — a type that depends on the order a `HashMap` iterates.
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
        // A generator that reads several files gets no notification when one of them stops
        // having something to say — the file is still there, still indexed, still a schema. So
        // the pass that writes is also the pass that prunes.
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
        // What every generator after this one inherits. They generate from *model* files
        // through the same side
        // table, so a pass over `db/*schema.rb` that pruned by "everything I did not just
        // write" would delete their work on the way past. Which sources belong to which
        // generator is the caller's question, which is why `Synthesized::sources` hands back
        // sources rather than deciding.
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

        // Both generators' documents, both still answering, after a settle that ran the schema
        // pass over a workspace whose only schema is `db/schema.rb`.
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
        // The property `make canary`'s file count depends on, and the reason the plumbing is a
        // watcher rather than the blanking hook a template gets: on every route this server
        // takes by itself — the cold walk, the watcher, the pass — rubydex never sees SQL, so
        // there is no parse error to file, no diagnostics to publish and no document to count.
        // The *generated* document exists, and it has no file behind it. (A client whose
        // document selector hands a `.sql` over on `didOpen` indexes it like any other buffer,
        // which is what every non-Ruby file does; no shipped client does — the extension's `LANGUAGES` is `ruby` and `erb`.)
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
        // The two-schema rule reached by a second road. An application has one format or the other,
        // so this is a repository that switched and did not delete the old file — and the two
        // are then two sources for one table, which is the case where answering at all means
        // answering with whichever was read last.
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
        // `refresh`'s third outcome: a `.sql` is neither a document to re-index nor one to
        // forget, so the branch invalidates and indexes nothing. All three events go through it,
        // which is why it sits above the `is_file` test as well as above the index gate — a deleted
        // dump is pruned by `forget_stale` on the settle this triggers, and nothing else would ever
        // trigger one.
        let source = "Story.new.title\n";
        let (mut harness, _dump, _uri) = sql_project(source);
        let secondary = harness.root.path().join("db/animals_structure.sql");
        let dog = harness.write("app/models/dog.rb", "class Dog\nend\n");
        harness.watch(&[&dog]);

        // Written: Rails names every other database's dump `db/<database>_structure.sql`,
        // exactly as it names the Ruby one `db/<database>_schema.rb`.
        let animals = harness.write(
            "db/animals_structure.sql",
            "CREATE TABLE public.dogs (\n    name character varying NOT NULL\n);\n",
        );
        harness.watch(&[&animals]);
        assert!(harness.has("Dog#name()"), "a new dump was not read");
        assert_eq!(harness.analysis.synthesized.len(), 2);

        // Deleted. Nothing re-reads a file that is gone, so a column left behind here would
        // answer for the life of the process.
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
        // The two clauses that bound a relation class. Two models declare `has_many
        // :comments`, and
        // between them they cost **one** `Comment::Relation` — 38 models cost 38 classes and not
        // 149. And nothing in it is a jump target: no line of anybody's code declares
        // `Comment::Relation#first`, so pointing at one of the two `has_many`s would be picking
        // an arbitrary half of a coin flip.
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
        // The one way the relation classes can make an answer *worse* rather than merely absent. A
        // project that wrote its own `Comment::Relation` meant something by it, so the pass emits
        // nothing at all for that element type — the collection loses its type rather than the
        // user losing their class.
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

    /// The bundle answers on the settle it lands, and not on the settle after that.
    ///
    /// **A whole-graph lookup that answers about the previous settle.** `Context::framework` asks
    /// `Graph::get`, which reads the map `Resolver::resolve` builds — and this pass runs
    /// immediately *before* the resolve, so it was answering about the previous settle. Measured
    /// over lobsters at the moment it is asked: 4 declarations against 1,968 definitions on the
    /// cold settle, **2,431 against 174,919** on the settle the bundle lands, 144,299 only on
    /// the one after. `has_one_attached` therefore declared nothing until the user's next
    /// keystroke, which is invisible to every measurement that does not settle twice.
    ///
    /// `index_gems` settles exactly once, which is what makes this a test rather than a
    /// coincidence: that settle is the one a bundle macro has to be answered on.
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

    /// Two settles over a workspace nobody touched write the same bytes.
    ///
    /// **The pass must never read a document it generated.** The graph holds the *previous*
    /// settle's generated documents at the moment it is read, so a lookup that could see one
    /// would answer differently on every pass and nothing in the output would say so. The
    /// filter is a property of the URI **scheme** rather than of a list, which is what a
    /// non-`file:` scheme is for; this asserts the property rather than the filter.
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
    /// `Namespaces::spellable` is where it lands: a joined `class Shouty::Thing::Point`
    /// introduces `Shouty`, which is safe exactly when
    /// something declares `Shouty` — and *something* has never meant *this application* except
    /// by accident of what `Context` was allowed to walk. What is **not** widened is which class
    /// a macro may name; `synthesized.md` has the six-corpus measurement that settles it.
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

    /// One document may open the same body twice, and `Synthesized::record` has to keep it.
    ///
    /// A file under a namespace **nothing declares** writes its members out one body per
    /// segment, so a file that declares on the module *and* on a class inside it opens
    /// `class Ns` twice — once for each. Reopening is legal RBS and the parse gate is where a
    /// mistake about that goes silent: it costs forem's whole `include` document, with every
    /// test still green.
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
        // Opened twice, in two spellings, and the second spelling is the conjured namespace's
        // doing: `app/services/ns/` declares `Ns` now, so the singleton body is *wrapped* in it
        // and `Panel`'s is joined onto it. Two openings either way, which is what the parse gate
        // is being asked about.
        assert_eq!(rbs.matches("module Admin\n").count(), 1, "{rbs}");
        assert_eq!(rbs.matches("module Ns::Admin\n").count(), 1, "{rbs}");
        assert!(harness.has("Ns::Admin::<Admin>#label()"), "{rbs}");
        assert!(harness.has("Ns::Admin::Panel#size()"), "{rbs}");
    }

    /// A `def` inside a compact-path class belongs to that class, and not to `Object`.
    ///
    /// The defect the conjured namespace closes, and it is worth stating as a member rather than
    /// as a declaration because silence was the mild half. `class User::Policy::NotAlreadySilenced`
    /// in a file Rails loads is ordinary discourse; with nothing declaring `User::Policy`,
    /// rubydex binds the plain `def`s inside it by walking the lexical chain up past a class
    /// whose declaration does not exist yet and stopping at `Object` — so `call` lands on every
    /// object in the workspace, and the `def` line itself answers nothing because the class it is
    /// asked about does not hold the member. Declaring the namespace the directory names is what
    /// Ruby is doing anyway, and the binding follows.
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
        // The half that is a wrong answer rather than a missing one: every receiver in the
        // workspace answered `call` before this, and the jump landed in a service object.
        assert!(!harness.has("Object#call()"));
    }

    /// The namespace is conjured by the **directory**, so a file that declares it wins instead.
    ///
    /// The other side of the filter, and it has to be a `class` to be worth testing: a directory
    /// conjures a `module`, and a `policy.rb` writing `class User::Policy` would be overruled by
    /// a generated body spelling the other keyword. rubydex holds one declaration of a constant,
    /// so the two kinds are not a reopening — they are a coin toss.
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
        // The file's own kind survived, which is the whole of what the filter protects: a
        // generated `module User::Policy` would have taken this singleton with it.
        assert!(harness.has("User::Policy::<Policy>#all()"));
    }

    /// Two directories spelling one namespace: one constant, and a place in each of them.
    ///
    /// `app/jobs/reports/` and `app/services/reports/` both name `Reports`, and neither
    /// directory is a line anybody can be sent to. Both files are, both write the constant, and
    /// deleting either leaves it — so both declare it and the declaration rubydex merges them
    /// into has a definition from each. The graph hands the documents over in no order at all,
    /// which is why `Context::autoloaded` sorts: the *order of the places* is what a reader is
    /// handed, and two runs over one workspace may not hand over two different orders.
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
        // One body per confirming file, and the directory keys nothing at all any more.
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
    /// The defect this closed: `hover` read `module Mod` off the conjured declaration while
    /// `definition` answered nothing at all, because the declaration was keyed by a directory
    /// and a directory has no line. Measured over six corpora at 93 such names, every one of
    /// which hovered and none of which could be jumped to.
    ///
    /// The places are the `Mod` of each `class Mod::…`, which is where Ruby's own operational
    /// test puts the declaration — no single file declares it and all of them do — and they
    /// arrive in path order, which is `Context::autoloaded`'s sort and not the walk's.
    #[test]
    fn a_namespace_only_a_directory_declares_is_a_place_in_every_file_that_writes_it() {
        let mut harness = Harness::new();
        // Three references to `Mod` in the first file and only the path's may be the place: the
        // use above the class is outside the construct, and the superclass is inside it but
        // later. `FLAG` is a declaration whose parent is not `Mod`, which is what the window is
        // found by skipping.
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
        // The one segment on the `class` line — line 1, not the `Mod::Audit` above it and not
        // the superclass after it.
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
        // And the card still reads the same, which is the half that was never broken.
        let markdown =
            harness.hover_at(&one, flagged, "Mod::FlaggedController")["contents"]["value"]
                .as_str()
                .expect("markdown")
                .to_owned();
        assert!(markdown.contains("module Mod"), "{markdown}");

        // The picker gains it for the same reason, and that is a consequence rather than a
        // second rule: `search` drops a row whose `locator::site` is `None`, so a namespace
        // nothing could be sent to was a namespace nobody could search for either.
        let names: Vec<String> = harness
            .ask("workspace/symbol", serde_json::json!({ "query": "Mod" }))
            .as_array()
            .into_iter()
            .flatten()
            .map(|symbol| symbol["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        assert!(names.contains(&"Mod".to_owned()), "{names:?}");
        // Once, although two files declare it: the picker offers declarations and this is one.
        assert_eq!(
            names.iter().filter(|name| *name == "Mod").count(),
            1,
            "{names:?}"
        );
    }

    /// A chain of directories is a place per segment, and each one is its own bytes.
    ///
    /// `app/controllers/api/v1/accounts/` conjures three namespaces from one file, and the
    /// three spans are three slices of one constant path. `full` is the namespace the
    /// declaration opens and stops before the class's own name, which is a different constant.
    #[test]
    fn every_segment_of_a_conjured_chain_is_its_own_place() {
        let mut harness = Harness::new();
        let source = "class Api::V1::Accounts::CredentialsController\n  def show\n  end\nend\n";
        let uri = harness.write(
            "app/controllers/api/v1/accounts/credentials_controller.rb",
            source,
        );
        harness.watch(&[&uri]);

        // `Api::V1::Accounts` is bytes 6..23 of the line, and the three segments slice it.
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
    /// The other half, and the one that bounds the rule: `User::Policy` is conjured only while
    /// nothing declares it, so a `policy.rb` beside the directory takes the name out of
    /// `Context::autoloaded` entirely and the `class User::Policy` line answers alone. The
    /// twelve files under `user/policy/` are not places for it and must not become any.
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
    /// The other half of the rule, and the half that is *not* a repair: the damage is done by
    /// a segment **nothing declares**, so a name every segment of which some file writes down is
    /// left joined exactly as it is written. Asserting that is what keeps the rule from spreading:
    /// a body per segment written unconditionally costs real positions on names like
    /// `Comment::Relation`.
    #[test]
    fn a_model_inside_a_module_is_left_joined_and_still_answers() {
        let source = "Admin.table_name_prefix\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
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
            !card.contains("Matched on the method name alone"),
            "the module keeps its own singleton: {card}"
        );
    }

    #[test]
    fn exactly_one_routes_file_declares_each_helper() {
        // Two routes files naming one helper would land in two generated documents, where
        // `Facts`' precedence cannot see them and RBS holds two `def story_path:` lines as an
        // overload set. First in URI order writes it, exactly as for a shared relation class.
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
        // Whichever file sorts first keeps it; here that is the application's own.
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
        // The rule `Comment::Relation` follows, and here it costs the whole feature: a project
        // that spelled `RouteHelpers` meant something by it, and a module this pass wrote into
        // would answer with its members and ours at once.
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
        // The pruning rule, on the one generator whose document also carries the `include`s:
        // an empty routes file must leave neither a helper nor a host behind.
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
    /// The rule `Comment::Relation` and `ROUTE_HELPERS` follow, asked of the one class the
    /// query interface invents. What it withdraws is the **relations**, which is that same
    /// collision behaviour reached by a second road rather than a rule of its own: with no relation class there is nothing for a `has_many` to return, so the whole
    /// half declines together instead of leaving a `-> Comment::Relation` naming a class nothing
    /// declares, or a relation inheriting whatever the user meant by the name.
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

        // Take the name away and the whole feature comes back, which is what says the decline
        // is about the collision rather than about anything else in the fixture.
        std::fs::remove_file(own.to_path().unwrap()).unwrap();
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
        // Both directions of the same property the schema pass has: the generators run before
        // every resolve, so a macro typed into a model is answerable on the next settle and one
        // deleted from it takes its member with it.
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
        // The direction class → table, in the case that decides it. `widgets` is a table with
        // a column, and there is no `Widget` — so nothing is declared, rather than a class
        // being invented for it or the table being singularized onto something that exists.
        let (harness, _schema, _uri) = rails_project("Story.new\n");
        assert!(harness.has("Story#title()"));
        assert!(!harness.has("Widget#name()"));
        assert!(harness.analysis.graph.get("Widget").is_none());
    }

    #[test]
    fn a_model_added_after_the_index_makes_its_table_answer() {
        // The reason the generation runs before every resolve rather than when the schema file
        // changes: the *other* input is which classes exist, and `rails generate model` writes
        // a file that has nothing to do with `db/schema.rb`'s mtime.
        let (mut harness, _schema, _uri) = rails_project("Widget.new\n");
        assert!(!harness.has("Widget#name()"));

        let widget = harness.write("app/models/widget.rb", "class Widget\nend\n");
        harness.watch(&[&widget]);

        assert!(harness.has("Widget#name()"), "a new model was not noticed");
    }

    #[test]
    fn editing_the_schema_replaces_what_it_declared() {
        // The same failure the side table exists to prevent, now through the generator that
        // fills it: a column that is renamed must stop answering under its old name, and it is
        // silent when it does not.
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
        // The cost a non-Rails project pays for all of the above, which has to be nothing: one
        // hash lookup per settle, no document, no entry in the table.
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
        // `settle` runs this pass before every `resolve` and a forced settle sits in front of
        // every graph-reading request, so without a gate one keystroke in a file with no macro
        // in it pays for a whole-workspace regeneration — 63 ms on discourse, in front of
        // completion's own 10.
        //
        // Three assertions, and the middle one is the gate: nothing is skipped until a pass has
        // run, a keystroke in a file no generator opens skips it, and every answer is still
        // there afterwards.
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

        // The half a naive "does *this* document declare anything" test would get wrong: a new
        // class name is a new answer for every macro anywhere that names one, so the projection
        // moves and the pass runs.
        harness.change(&uri, "class Renamed\n  def a\n  end\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.passes > before,
            "a class the workspace did not have before is not nothing"
        );
    }

    #[test]
    fn a_keystroke_in_a_file_the_pass_does_not_read_does_not_walk_the_workspace_either() {
        // The property the per-document gate rests on: what is held has to be a function of
        // **what the document contributes** and not of the document, proven by changing a body
        // without changing what it contributes.
        //
        // `passes` cannot see this — the outer gate already stops the generators, and what is
        // left is the projection they are handed being rebuilt in full to decide that. So
        // `walks` is the instrument, and the two assertions are the point: an edit that
        // rewrites most of a file walks nothing, and an edit that renames the class it defines
        // walks everything.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);

        let plain = "class Plain\n  def a\n  end\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        let walks = harness.analysis.walks;

        harness.open(&uri, plain);
        // A comment, a rename of a method, a local variable, a whole second method and a string
        // literal — everything a file is mostly made of, and not one of them is read by any
        // field of a `Contribution`.
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

        // The other half, and it is the same file: the one line in it the walk *does* read.
        harness.change(&uri, "class Renamed\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.walks > walks,
            "a class the workspace did not have before is not nothing"
        );
    }

    #[test]
    fn a_keystroke_in_a_model_file_runs_the_generators_and_does_not_walk_the_workspace() {
        // **The two gates must not share a clause.** "Would the walk produce the projection
        // already held" and "has a file some generator reads changed" are different questions,
        // and the second is right about the generators and says
        // nothing about the walk: `has_many :comments` becoming `has_many :tags` changes what
        // the file says and not one thing the projection is made of, so the generators must run
        // and the walk must not. On discourse the walk is 70 ms of every such keystroke.
        //
        // `walks` and `passes` together are the only instrument that can say so, and the two
        // assertions have to be made in the same breath: a pass that did not run would satisfy
        // the first one for the wrong reason.
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
    fn a_keystroke_in_one_model_file_reads_that_file_and_no_other() {
        // The parse memo: a keystroke in a model file costs the reading of *that* file and
        // nothing else. Without it, one changed file makes the pass re-read and re-parse every
        // file on every list to learn what all but one of them said last time — 1,527 files and
        // 104 ms of Prism on discourse, every settle.
        //
        // `reads` is the instrument for the reason `passes` and `walks` are: the memo changes
        // no answer at all, and a file re-parsed into exactly the tree it already had is
        // indistinguishable from one that was not opened.
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

        // A settle over a workspace nobody touched reads nothing at all, which is the wider
        // claim: the memo is keyed by the file rather than by which file was edited.
        let reads = harness.analysis.reads();
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.reads(),
            reads,
            "a settle over an unchanged workspace re-read files"
        );

        // Opening it is not a keystroke and does cost one read of one file: a stamp and a
        // buffer cannot be compared, so the authority changing hands is a miss by construction.
        // It happens once per open, of one file, and the alternative is hashing what is on disk
        // to find out — which is the read.
        harness.open(
            &story,
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        let reads = harness.analysis.reads();

        // And *then* one keystroke in one of them is one read. The edit changes what the file
        // declares, so the pass is not gated out — this is the pass running in full and opening
        // exactly one file.
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
        // The guarantee the memo is most able to lose: this pass claims the answer is a
        // function of what is **on disk**, and it earns that by re-reading everything every
        // time. A memo is exactly the thing that stops doing that.
        //
        // What replaces the re-read is the `stat` — `Fresh::Disk` is the gate's own stamp and
        // not a second mechanism — so a `git checkout` that rewrites a schema takes effect at the
        // very next settle, with no watcher notification anywhere in it. Nothing here calls
        // `watch`, which is the whole test.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        // Sleeping is not an option and does not have to be: `stamp_of` reads the length as
        // well as the modification time, precisely because a filesystem's clock is coarse and
        // two writes inside one tick are a real edit.
        std::fs::write(
            schema.to_path().expect("a file uri"),
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

        // And a file that is *gone* takes its parse with it rather than leaving one nothing can
        // refresh — the same guarantee at the other end, and the one that would leave a column
        // answering forever.
        std::fs::remove_file(schema.to_path().expect("a file uri")).expect("delete the schema");
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert!(
            !harness.has("Story#headline()"),
            "a schema deleted behind the server's back is still declaring columns"
        );
    }

    #[test]
    fn a_file_whose_name_ends_schema_rb_and_is_not_a_schema_is_never_read_as_one() {
        // The suffix is the cheap half of the test and the module's own reader is the rule: a dump
        // lives in `db/`, so a file called `legacy_schema.rb` anywhere else is somebody's own
        // code that happens to end in those nine characters. It is on `rails_lists::SCHEMAS` — the
        // `Wants` row matches the name — and the reader is never handed it, which is why the
        // filter sits where the pass decides what to *open* rather than where it declares.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        //
        // The decoy is a **copy of the real schema**, which is what makes one assertion enough:
        // a table two schema sources declare is declared by neither, so reading it would take
        // `stories` away from both and `Story#title` would stop answering. Asserting that some
        // table *only* the decoy holds is absent would prove nothing — nothing declares a class
        // to claim it either way.
        let decoy = harness.write("lib/legacy_schema.rb", SCHEMA_RB);
        harness.watch(&[&decoy]);

        assert!(
            harness.has("Story#title()"),
            "a file named like a schema and living outside db/ was read as one"
        );
    }

    #[test]
    fn a_file_that_joins_a_list_without_changing_is_read_for_the_reader_it_joined_for() {
        // The memo compares the readers an entry was built for and not only its text, and
        // this is the one shape that needs it. Every list but one is a function of a document's
        // own content; `rails_lists::MODELS` is not, because `Analysis::walk` adds to it, after the
        // walk, every **model** that writes no macro at all — and whether a class is a model
        // depends on a superclass chain that runs through *other files*. So `class Widget < Base`
        // joins the model list the moment a different file makes `Base` a model, with not one
        // byte of `widget.rb` changed.
        //
        // A memo keyed on the text alone would then serve an entry that was read for the
        // `@return` tag and never ran the model reader, and `Widget` would silently get no
        // relation class.
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
        // Two files: the one that was edited, and the one that joined a list because of it.
        assert_eq!(harness.analysis.reads(), reads + 2);
    }

    #[test]
    fn a_document_the_walk_never_visits_is_never_skipped_by_it() {
        // The clause that makes one `Contribution` per document enough, and the one case it cannot
        // cover. `Analysis::contribution` declines an `.rbs` outright — a signature file has
        // `def`s and doc comments and would be handed to a reader that parses Ruby — but
        // `bundle_namespaces` reads **every** definition in the graph, so a `module` written in
        // the project's own `sig/` can still move a `Context`. A document with no contribution
        // is therefore not a document with an unchanged one, and the gate refuses it.
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
        // The guarantee the gate must not weaken, and the suite is what catches it: this
        // pass claims the answer is a function of what is on disk, and it earns that by
        // re-reading everything every time. A `git checkout` that deletes a `db/schema.rb`
        // sends no notification until the watcher gets round to it, so a gate that trusted
        // notifications alone would keep answering with columns of a file that is gone.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        let plain = "class Plain\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();

        // Behind the server's back, and then a keystroke somewhere unrelated.
        std::fs::remove_file(schema.to_path().unwrap()).unwrap();
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
        // The one input to the pass that is **not** a projection of the graph —
        // `db/*structure.sql` — and therefore the one thing the contributions cannot cover:
        // a `.sql` is not a document, so no `WANTS` row can reach one and no contribution can
        // move when one appears. A `git checkout` that brings one in sends no notification the
        // gate may trust, exactly as the deletion in the test above sends none.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        std::fs::remove_file(schema.to_path().unwrap()).unwrap();

        let plain = "class Plain\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        assert!(!harness.has("Story#title()"), "the schema is gone");

        // Behind the server's back, and then a keystroke in a file that declares nothing.
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
        // Indexed and readable are two different questions, and the gap between them is real:
        // a `git checkout` removes the file, and the next settle runs before the watcher
        // notification does. Nothing to read means nothing to say — not the last thing it said.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        std::fs::remove_file(schema.to_path().unwrap()).unwrap();
        harness.open(&uri, source);
        harness.change(&uri, source);

        assert!(harness.analysis.synthesized.is_empty());
        assert!(!harness.has("Story#title()"));
    }

    /// A project with one file from every family the `[rails]` and `[types]` switches govern.
    ///
    /// One fixture for eight tests, because what each of those tests is really about is the
    /// **other seven**: a switch that turns off more than its own family is the failure worth
    /// catching, and it is only visible where all eight are present at once.
    fn every_family(config: &str) -> Harness {
        let mut harness = Harness::configured(config);
        harness.write("db/schema.rb", SCHEMA_RB);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        // The association names a class the application defines, or it declines — which is the
        // rule rather than the fixture being fussy.
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
        // The framework family is two files rather than one, and that is what it is: the
        // `config/application.rb` it is declared from, and the bundle whose constants both ends
        // of every row are checked against.
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
    /// `module Rails` and classes for the rest, because which keyword opens each body is read
    /// off the graph rather than assumed — a fixture that spelled them all the same way would
    /// not be testing that.
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

    /// What each family writes when it is on, named once so eight tests cannot disagree.
    ///
    /// The **generated RBS** and not `Harness::has`, because that is the question these tests
    /// are really asking: did this generator run at all. A declaration can go missing for a
    /// dozen reasons downstream of the pass, and every one of them would read here as a switch
    /// working.
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
        // The one family with no key of its own: it is gated by the umbrella, so it is absent
        // from the per-switch loop below and present in the umbrella's test.
        ("framework", "config/application.rb", "def self.root:"),
    ];

    /// Every family but `absent`, asserted present; `absent` asserted gone.
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
        // The guard the two tests below need: a fixture where a family declared nothing anyway
        // would make every one of them pass by accident.
        let harness = every_family("");
        for (family, source, declared) in FAMILIES {
            let rbs = harness.generated_rbs(source);
            assert!(rbs.contains(declared), "{family}: {declared} in {rbs}");
        }
    }

    #[test]
    fn each_switch_turns_off_its_own_family_and_nothing_else() {
        // The claim the whole of `[rails]` rests on. A switch that took a neighbour with it
        // would be invisible in a project that happens not to use the neighbour, which is most
        // projects.
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
        // would have been the first Rails word to leak somewhere it does not belong, and this is
        // what says it is not just a naming choice.
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
        // Not indexed and not read are the same sentence. A project that narrowed
        // `index.include` has said what it wants looked at, and this is not the place to
        // overrule it.
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
    /// The registry is the point: four lists, three predicates and one loop, so a reader that
    /// wants a fifth list adds a row rather than a walk. `modules` and `superclasses` are here
    /// because the concern, entry-point and routes readers need them and they cost the same
    /// loop — a projection added later is a second walk over every document in the workspace
    /// that nobody notices.
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
        // Both annotation shapes in one file, which is the case the single loop exists for: a
        // `sig` *and* a YARD tag put it on the annotated list once, and twice would generate it
        // twice.
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
        // `legacy.rb` writes no macro at all and is on the list anyway: it defines a model, so
        // it is where that model's relation class and class side belong.
        // The only membership decided after the walk rather than during it.
        assert_eq!(
            named(rails_lists::MODELS),
            ["models/legacy.rb", "models/story.rb"]
        );
        assert_eq!(named(rails_lists::RENAMED), ["models/legacy.rb"]);
        assert_eq!(named(annotations_list::ANNOTATED), ["lib/widget.rb"]);

        // Every class *and* module, because that is the bound on what a macro may name; and the
        // modules on their own, because a module is a different thing to declare on.
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

        // Spelled as written and not qualified: the test applied to it is a suffix, and
        // a superclass a class does not define is still read — `ApplicationRecord` is the base
        // and `ActiveRecord::Base` is a gem's.
        assert_eq!(
            context.superclasses.get("Story").map(String::as_str),
            Some("ApplicationRecord")
        );
        assert_eq!(
            context.superclasses.get("Legacy").map(String::as_str),
            Some("ActiveRecord::Base")
        );
        assert_eq!(context.superclasses.get("Widget"), None);

        // A table claimed by exactly one top-level class, which is what the schema generator
        // then filters for ambiguity.
        assert_eq!(
            rails_of(&context).claims.get("stories").map(Vec::as_slice),
            Some(["Story".to_owned()].as_slice())
        );
    }

    /// A class written inside a `class << self` body is not a class this pass can name.
    ///
    /// Its lexical nesting is the singleton class, whose own name is `ParentScope::Attached` —
    /// not a constant anybody writes — so `qualified_name` answers `None` and the walk moves on.
    /// The rule it protects is the one every generator inherits: a class the application does
    /// not define declares nothing, and "cannot be spelled" and "is not defined" have to reach
    /// the same answer.
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

    /// A class Ruby accepts and Rails' inflector does not claims no table.
    ///
    /// `class Ünicode` is legal Ruby — a constant must begin with an uppercase letter, and Ruby
    /// means Unicode's uppercase — and `underscore` deliberately requires an *ASCII* capital,
    /// because there is no acronym table here and no way to guess what such a class's table
    /// would be called. The failure direction is the one the schema is built to have: no claim,
    /// rather than a claim on a table that does not exist.
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

    /// A workspace that stops declaring anything altogether still gets pruned.
    ///
    /// The clause is `synthesize`'s first line: it returns early only when there is nothing to
    /// read **and** nothing it wrote last time. Deleting every model file makes the first true
    /// and the second false, which is the one shape where an early return would leave a
    /// `Comment::Relation` in the graph for the life of the process — with no file left that
    /// could ever be edited to correct it.
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

        std::fs::remove_file(story.to_path().unwrap()).unwrap();
        std::fs::remove_file(comment.to_path().unwrap()).unwrap();
        harness.watch(&[&story, &comment]);
        assert!(!harness.has("Comment::Relation"), "left behind");
        assert!(!harness.has("Story#comments()"), "left behind");
    }

    /// A file that vanishes between the walk and the read costs its declarations and nothing
    /// else.
    ///
    /// `synthesize` runs before every resolve and reads from the graph's document list, which a
    /// watcher event has not necessarily caught up with. There is no notification to wait for
    /// and nothing to recover: the file is skipped, the rest of the pass runs, and the next
    /// event prunes it properly.
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

        // Deleted on disk and *not* announced, so the document is still in the graph and its
        // text is not.
        std::fs::remove_file(widget.to_path().unwrap()).unwrap();
        harness.analysis.synthesize();
        harness.analysis.resolve();
        assert!(harness.has("Gadget#size()"), "the pass stopped early");
    }

    #[test]
    fn a_second_pass_does_not_derive_from_what_the_first_one_delegated() {
        // The phase boundary, asserted *across settles* rather than within one — which is the
        // shape that could rot silently. `synthesize` runs before every resolve, so if the union
        // ever held phase two's own output the answer would change on the second keystroke and
        // keep changing: a `delegate` through a `delegate` would type on run two, a chain of
        // three on run three. It cannot, because the union is built from a map that is fresh
        // every call and is built before anything is merged into it — and this is what says so.
        //
        // `Story#profile` is a delegation that *does* type, through `belongs_to :user` and
        // `has_one :profile`. `Story#bio` delegates through it, and must stay untyped however
        // many times the pass runs — which only a chain can show, because a hover card names no
        // return type.
        let mut harness = Harness::new();
        // Every one of them writes `< ApplicationRecord`, because of the host test: a class
        // that inherits nothing is not an ActiveRecord model and its association macros declare
        // nothing at all.
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
            through.contains("Matched on the method name alone"),
            "phase two derived from itself: {through}"
        );

        let documents = harness.analysis.synthesized.len();
        harness.analysis.synthesize();
        harness.analysis.resolve();
        assert_eq!(harness.analysis.synthesized.len(), documents);
        assert_eq!(card(&mut harness, &uri, source, "bio\n"), typed);
        assert_eq!(card(&mut harness, &uri, source, "photo"), through);
    }

    #[test]
    fn a_caption_names_the_gem_a_file_came_from_or_falls_back_to_the_file() {
        // `workspace_relative` would say nothing outside the root can reach it; an engine makes
        // that false — an engine's `config/routes.rb` declares helpers now — and the first card
        // the engine-routes test printed read "From `routes.rb`", the same caption the project's
        // own routes file gets. The marker is a directory named `gems`, which every layout
        // `gems::gem_roots` knows ends in.
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
        // And a path with no such ancestor gets nothing, so the caller keeps its own fallback
        // rather than this one guessing at a name.
        assert_eq!(gem("/somewhere/else/config/routes.rb"), None);
    }

    /// A bundle, as small as one that still has the shape: `ActiveRecord::Relation` reached
    /// through an `include`, exactly as `relation.rb` writes it.
    fn with_a_bundle() -> (Harness, crate::workspace::DocUri) {
        let harness = Harness::new();
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

    /// The row this file's [`Owners`] exists for: a member ya-lsp declared itself,
    /// answering with the `def` Rails really wrote.
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
            card.contains("query interface"),
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

    /// And the class side takes its own `def` where Rails writes one, rather than the
    /// relation's fall-through.
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

    /// With no bundle there is nothing to find, and that is the same answer as before this
    /// existed rather than a worse one.
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

    /// The bug the corpora found and the suite could not: a name `Kernel` also declares.
    #[test]
    fn a_query_method_ruby_s_own_root_also_declares_is_not_answered_with_the_root() {
        let (mut harness, _) = with_a_bundle();
        // `select` is the shape: `Kernel#select` is `IO.select`, it is an ancestor of
        // everything, and it is spelled without parentheses — so the walk finds it one spelling
        // before it finds `ActiveRecord::QueryMethods#select`.
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

        // And the other half: rejecting the root may not take the real answer with it. Where
        // Rails does write the name, that is what a reader gets, with the root's `def` sitting
        // in the same walk.
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

    /// And the second: one `def` claimed once, however many generated declarations borrowed it.
    #[test]
    fn one_def_borrowed_by_every_relation_class_is_still_one_place() {
        let (mut harness, _) = with_a_bundle();
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        harness.write("app/models/tag.rb", "class Tag < ApplicationRecord\nend\n");
        // An untyped receiver, so the answer is the name rung's list — which holds the query
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
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod framework_tests {
    use crate::analysis::testing::*;

    /// A Rails application, with the four constants the framework table names declared the way
    /// a bundle declares them — `module Rails`, and classes for the rest.
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

    /// And it still reaches what `Rails::Application` itself declares.
    #[test]
    fn the_frameworks_own_members_are_reached_through_the_projects_class() {
        let source = "Rails.application.routes\n";
        let (mut harness, uri) = project(source);
        let card = card(&mut harness, &uri, source, "routes");
        assert!(card.contains("Rails::Application#routes"), "{card}");
    }

    /// A workspace whose bundle declares none of it declares nothing, and the chain is left
    /// exactly where it was: on the name rung, which is an honest miss rather than a `Rails`
    /// this crate invented.
    #[test]
    fn a_project_with_no_framework_indexed_declares_nothing() {
        let (mut harness, _schema, _uri) = rails_project("");
        harness.write("config/application.rb", APPLICATION_RB);
        let source = "Rails.root.join(\"config\")\n";
        let uri = harness.write("app/use.rb", source);
        harness.index();
        let card = card(&mut harness, &uri, source, "join");
        assert!(!card.contains("Pathname#join"), "{card}");
        assert!(card.contains("the method name alone"), "{card}");
    }
}
