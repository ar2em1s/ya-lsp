//! What a body of knowledge is, and the whole of what the generator pass knows about one.
//!
//! The pass reads what the workspace declares about itself and writes the RBS it implies.
//! *What* it reads is not its business: a schema, a `Struct.new`, a Sorbet `sig` and an RSpec
//! `describe` are four bodies of knowledge that share one shape — some documents, a projection
//! of them, and [`Facts`](crate::generated::Facts) at the end. This module is that shape written
//! down, so the pass can drive a module it has never heard of.
//!
//! # Two contracts already existed and this is the third
//!
//! [`generated::Facts`](crate::generated) is the **output**: `Owner`, `Declared`, the collision
//! rules and `render` name no framework, and every generator already ends there. It took being
//! cut into one document per body without a single generator changing, which is what an output
//! contract being real looks like. [`synthesized::Mapping`](crate::analysis::synthesized) is the
//! **place**: a generated declaration's source is a `(uri, full, selection)` triple and nothing
//! more.
//!
//! What was missing is the **input** — which documents a module is handed and what it may ask
//! about them — and the **schedule**, which is the order the modules run in and the two channels
//! they speak to each other through. This is the input half: [`Wants`] is the vocabulary, a
//! module supplies the rows, and [`Registry`] is the only place core names one.
//!
//! # What core still legitimately knows
//!
//! [`Source`](crate::generated::Source)'s rank. Precedence between two generated declarations of
//! one member is a **total order across modules** — a column beats an `attribute` beats a
//! `delegate` — so a module cannot pick its own number without making the ladder unreadable. It
//! stays one table, reviewed whole. That is honest coupling; hiding it behind a registry would be
//! worse than writing it down.

pub mod annotations;
pub mod rails;
#[cfg(test)]
pub mod rspec;
pub mod structs;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::generated::{At, Facts};
use crate::workspace::{DocUri, Features};

/// Which projection a module is asking for, as the module spells it.
///
/// A string and not an enum variant, because core must be able to hold a list it has never heard
/// of — and a `&'static str` keeps it free: comparing two is a pointer-length compare, and the
/// log line that says which generator was handed what prints it as it was written. Namespaced by
/// the module that owns it (`rails.schemas`) so two modules cannot collide by accident, and the
/// registry asserts they have not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListId(pub &'static str);

impl std::fmt::Display for ListId {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(self.0)
    }
}

/// The test that puts a document on one list.
///
/// **The vocabulary is core's and the rows are the module's.** Every predicate here is a filter
/// over the definitions and references indexing already recorded, so none of them opens a file
/// and none of them names a framework: the count of files a generator then reads is the count
/// that really do call one of these. What a module supplies is which names go in the slots.
pub struct Wants {
    pub list: ListId,
    /// Receiverless call names that put a document on the list.
    pub calls: &'static [&'static str],
    /// Constant names that put a document on the list, matched on the last segment.
    ///
    /// Matching the last segment rather than the whole path means somebody's own `Foo::Struct`
    /// puts its file on the list too, which costs one parse and declines — the direction every
    /// filter here errs in, because the alternative is missing `::Struct` and every spelling of a
    /// reference nobody has thought of.
    pub constants: &'static [&'static str],
    /// Namespace names whose **definition** puts a document on the list, matched on the last
    /// segment.
    ///
    /// The only predicate that reads a `module` rather than a call, a reference or a `def`. It
    /// has to exist for the same reason `defines` does: a body whose whole content is a
    /// hand-written `module ClassMethods` calls nothing, references nothing and defines no
    /// singleton method, so it would be on no list by any other test.
    pub modules: &'static [&'static str],
    /// Singleton method names whose **definition** puts a document on the list.
    ///
    /// The receiver is checked, because an instance method of that name is a different method.
    pub defines: &'static [&'static str],
    /// A file-name suffix that puts a document on the list.
    pub path: Option<&'static str>,
    /// Whether a `@return`/`@param` tag in a comment above a `def` puts it on the list.
    pub tags: bool,
    /// Whether a class this document defines matching the module's own superclass test puts it
    /// on the list.
    ///
    /// The one predicate whose *question* core cannot ask, because "is this class one of mine"
    /// is the module's own convention. Core answers it by handing the module the superclass and
    /// the mixins it read — see [`Knowledge::claims_by_ancestry`].
    pub inherits: bool,
    /// Whether a **Rails engine's** document may go on this list, or only the user's own code.
    ///
    /// Framework-shaped in its name and not in its meaning: it is *a tree the workspace ships
    /// that is not the workspace*, and a list is open to one when what it reads declares members
    /// on a class the reader can name.
    pub engines: bool,
    /// Whether a **gem's own `lib/`** may go on this list.
    pub gems: bool,
    /// Whether a document on this list is **read** without anything being declared from it.
    ///
    /// The input to a generator that is itself empty: `self.table_name=` renames a table for a
    /// schema that has to exist somewhere else, so a workspace whose only listed document is one
    /// of these has nothing for the pass to do. [`Context::is_empty`] is where that matters, and
    /// [`Context::read_by_a_generator`] is where it deliberately does not — such a file is still
    /// read, and the gate that watches what generators read has to watch it.
    pub reads_only: bool,
}

