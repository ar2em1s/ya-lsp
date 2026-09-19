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
    /// Find the file Rails writes each generated query method in, now that the graph can say.
    ///
    /// - **Run after the resolve, never inside the pass.** The generators write the text rubydex is
    ///   about to link, so during the pass the graph holds four declarations (`Object`,
    ///   `BasicObject`, `Module`, `Class`) and every question about a gem's classes answers
    ///   nothing. The pass states *names*; this answers them, one step later in the same settle.
    /// - **Asked again every settle**, not cached across settles: a bundle can change under a
    ///   workspace, and a jump into a version that went away fails silently.
    /// - **Memoised within a settle.** The class side is written onto every base in the project
    ///   with the same names, so six bases ask the graph once and read the memo five times.
    pub(super) fn place_generated_members(&mut self) {
        if self.synthesized.named().next().is_none() {
            return;
        }
        let started = Instant::now();
        let mut rails = Owners::new(&self.graph, &self.knowledge);
        let layout = self.layout();
        // Collected before anything is written, because resolving reads the table places are
        // written into: `locator::places` uses it to tell a generated definition from one on disk,
        // and must see the same table for every member in one settle.
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
    /// Every registered module's owners, resolved once per settle.
    ///
    /// The names come from [`knowledge::Knowledge::places_members_on`] and the lookup is core's:
    /// this runs **after** the resolve, so the graph can answer, which a module may never assume
    /// while declaring.
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

impl Analysis {}

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

/// What a file looked like when the pass last read it: modification time and length.
///
/// `None` for a missing file, as a value, because a deleted schema and one that never existed must
/// compare unequal to one that did. Length too, because modification times are coarse and two
/// writes in one tick are a real edit.
fn stamp_of(path: &Path) -> Option<(std::time::SystemTime, u64)> {
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
    pub(super) fn synthesize(&mut self) {
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
            return;
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
            return;
        }
        self.passes += 1;
        // Every file a generator is about to open is read and parsed here and only here, and only
        // if its text moved since the last pass.
        self.refresh_sources(&context);
        if context.is_empty(&self.knowledge) && self.generated.is_empty() {
            // The view context is rebuilt too, with no sources rather than skipped: its helpers
            // half is a projection of the walk above and costs nothing, and a workspace whose last
            // macro was just deleted must *lose* the exports it had.
            self.views = self.view_context(&context);
            self.remember(context);
            return;
        }

        let mut generated = knowledge::Declared::new();
        self.views = self.view_context(&context);
        // **Every registered module, through the three phases**; core knows nothing else about
        // order. What a module's generators owe each other is the module's business; what they owe
        // *another* module's is what the phases are. Taken out and put back for `refresh_sources`'
        // reason: the view below borrows the rest of `self` while a module writes to itself.
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
            // Nothing after this may declare; that is what phase three means.
            for module in knowledge.modules_mut() {
                counted.extend(module.derive(&declaring, &mut generated));
            }
            counted
        };
        self.knowledge = knowledge;

        let mut kept: HashSet<String> = HashSet::new();
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
                .record(self.graph.graph_mut(), &mut self.types, &uri, parts)
                .len();
            kept.insert(uri.as_str().to_owned());
        }
        self.forget_stale(&kept);

        // Debug, not info: this runs before every resolve, one line per settle, and would drown an
        // ordinary session's log. These numbers exist nowhere else, and a report about this feature
        // needs them.
        //
        // `lists` is what the walk handed the generators; the counts after are what they made of
        // it. Together they answer "did a generator run, and on what", which is how a switched-off
        // generator becomes visible.
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
    /// 1. **Was a listed file touched?** A `Context` says which documents a generator opens, never
    ///    what is in them, so touching the file is the whole answer.
    /// 2. **Is every file it read still there, unchanged?** Not belt and braces: the pass claims
    ///    its answer is a function of what is on disk, so a `git checkout` deleting `db/schema.rb`
    ///    must stop the columns answering at the next settle, before any watcher notification
    ///    arrives. One `stat` per file read buys that back, and the parse memo rests on the same
    ///    `stat`.
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
            .filter_map(|uri| DocUri::from_graph_uri(uri)?.to_file_path())
            .map(|path| {
                let stamp = stamp_of(&path);
                (path, stamp)
            })
            .collect();
        self.generated_from = Some(context);
        self.touched.clear();
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
            context.absorb(document.uri(), contribution.clone(), &mut included);
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
        // A directory's conjured name is kept only where **nothing else declares it**: a `user.rb`
        // beside the `user/` directory, a `module Chat` in a plugin, a gem. So the filter runs
        // here, after both the application's names and the bundle's are recorded, never in the loop
        // above where neither set is complete. Which framework classes may be written onto is
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

    /// Which document declares each of these namespaces, for the one generator that must open a
    /// file it was not handed.
    ///
    /// - **[`Analysis::bundle_namespaces`]' walk, asked for a URI instead of a kind**, bounded the
    ///   same way: last segments are hashed first, so an unwanted definition costs one compare.
    /// - **A name several files reopen answers with the first in URI order**: arbitrary but
    ///   deterministic. What this reads is what a module declares in its own body, and a module
    ///   spread over two files would need both read whichever were picked.
    /// - **A generated document is skipped**, for `bundle_namespaces`' reason: this pass wrote
    ///   those, and reading its own output back is how a fact starts deriving from itself.
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
    fn refresh_sources(&mut self, context: &Context) {
        // Taken out and put back, because the closures below borrow the rest of `self` (open
        // buffers, the workspace root) while a module writes to itself. `Analysis::walk` does the
        // same with the contributions.
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

    /// How one document's text is identified for a module's own memo; see [`knowledge::Fresh`] for
    /// why the two authorities cannot share one answer.
    fn freshness(&self, uri: &DocUri) -> knowledge::Fresh {
        match self.open.get(uri) {
            Some(open) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                open.text.text().hash(&mut hasher);
                knowledge::Fresh::Buffer(hasher.finish())
            }
            None => knowledge::Fresh::Disk(uri.to_file_path().as_deref().and_then(stamp_of)),
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
    fn workspace_relative(&self, uri: &DocUri) -> String {
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
            Box::new(structs_list::Structs),
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
            context.absorb(uri, contribution, &mut included);
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

    /// A module that is not Rails declares through the same seam, and `environment.rs`'s fence does
    /// not get in its way.
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
        // Its declarations map to the file that implied them like any others, and the provenance
        // names it.
        let rbs = harness.generated_for(&spec).expect("the spec declared");
        assert!(rbs.contains("spec/models/story_spec.rb"), "{rbs}");
        // Rails declares nothing, because Rails is not registered in this build.
        assert!(!harness.has("Story#title()"));
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
        // Solidus writes the first shape: `class Spree::Product < Spree::Base` in the library, and
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

    #[test]
    fn a_second_database_s_schema_is_read_and_says_which_file_it_is() {
        // Rails has had multiple databases since 6.0, and a new Rails 8 app ships three secondary
        // schemas (solid_queue, solid_cache, solid_cable). lobsters has three of its own. Reading
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
        assert!(card.contains("db/animals_schema.rb"), "{card}");
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
    /// forem's whole `include` document with every test green.
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
    /// `class User::Policy::NotAlreadySilenced` in a file Rails loads is ordinary discourse. With
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
        // per-switch loop and present in the umbrella's test.
        ("framework", "config/application.rb", "def self.root:"),
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
        // `has_one :profile`). `Story#bio` delegates through it and must stay untyped however many
        // passes run, which only a chain shows, since a hover card names no return type.
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
        assert!(card.contains("the method name alone"), "{card}");
    }
}