/// What one document contributes to one module's own projection.
///
/// A trait object because the five things Rails wants per document are heterogeneous — an owner,
/// a `(name, span)` pair, two kinds of string pair — and flattening them into one generic value
/// would be lossy rather than decoupled.
///
/// **The two methods are what the pass's cheap gate rests on.** `Analysis::walk` holds one
/// contribution per document and `context_would_be_the_same` compares the held one against a
/// freshly projected one, so a module whose `same_as` is wrong makes the gate answer *nothing
/// moved* about a document that did. That is the one bug in this file that would be silent, and
/// it is why both methods are tested rather than assumed.
pub trait Contributes: std::fmt::Debug {
    /// Whether this says exactly what `other` says. `false` for a different module's value.
    fn same_as(&self, other: &dyn Contributes) -> bool;
    /// A copy, because the walk keeps one and the merge takes one.
    fn clone_box(&self) -> Box<dyn Contributes>;
    /// For the module to get its own type back out of what core held for it.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Everything one module made of every document — its half of a [`Context`](crate::analysis).
///
/// The merge of a pass's worth of [`Contributes`], and the same two rules apply to it: the pass's
/// wide gate compares two whole projections, so `same_as` decides whether a settle is skipped.
///
/// **`absorb` must not depend on the order documents arrive in.** The walk visits them in
/// whatever order the graph holds them, and the memo hands back a mixture of held and fresh
/// contributions, so a projection that sorted by arrival would answer differently on two settles
/// over one unchanged workspace. [`Self::settle`] is where an order-dependent field is repaired.
pub trait Projects: std::fmt::Debug {
    /// Take what one document contributed. Called once per document, in no particular order.
    fn absorb(&mut self, uri: &str, contribution: &dyn Contributes);
    /// Put right whatever the merge could only decide once all of it had arrived.
    fn settle(&mut self) {}
    /// Whether there is nothing here for a generator to declare from.
    ///
    /// Asked with the lists, because a module may hold something that is on no list at all — what
    /// a directory conjures is a projection of paths rather than of documents.
    fn declares_nothing(&self) -> bool {
        true
    }

    /// Files this module reads that are not graph documents at all.
    ///
    /// `db/*structure.sql` is the one, and it is why this exists: nothing else would ever notice
    /// it had changed, so the gate that re-reads what generators read has to be told about it.
    fn also_reads(&self) -> &[DocUri] {
        &[]
    }

    /// Whether this says exactly what `other` says. `false` for a different module's value.
    fn same_as(&self, other: &dyn Projects) -> bool;
    /// For the module to get its own type back out of what core held for it.
    fn as_any(&self) -> &dyn std::any::Any;
    /// And mutably, for the generators that fill in what only the whole projection decides.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// What core holds on a module's behalf: its projection, or nothing to project.
#[derive(Debug, Default)]
pub struct Projected(pub Option<Box<dyn Projects>>);

impl Projected {
    /// The module's own projection, as the module's own type.
    ///
    /// The one downcast in the contract, and it is deliberately the *caller* that names the type:
    /// core holds a `dyn Projects` and never learns what is inside one.
    #[must_use]
    pub fn of<T: 'static>(&self) -> Option<&T> {
        self.0.as_ref()?.as_any().downcast_ref::<T>()
    }

    /// And mutably, for the one field a projection can only fill once all of it has arrived.
    pub fn of_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.0.as_mut()?.as_any_mut().downcast_mut::<T>()
    }
}

impl PartialEq for Projected {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(ours), Some(theirs)) => ours.same_as(theirs.as_ref()),
            _ => false,
        }
    }
}

impl Eq for Projected {}

/// What core holds on a module's behalf: its contribution, or nothing to say.
#[derive(Debug, Default)]
pub struct Contributed(pub Option<Box<dyn Contributes>>);

impl Clone for Contributed {
    fn clone(&self) -> Self {
        Self(self.0.as_ref().map(|held| held.clone_box()))
    }
}

impl PartialEq for Contributed {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(ours), Some(theirs)) => ours.same_as(theirs.as_ref()),
            _ => false,
        }
    }
}

impl Eq for Contributed {}

/// Where a name a document declares was written, and where each of a set of names is spelled
/// inside it.
///
/// A type of its own because it travels as a borrowed closure: the pass computes it from the
/// graph, and a module may only ask it — see [`Seen::confirming`].
pub type Confirming<'a> = &'a dyn Fn(&str, &BTreeSet<&str>) -> HashMap<String, At>;

/// What a module may read of one document, and the whole of it.
///
/// **Everything here was already read by the one walk**, which is the property that makes a
/// module's projection a fold rather than a second pass: `Analysis::walk` visits every document
/// once, records the generic facts below, and hands them straight back. A module that needs
/// something not on this list is asking for a second walk — 105 ms on discourse — and the answer
/// is to widen this struct rather than to open the graph.
pub struct Seen<'a> {
    /// The document's own URI.
    pub uri: &'a str,
    /// Whether this is the user's own code.
    pub own: bool,
    /// The user's own code, plus a tree the workspace ships that is not the workspace.
    pub generator: bool,
    /// `(name, whether the line said `module`)` for everything this document declares.
    pub declared: &'a [(String, bool)],
    /// `(class, the superclass it names)`, spelled as written.
    pub superclasses: &'a [(String, String)],
    /// `(the body that wrote the `include`, the constant it spelled)`.
    pub included: &'a [(String, String)],
    /// Where a name this document declares was written, and where each of `names` is spelled
    /// inside it.
    ///
    /// The one thing a module cannot fold out of the four lists above, because it is about byte
    /// offsets rather than about names. Generic despite having one caller: *the first definition
    /// whose parent is this name, and the earliest reference to each of these inside it*.
    pub confirming: Confirming<'a>,
}

/// How the text of one source file was identified when a module last parsed it.
///
/// Two variants because the pass has two authorities for what a file says and they cannot be
/// compared the same way. A file on disk is identified by its modification time and length, which
/// is the pass gate's own evidence and not a second mechanism: that gate earns "the answer is a
/// function of what is on disk" by looking at the disk, and a memo that trusted anything weaker
/// would take it away. A file the editor holds has no useful stamp at all — the disk is behind the
/// buffer — so it is identified by its text, hashed. Not by the version: a client may send
/// `didChange` with no version, and two versions of one buffer would then look like no change at
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fresh {
    /// A file on disk: when it was written, and how long. `None` for a file that is not there —
    /// a value rather than an error, because a schema that has been deleted and one that never
    /// existed have to compare unequal to one that did.
    Disk(Option<(std::time::SystemTime, u64)>),
    /// A file the editor holds, hashed.
    Buffer(u64),
}

/// What a module may read while it re-parses its own sources.
///
/// **The filesystem stays core's.** A module says which documents it wants and what it made of
/// them; it never opens a file, because an open buffer is authoritative over the file on disk and
/// only the server knows which buffers are open. So the text and its freshness arrive as closures
/// and the module keeps whatever it parsed.
pub struct Sources<'a> {
    /// What the one walk produced.
    pub context: &'a Context,
    /// Which bodies of knowledge this project asked for.
    pub features: Features,
    /// How the text of a file is identified right now, for a memo to compare against.
    pub fresh: &'a dyn Fn(&DocUri) -> Fresh,
    /// The text itself, as the buffer or the file has it. `None` for a file that has gone.
    pub text: &'a dyn Fn(&DocUri) -> Option<String>,
    /// How a file inside the workspace should be spelled to a person: `db/animals_schema.rb`.
    ///
    /// Every reader writes it into a provenance comment, and it is a function of the URI and of
    /// where the workspace root is — which is core's fact rather than a module's.
    pub caption: &'a dyn Fn(&DocUri) -> String,
}

/// Everything a module has already declared, keyed by the source file it came from.
///
/// One entry per source and not per generator: a file that feeds two of them gets one generated
/// document holding both, because the side table is keyed by generated URI and a second record for
/// one source would replace the first rather than add to it. [`add`] is where that merge happens
/// and where precedence between two generators is enforced.
pub type Declared = std::collections::BTreeMap<String, (DocUri, Facts)>;

/// Add one generator's facts to the document its source file names.
///
/// Merging here is where precedence is enforced: a member two generators both name is decided by
/// [`Facts`]' rank rather than written twice as a silent overload. A generator that said nothing
/// adds no entry, so a source none of them had anything to say about is not recorded and is not
/// kept.
pub fn add(into: &mut Declared, uri: &DocUri, facts: Facts) {
    if facts.is_empty() {
        return;
    }
    into.entry(uri.as_str().to_owned())
        .or_insert_with(|| (uri.clone(), Facts::default()))
        .1
        .extend(facts);
}

/// Which document declares each of a set of module names.
///
/// The one graph question a generator may ask, and it is deliberately a **name** question rather
/// than a resolved one: the pass writes the text rubydex is about to link, so while a generator
/// runs the graph holds four declarations and nothing about a name has an answer yet. A name
/// several files reopen answers with the first in URI order, which is arbitrary and deterministic.
pub type Declares<'a> = &'a dyn Fn(&BTreeSet<String>) -> BTreeMap<String, DocUri>;

/// What a generator may read while it declares.
///
/// Four things, and they are what the eight Rails generators were measured to need: a document's
/// **text**, how a file is **spelled to a person**, whether a document is the **user's own code**,
/// and the **features**. Everything else a generator wants is in the [`Context`] or in the memo it
/// keeps itself.
///
/// **No graph and no filesystem.** A generator writes the text rubydex is about to link, so while
/// one runs the graph holds four declarations and every question about a gem's classes answers
/// nothing; and an open buffer is authoritative over the file on disk, which only the server
/// knows about. Both are why these arrive as closures rather than as handles.
pub struct Declaring<'a> {
    /// What the one walk produced.
    pub context: &'a Context,
    /// Which bodies of knowledge this project asked for.
    pub features: Features,
    /// A document's text, as the buffer or the file has it.
    pub text: &'a dyn Fn(&DocUri) -> Option<String>,
    /// How a file inside the workspace should be spelled to a person: `db/animals_schema.rb`.
    pub caption: &'a dyn Fn(&DocUri) -> String,
    /// Whether a document is the user's own code rather than a gem's.
    pub own: &'a dyn Fn(&str) -> bool,
    /// Which document declares each of these module names — see [`Declares`].
    pub declares: Declares<'a>,
}

/// What a module may read while it looks for files nothing has indexed.
///
/// The one place a module touches the filesystem, and it touches it through core: a
/// `db/*structure.sql` is not a graph document, so nothing would ever notice it had changed and
/// nothing would ever put it on a list.
pub struct Reading<'a> {
    /// The workspace root.
    pub root: &'a std::path::Path,
    /// Whether the project's own `index.include` admits a path.
    pub admits: &'a dyn Fn(&std::path::Path) -> bool,
    /// Which bodies of knowledge this project asked for.
    pub features: Features,
}

/// What one module declared, for the one line that says the pass ran.
///
/// A list of `(what, how many)` rather than a struct, because the eleven numbers the pass used to
/// report were eleven generators' and no two modules have the same ones.
pub type Counted = Vec<(&'static str, usize)>;

/// One body of knowledge, and the whole of what the pass knows about it.
pub trait Knowledge {
    /// What it is called, for the log line and for the registry's collision check.
    fn name(&self) -> &'static str;

    /// For a caller that holds the registry and wants one module back as its own type.
    ///
    /// The generators have not all moved out of the pass yet, and the ones still there read the
    /// memo this module keeps. It goes when they do.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Which documents it wants, one row per list.
    fn wants(&self) -> &'static [Wants];

    /// Whether this project asked for the list at all.
    ///
    /// The one place a switched-off generator is switched off, because [`Wants`] is the one
    /// table that decides which documents any generator ever sees: an empty list is a generator
    /// that does nothing, with no removal path to write and nothing downstream to teach.
    fn wanted(&self, list: ListId, features: Features) -> bool;

    /// Names this module will declare on or beside, whose namespaces have to be spellable.
    ///
    /// `Namespaces::spellable` asks whether every segment **above** a name is declared, and the
    /// answer comes from the bundle rather than from the application — so a module that writes
    /// onto a class in a gem has to say which names to go and look up. Both the name and every
    /// prefix of it are asked, because a body may be opened for either.
    ///
    /// Empty by default, which is right for a module that declares only onto the application's
    /// own classes: those are in `Context::classes` already and are asked about without this.
    fn spellable_names(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// What one document contributes to this module's own projection, or nothing.
    ///
    /// **A fold and never a walk.** Everything in [`Seen`] was already read by the one pass over
    /// the documents, so a module that needs a second look at the graph is asking for a second
    /// walk — 105 ms on discourse — and the answer is to widen [`Seen`] rather than to open the
    /// graph here.
    fn contribute(&self, _seen: &Seen<'_>) -> Option<Box<dyn Contributes>> {
        None
    }

    /// Files this module reads that nothing has indexed and no list can name.
    ///
    /// Asked once per pass, before the projection settles, so that whatever comes back is on the
    /// projection in time for the gate that re-reads what generators read.
    fn discover(&self, _reading: &Reading<'_>) -> Vec<DocUri> {
        Vec::new()
    }

    /// Take what [`Knowledge::discover`] found, so the module can put it on its projection.
    fn discovered(&mut self, _found: Vec<DocUri>) {}

    /// Re-read whatever this module parses, before anything declares.
    ///
    /// **The memo is the module's own and it is deliberately of the *parse* rather than of the
    /// facts.** Memoising the facts is the obvious seam and the worse one: they are a function of
    /// the text *and* the [`Context`], so the whole memo would have to be dropped whenever the
    /// projection moves. A reader that takes nothing but the text has no second input to compare,
    /// no invalidation rule to get wrong, and a keystroke that adds a class somewhere else in the
    /// project does not throw it away.
    fn refresh(&mut self, _sources: &Sources<'_>) {}

    /// The classes this module's generated members are really defined on, when the definition is
    /// somebody else's file rather than a line the generator read.
    ///
    /// `[instance side, class side]`, and both are **names looked up after the resolve** rather
    /// than matched: a member is found on one of these or it is not found, and not found means no
    /// place, exactly as an unmapped span does. ActiveRecord's query interface is the one case —
    /// no file in a project declares `Story.where`, and activerecord does.
    ///
    /// Empty by default, which is right for every module whose generated members' places are all
    /// lines it read.
    fn places_members_on(&self) -> [&'static [&'static str]; 2] {
        [&[], &[]]
    }

    /// Put right whatever only the **whole** walk decides, before the bundle is asked about
    /// anything.
    ///
    /// A module may reach into its own projection here and into the lists it registered: which of
    /// the application's classes are models is a question about the chain above each, and the file
    /// that defines one joins the list that opens it. Both are decided after every document has
    /// been seen and before the feature gate runs, which is why this is a hook and not a phase.
    fn after_the_walk(&self, _context: &mut Context) {}

    /// The same, once the bundle has been asked what it declares.
    ///
    /// Two things need it and both are about names rather than documents: what a directory
    /// conjures is only conjured where **nothing else declares the name**, and which framework
    /// classes may be written onto is exactly which of them the bundle holds. Whatever is returned
    /// is declared as a module namespace, because a module cannot reach `Namespaces` while it
    /// holds its own projection mutably.
    fn after_the_bundle(&self, _context: &mut Context) -> Vec<String> {
        Vec::new()
    }

    /// What a template's implicit receiver can answer, where this module knows.
    ///
    /// The one output of a body of knowledge that is **not** a declaration: what `helper_method`
    /// hands over is a permission, and the `def` it names is already indexed. It travels as a
    /// core type because the type rungs outside the pass read it.
    fn views(&self, _declaring: &Declaring<'_>) -> Option<crate::analysis::views::Views> {
        None
    }

    /// Phase one: the namespaces this module conjures, before anything is declared inside them.
    ///
    /// Separate from [`Knowledge::declare`] because a generator keyed by a **directory** rather
    /// than by a file can never merge into another's document, and because the list reads as it
    /// runs — the namespaces, then the things inside them.
    fn conjure(&mut self, _declaring: &Declaring<'_>, _into: &mut Declared) -> Counted {
        Counted::new()
    }

    /// Phase two: everything this module declares from the documents it was handed.
    fn declare(&mut self, _declaring: &Declaring<'_>, _into: &mut Declared) -> Counted {
        Counted::new()
    }

    /// Phase three: what this module derives from what **every** module said.
    ///
    /// Last, and nothing after it may declare: a `delegate` whose target is another generator's
    /// member would otherwise be deriving from a fact that had not been said when it asked, which
    /// is the ordering assumption [`Facts::returns`] exists to remove.
    fn derive(&mut self, _declaring: &Declaring<'_>, _into: &mut Declared) -> Counted {
        Counted::new()
    }

    /// An empty projection of this module's own, for the walk to merge into.
    ///
    /// `None` for a module that projects nothing, which is the ordinary case for one whose
    /// generators read only the documents on its lists.
    fn projection(&self) -> Option<Box<dyn Projects>> {
        None
    }

    /// Whether a class with this superclass and these mixins is one of the module's own.
    ///
    /// Answers [`Wants::inherits`], and defaults to no so that a module with no such convention
    /// writes nothing.
    fn claims_by_ancestry(&self, _superclass: Option<&str>, _mixins: &[String]) -> bool {
        false
    }
}

/// Every body of knowledge this build has.
///
/// **The registry is the only place core names a module**, and it is deliberately not in the
/// pass: `Analysis::synthesize` takes a `&Registry` and never a module. A build with an empty
/// one compiles, runs, and declares nothing — which is the property that says the seam is real
/// rather than asserted, and there is a test that holds it.
#[derive(Default)]
pub struct Registry {
    modules: Vec<Box<dyn Knowledge>>,
    /// Every row of every module's [`Knowledge::wants`], flattened once.
    ///
    /// Flattened here because the pass zips it against a per-row filter and a per-row `bool`
    /// array, and rebuilding that per document is the shape `Filters` already exists to avoid.
    /// The index into this vector is the index into all of those, which is why it is built once
    /// and read by reference everywhere.
    wants: Vec<&'static Wants>,
    /// Which module supplied each row of [`Self::wants`], by its index in `modules`.
    owner: Vec<usize>,
}

impl Registry {
    /// Build a registry, and refuse two modules that claim one list.
    ///
    /// The collision is a programming error rather than a user's, so it panics at construction
    /// — which happens once per server — rather than answering something arbitrary per keystroke.
    /// A `ListId` is namespaced by its module for exactly this reason, so reaching the panic
    /// takes two modules spelling one namespace.
    #[must_use]
    pub fn new(modules: Vec<Box<dyn Knowledge>>) -> Self {
        let mut wants: Vec<&'static Wants> = Vec::new();
        let mut owner: Vec<usize> = Vec::new();
        let mut claimed: BTreeSet<ListId> = BTreeSet::new();
        for (at, module) in modules.iter().enumerate() {
            for row in module.wants() {
                assert!(
                    claimed.insert(row.list),
                    "two bodies of knowledge claim the list {}; the second is {}",
                    row.list,
                    module.name()
                );
                wants.push(row);
                owner.push(at);
            }
        }
        Self {
            modules,
            wants,
            owner,
        }
    }

    /// A build with nothing registered. What the test for the seam drives.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Every row, in registration order. The index is the key to everything per-row.
    #[must_use]
    pub fn wants(&self) -> &[&'static Wants] {
        &self.wants
    }

    /// Which module supplied row `at`, as an index into [`Self::modules`].
    ///
    /// The index and not the module, for the one caller that keys a per-module array by it:
    /// `Wants::inherits` is answered once per module per document and read once per row.
    #[must_use]
    pub fn module_at(&self, at: usize) -> usize {
        self.owner[at]
    }

    /// The module that supplied row `at`.
    #[must_use]
    pub fn behind(&self, at: usize) -> &dyn Knowledge {
        self.modules[self.owner[at]].as_ref()
    }

    /// Whether the project asked for the body of knowledge one list feeds.
    ///
    /// Asked of the module that owns the list, which is the only thing that can answer it: a list
    /// nobody registered is wanted by nobody, and that is the arm a build with an empty registry
    /// takes for every question.
    #[must_use]
    pub fn wanted(&self, list: ListId, features: Features) -> bool {
        self.wants
            .iter()
            .position(|row| row.list == list)
            .is_some_and(|at| self.behind(at).wanted(list, features))
    }

    /// Every module, in registration order.
    pub fn modules(&self) -> impl Iterator<Item = &dyn Knowledge> {
        self.modules.iter().map(AsRef::as_ref)
    }

    /// And mutably, for the one phase a module writes to itself in: re-reading its own sources.
    pub fn modules_mut(&mut self) -> impl Iterator<Item = &mut Box<dyn Knowledge>> {
        self.modules.iter_mut()
    }

    /// One module back as its own type, for a caller that knows which it wants.
    #[must_use]
    pub fn of<T: 'static>(&self) -> Option<&T> {
        self.modules
            .iter()
            .find_map(|module| module.as_any().downcast_ref::<T>())
    }

    /// How many are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Registry")
            .field(
                "modules",
                &self.modules.iter().map(|m| m.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// What **one document** contributes to a [`Context`], and nothing else.
///
/// The walk is a projection — every field below is filled from this one document — and it is
/// callable per document rather than being one loop that writes straight into the merged
/// [`Context`], because otherwise the cheapest question in the pass is unanswerable: *did the
/// document a keystroke touched contribute anything different?* [`Context::absorb`] is the only
/// thing that merges one.
///
/// **The value itself is what is kept per document, and it answers two questions rather than
/// one.** It is the per-document gate's evidence: a keystroke re-projects the one document it
/// touched and compares it against what that document contributed last time, which is a
/// comparison of *what the document contributes* and not of the document — so a comment, a local
/// variable, a whole method body, anything no field here reads, moves nothing. And it is the
/// **memo** `Analysis::walk` reads: a document rubydex has not re-indexed cannot contribute
/// anything different, so the held value is taken rather than projected again. One map answers
/// both, because the second question is the first one asked of every document at once — and a
/// hash of this struct beside the struct would be a second copy of one fact with a second
/// invalidation rule to get wrong.
///
/// Every field is a `Vec` in the order the document's definitions are recorded, which is
/// deterministic for a given file. The **merge** is what must not depend on the order documents
/// are visited in, and [`Context::absorb`] and [`Context::settle`] are where that is paid for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Contribution {
    /// Which registered lists this document joins.
    pub lists: Vec<ListId>,
    /// `(class, the superclass it names)` — the input to both `superclasses` and `defined_in`,
    /// which are filled by one `if let` and must stay that way.
    pub superclasses: Vec<(String, String)>,
    /// `(name, whether the line said `module`)`, feeding `classes`, `modules` and `namespaces`.
    pub declared: Vec<(String, bool)>,
    /// The same pair for a **gem's** own classes and modules, which feed exactly one thing.
    ///
    /// A separate field and not a wider `declared`, because `classes` and `modules` are read as
    /// *the names this application declares*: `Elsewhere::known` gates a `delegated_type` on them,
    /// `Namespaces` decides what a generated name may be joined onto, and `models_of` climbs
    /// them. Every one of those would answer differently about a project whose bundle happens to
    /// hold a name — which is a change nobody asked for and no measurement behind it. What a gem's
    /// names are needed for is `includers_of`: resolving `include ActiveModel::API` written in
    /// `ActiveRecord::Base` needs both of those names, and needs them for no other reason.
    pub foreign: Vec<(String, bool)>,
    /// `(the body that wrote the `include`, the constant it spelled)`.
    pub included: Vec<(String, String)>,
    /// And what each registered module made of all of the above.
    ///
    /// One entry per module in registration order, so the comparison the cheap gate makes is a
    /// field-by-field one and a module that contributed nothing compares equal to a module that
    /// contributed nothing. Core never looks inside one — see [`Contributed`].
    pub modules: Vec<Contributed>,
}

/// Everything the generators need from the graph, gathered in one pass.
///
/// Nothing here is a decision: which schema files are really schemas, which class claims which
/// table and which association may be believed are all settled by the generator that asks, so
/// that this stays one loop over the documents rather than one per generator.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// The documents on each list, sorted. Absent means empty.
    pub documents: BTreeMap<ListId, Vec<String>>,
    /// Every class or module the application defines — **and every one a Rails engine defines
    /// under its `app/`** — fully spelled.
    ///
    /// The bound on what a macro may name, widened by exactly one directory per gem. A gem that happens to define `class Story` in its `lib/` is still not this
    /// application's model, so an association naming it declares nothing — the same answer a
    /// misspelled one gets, reached without asking whether the spelling was a mistake. What
    /// changed is that `ActiveStorage::Blob` *is* nameable now, because an engine's `has_many
    /// :variant_records` has to reach `ActiveStorage::VariantRecord` and both sit under `app/`.
    ///
    /// Two of this struct's fields deliberately did **not** widen with it — `claims` and
    /// `hosts` — because they mean "the application" rather than "a class the reader can name".
    /// Their docstrings say so at the point they are filled.
    pub classes: BTreeSet<String>,
    /// The subset of `classes` written as `module` rather than as `class`.
    ///
    /// **Not read by the macro reader.** Whether a body is a module is a property of the file
    /// a generator is already reading, so `read_model` learns it from the source and never asks
    /// the graph. What needs this is a question about a name's **namespace**, which is somebody
    /// else's file: `Facts::render` asks it here, because a segment may be joined onto a
    /// generated name only where the application declares it. The concern fan-out wants it for
    /// the other reason — deciding that an `include Storyish` names a concern *this application
    /// defines* is a question about the graph and not about the file the `include` is written
    /// in.
    pub modules: BTreeSet<String>,
    /// Every class a **gem** declares, and every module of one, read only by `includers_of`.
    ///
    /// Deliberately not folded into the two above — [`Contribution::foreign`] says why.
    pub foreign_classes: BTreeSet<String>,
    pub foreign_modules: BTreeSet<String>,
    /// A class the application defines, and the superclass it names, spelled as written.
    ///
    /// A mailer and a job have no macro at all and are recognised by what
    /// they inherit. Spelled as written rather than qualified, because the test is a suffix —
    /// `ApplicationMailer` — and a lexical nesting the reference does not have would only make
    /// the string longer.
    pub superclasses: BTreeMap<String, String>,
    /// A class the application defines, and the document that says what it inherits.
    ///
    /// A document that names a **superclass** rather than any document that reopens the name:
    /// a model reopened in a second file to nest something under it writes its own name twice,
    /// and only one of the two says what it is. It is the same defect `claims` has to guard
    /// against, and the same `if let Some(superclass)` answers both.
    ///
    /// Where more than one still does — solidus reopens `class Spree::Product < Spree::Base` in
    /// its specs — the **lowest URI** wins rather than the last one walked, because this loop
    /// visits the graph's documents in no defined order and a generated document that moves
    /// between runs is a difference waiting to matter. It is `settle`'s rule applied one field
    /// earlier.
    ///
    /// What it is for is emission, and only for a model no macro names: a class no macro
    /// anywhere asks about has no document that asked for its relation, so its own is where the
    /// relation goes. Every element that already had a home **keeps it** —
    /// `rails::model_declarations` has what moving them cost.
    pub defined_in: BTreeMap<String, String>,
    /// What may be spelled around a generated owner, and by whose authority.
    ///
    /// **`classes` and `modules` above answer "may a macro name this"; this answers "may a
    /// generated name be spelled around this", and they are not one question.** Everything in `classes` is in here too, and then
    /// `Analysis::bundle_namespaces` adds what the **bundle** declares about the namespaces
    /// *above* those names — which is the only place a generated name can introduce a segment
    /// nobody wrote.
    ///
    /// The two are deliberately not merged, and the corpus is the argument rather than caution:
    /// at 3,979 association sites over six applications, 541 name a class no file in the
    /// application declares and the whole graph supplies **21 of them, every one a Ruby core
    /// class matched by accident** — `has_many :objects` camelizes to `Object`, nineteen times
    /// in mastodon alone. A macro names this application's models; a namespace belongs to
    /// whoever wrote it.
    pub namespaces: crate::generated::Namespaces,
    /// A concern, and every **class** that includes it — directly, or through another module.
    ///
    /// The one projection here whose value is a set rather than a name: `scope :expired` in
    /// `Expireable` is `Poll.expired` *and* `Invite.expired`, six different relation types for
    /// one line, which is exactly why the macro reader refuses to write it down. A module cannot own the declaration; the includers can, one each.
    ///
    /// **Transitive, and the closure measures nothing.** `ActiveSupport::Concern` chains its
    /// dependencies — a concern that includes a concern hands the inner one's `included` block
    /// to whatever includes the outer — so a one-hop reading would be a rule that is wrong and
    /// cheap. Over the six corpora it changes **0** of the 143 pairs, which makes the closure a
    /// correctness property rather than a count.
    ///
    /// Only classes are values. A module that includes a concern is walked *through* and is
    /// never a target: `Bigger.expired` is not a thing anybody can call, and the class that
    /// includes `Bigger` is where the members really land.
    pub includers: BTreeMap<String, BTreeSet<String>>,
    /// And what each registered module made of all of it.
    ///
    /// One entry per module in registration order, filled by
    /// [`Knowledge::projection`] before the walk starts. Core never looks inside one:
    /// a reader that wants a module's own projection asks for it by that module's type, through
    /// [`Projected::of`].
    pub projections: Vec<Projected>,
}

impl Context {
    /// The documents on one list, or nothing.
    pub fn documents(&self, list: ListId) -> &[String] {
        self.documents.get(&list).map_or(&[], Vec::as_slice)
    }

    /// Whether any generator has anything to read.
    ///
    /// A list whose row says it **reads only** is not counted: `self.table_name=` on its own
    /// declares nothing at all — it renames a table for a schema that has to exist somewhere
    /// else — and neither does any other input to a generator that is itself empty. And a module
    /// may have something of its own that is on no list, which is what `declares_nothing` asks.
    pub fn is_empty(&self, knowledge: &Registry) -> bool {
        let declaring: BTreeSet<ListId> = knowledge
            .wants()
            .iter()
            .filter(|row| !row.reads_only)
            .map(|row| row.list)
            .collect();
        self.documents
            .iter()
            .filter(|(list, _)| declaring.contains(*list))
            .all(|(_, on)| on.is_empty())
            && self.projections.iter().all(|projection| {
                projection
                    .0
                    .as_ref()
                    .is_none_or(|held| held.declares_nothing())
            })
    }

    /// Every file some generator opens, for the pass gate.
    ///
    /// Every list, including the read-only ones [`Context::is_empty`] leaves out: such a list
    /// declares nothing on its own and is still *read*, and this question is about reading. A
    /// module's own non-document inputs are here too — a `db/*structure.sql` is not a graph
    /// document, and it can still be open in an editor, which is the one way it reaches this pass
    /// without the watcher.
    pub fn read_by_a_generator(&self) -> impl Iterator<Item = &str> {
        self.documents.values().flatten().map(String::as_str).chain(
            self.projections
                .iter()
                .filter_map(|projection| projection.0.as_ref())
                .flat_map(|held| held.also_reads())
                .map(DocUri::as_str),
        )
    }

    /// Merge what one document contributes — the **only** place a [`Contribution`] becomes part
    /// of a `Context`.
    ///
    /// **Every line here must be order-independent.** Keeping one value per document is only
    /// sound if the merged answer
    /// is a function of the *set* of contributions and not of the order the graph's map happens
    /// to hand them over in — and that is what the memo rests on as well as the gate, because a
    /// walk that absorbs mostly held values visits them in whatever order the graph hands them
    /// over in. So the two fields that were not are fixed here and in
    /// [`Context::settle`]:
    ///
    /// - **`superclasses` now takes the lowest URI**, which is the rule `defined_in`'s own
    ///   docstring states two lines below where it was written. The two maps are filled by one
    ///   `if let Some(superclass)`, and only one of them was deterministic: solidus really does
    ///   reopen `class Spree::Product < Spree::Base` in its specs, and which of the two lines
    ///   won was whatever the walk reached last.
    /// - **`claims` is sorted** by `settle`, because it is pushed per definition.
    ///
    /// Everything else is a `BTreeSet`, a `BTreeMap` keyed by a name, or a list `settle` sorts.
    pub fn absorb(
        &mut self,
        uri: &str,
        contribution: Contribution,
        included: &mut Vec<(String, String)>,
    ) {
        for list in contribution.lists {
            self.documents.entry(list).or_default().push(uri.to_owned());
        }
        // Each module's own half, handed straight back to the module that made it. Zipped by
        // position, which is registration order on both sides, so core never has to know which
        // projection belongs to whom.
        for (projection, held) in self.projections.iter_mut().zip(&contribution.modules) {
            if let (Some(projection), Some(held)) = (projection.0.as_mut(), held.0.as_ref()) {
                projection.absorb(uri, held.as_ref());
            }
        }
        for (name, superclass) in contribution.superclasses {
            // `<=` and not `<`: a document that already holds the entry is writing its *second*
            // `class Story < ...`, which is one file disagreeing with itself and where the last
            // line is the one Ruby runs.
            if self
                .defined_in
                .get(&name)
                .is_none_or(|held| uri <= held.as_str())
            {
                self.superclasses.insert(name.clone(), superclass);
                self.defined_in.insert(name, uri.to_owned());
            }
        }
        for (name, module) in contribution.declared {
            self.namespaces.declare(name.clone(), module);
            if module {
                self.modules.insert(name.clone());
            }
            self.classes.insert(name);
        }
        for (name, module) in contribution.foreign {
            if module {
                self.foreign_modules.insert(name.clone());
            }
            self.foreign_classes.insert(name);
        }
        included.extend(contribution.included);
    }

    /// An empty projection, with a slot for every registered module's own.
    ///
    /// **The slots are positional and are made here rather than on first use**, because
    /// [`Context::absorb`] zips a document's contributions against them: a `Context` built
    /// without them would silently absorb nothing at all for every module, which is a whole
    /// body of knowledge answering nothing with no error anywhere.
    pub fn new(knowledge: &Registry) -> Self {
        Self {
            projections: knowledge
                .modules()
                .map(|module| Projected(module.projection()))
                .collect(),
            ..Self::default()
        }
    }

    /// One module's own projection, by that module's own type.
    ///
    /// The one downcast a reader makes, and core makes none: what is held is `dyn Projects` and
    /// the *caller* names what it wants back.
    pub fn projection<T: 'static>(&self) -> Option<&T> {
        self.projections.iter().find_map(Projected::of)
    }

    /// And mutably, for the fields a projection can only fill once the whole walk is in.
    pub fn projection_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.projections.iter_mut().find_map(Projected::of_mut)
    }

    /// Sort every list, so that a project answers the same way on every run whatever order the
    /// graph's map happens to iterate in. Which file writes a shared relation class depends on
    /// this, and so does which of two schemas is read first.
    pub fn settle(&mut self) {
        for documents in self.documents.values_mut() {
            documents.sort_unstable();
        }
        // And every module's own, for the same reason read one layer out: a projection that
        // varied with the order the graph handed the documents over would make two walks over one
        // unchanged workspace produce two `Context`s, which is what the wide gate compares and
        // what the per-document contributions stand on.
        for projection in &mut self.projections {
            if let Some(projection) = projection.0.as_mut() {
                projection.settle();
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    struct Pretend(&'static str, &'static [Wants]);

    impl Knowledge for Pretend {
        fn name(&self) -> &'static str {
            self.0
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn wants(&self) -> &'static [Wants] {
            self.1
        }
        fn wanted(&self, _list: ListId, features: Features) -> bool {
            features.structs
        }
    }

    const fn row(list: ListId) -> Wants {
        Wants {
            list,
            calls: &[],
            constants: &[],
            modules: &[],
            defines: &[],
            path: None,
            tags: false,
            inherits: false,
            engines: false,
            gems: false,
            reads_only: false,
        }
    }

    static ONE: [Wants; 1] = [row(ListId("pretend.one"))];
    static TWO: [Wants; 2] = [row(ListId("pretend.two")), row(ListId("pretend.three"))];
    static SAME: [Wants; 1] = [row(ListId("pretend.one"))];

    /// The seam, stated as a test: a build that registers nothing still builds.
    ///
    /// The one thing core is allowed to know about its modules is the line that registers them,
    /// and that line is not in the pass — so removing it has to leave something that compiles and
    /// answers *nothing wanted* to every question, rather than something that does not build.
    #[test]
    fn a_build_with_no_body_of_knowledge_registered_wants_nothing() {
        let registry = Registry::empty();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.wants().is_empty());
        assert!(registry.modules().next().is_none());
        assert!(!registry.wanted(ListId("rails.models"), everything()));
        assert_eq!(format!("{registry:?}"), "Registry { modules: [] }");
    }

    #[test]
    fn a_row_knows_which_module_supplied_it() {
        let registry = Registry::new(vec![
            Box::new(Pretend("first", &ONE)),
            Box::new(Pretend("second", &TWO)),
        ]);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.wants().len(), 3);
        assert_eq!(registry.behind(0).name(), "first");
        assert_eq!(registry.behind(2).name(), "second");
        assert_eq!(registry.module_at(2), 1);
        // And the switch is the owning module's to answer, not core's.
        assert!(registry.wanted(ListId("pretend.three"), everything()));
        assert!(!registry.wanted(ListId("pretend.three"), nothing()));
        // A list nobody registered is wanted by nobody, which is the same arm as an empty build.
        assert!(!registry.wanted(ListId("pretend.absent"), everything()));
    }

    /// A `ListId` is namespaced by its module so this takes two modules spelling one namespace,
    /// which is a programming error rather than a user's — so it fails at construction, once per
    /// server, rather than answering something arbitrary per keystroke.
    #[test]
    #[should_panic(expected = "two bodies of knowledge claim the list pretend.one")]
    fn two_bodies_of_knowledge_cannot_claim_one_list() {
        let _ = Registry::new(vec![
            Box::new(Pretend("first", &ONE)),
            Box::new(Pretend("second", &SAME)),
        ]);
    }

    /// The registry the server really builds, and the property clause 2 of the item is about.
    ///
    /// Two halves. **Every row in play came from a module that registered it**, checked by the
    /// namespace on its id rather than by a list written out here — a list core kept its own copy
    /// of would be the coupling this seam exists to remove. And **every switch is answered by the
    /// module that owns the list**, which is what makes `features.md`'s table a property of the
    /// modules rather than of the pass.
    #[test]
    fn every_list_in_play_came_from_a_module_that_registered_it() {
        let registry = Registry::new(vec![
            Box::new(rails::Rails::default()),
            Box::new(annotations::Annotations::default()),
            Box::new(structs::Structs),
        ]);
        assert_eq!(
            registry.modules().map(Knowledge::name).collect::<Vec<_>>(),
            vec!["rails", "annotations", "structs"]
        );
        for (at, row) in registry.wants().iter().enumerate() {
            let module = registry.behind(at);
            assert!(
                row.list.0.starts_with(module.name()),
                "{} is not {}'s to register",
                row.list,
                module.name()
            );
        }

        // The switches, one flag at a time, read through whichever module owns the list.
        let only = |set: fn(&mut Features)| {
            let mut features = nothing();
            set(&mut features);
            features
        };
        for (list, features) in [
            (rails::SCHEMAS, only(|f| f.schema = true)),
            (rails::RENAMED, only(|f| f.schema = true)),
            (rails::MODELS, only(|f| f.models = true)),
            (rails::CONCERNS, only(|f| f.models = true)),
            (rails::ROUTES, only(|f| f.routes = true)),
            (rails::ENTRYPOINTS, only(|f| f.entrypoints = true)),
            (rails::FRAMEWORK, only(|f| f.rails = true)),
            (annotations::ANNOTATED, only(|f| f.annotations = true)),
            (structs::STRUCTS, only(|f| f.structs = true)),
        ] {
            assert!(registry.wanted(list, features), "{list} was not wanted");
            assert!(
                !registry.wanted(list, nothing()),
                "{list} was wanted anyway"
            );
        }

        // A list nobody registered is nobody's, and so is one asked of the wrong module — the
        // arm each `wanted` ends with. The registry never reaches it, because it finds the row
        // before it asks; it is there so that adding a row and forgetting its switch declines
        // rather than answers something arbitrary.
        let elsewhere = ListId("rspec.groups");
        assert!(!registry.wanted(elsewhere, all()));
        assert!(!rails::Rails::default().wanted(elsewhere, all()));
        assert!(!annotations::Annotations::default().wanted(elsewhere, all()));
        assert!(!structs::Structs.wanted(elsewhere, all()));
        // And the one predicate whose question core cannot ask is the module's.
        assert!(rails::Rails::default().claims_by_ancestry(Some("ApplicationJob"), &[]));
        assert!(
            !annotations::Annotations::default().claims_by_ancestry(Some("ApplicationJob"), &[])
        );

        // Each comes back as its own type, which is how a caller that knows which it wants reads
        // the memo that module keeps. A type nobody registered comes back as nothing.
        assert!(registry.of::<rails::Rails>().is_some());
        assert!(registry.of::<annotations::Annotations>().is_some());
        assert!(registry.of::<structs::Structs>().is_some());
        assert!(registry.of::<rspec::RSpec>().is_none());
    }

    /// Clause 3 of the item: a second body of knowledge, and what it costs to add one.
    ///
    /// **Zero files outside its own.** `rspec.rs` registers a list, a row and a generator, and
    /// touches no core file — not this one, not the pass, not `Features`, not `Source`. Before the
    /// registry it would have taken six: the `List` enum, the `WANTS` table, the feature gate, the
    /// `Context`, the `Contribution` and the pass's hand-written ordering.
    #[test]
    fn a_second_body_of_knowledge_costs_nothing_outside_its_own_file() {
        let registry = Registry::new(vec![Box::new(rspec::RSpec)]);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.wants().len(), 1);
        assert_eq!(registry.wants()[0].list, rspec::GROUPS);
        assert_eq!(registry.behind(0).name(), "rspec");
        // It wants its own list and nothing else's, which is the answer an empty build gives to
        // every question.
        assert!(registry.wanted(rspec::GROUPS, nothing()));
        assert!(!registry.wanted(rails::MODELS, all()));
        // And it registers beside the three that ship, without either noticing.
        let both = Registry::new(vec![
            Box::new(rails::Rails::default()),
            Box::new(annotations::Annotations::default()),
            Box::new(structs::Structs),
            Box::new(rspec::RSpec),
        ]);
        assert_eq!(both.len(), 4);
        assert_eq!(both.wants().len(), 10);
        assert!(both.wanted(rspec::GROUPS, nothing()));
        assert!(both.wanted(rails::MODELS, all()));
    }

    fn all() -> Features {
        Features {
            rails: true,
            schema: true,
            models: true,
            routes: true,
            entrypoints: true,
            views: true,
            structs: true,
            annotations: true,
        }
    }

    fn everything() -> Features {
        Features {
            structs: true,
            ..nothing()
        }
    }

    fn nothing() -> Features {
        Features {
            rails: false,
            schema: false,
            models: false,
            routes: false,
            entrypoints: false,
            views: false,
            structs: false,
            annotations: false,
        }
    }
}
