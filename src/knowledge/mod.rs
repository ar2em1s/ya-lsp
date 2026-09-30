//! What a body of knowledge is, and everything the generator pass knows about one.
//!
//! The pass reads what the workspace declares about itself and writes the RBS it implies. *What* it
//! reads is not its business: a schema, a `Struct.new`, a Sorbet `sig` and an RSpec `describe` are
//! four bodies of knowledge with one shape (some documents, a projection of them, and
//! [`Facts`](crate::generated::Facts) at the end). This module writes that shape down, so the pass
//! can drive a module it has never heard of.
//!
//! # The three contracts
//!
//! - **Output**: [`generated::Facts`](crate::generated). `Owner`, `Declared`, the collision rules
//!   and `render` name no framework, and every generator ends there.
//! - **Place**: [`synthesized::Mapping`](crate::analysis::synthesized). A generated declaration's
//!   source is a `(uri, full, selection)` triple and nothing more.
//! - **Input**, this module: which documents a module is handed and what it may ask about them.
//!   [`Wants`] is the vocabulary, a module supplies the rows, and [`Registry`] is the only place
//!   core names a module. The schedule (the order modules run in, and the two channels they talk
//!   through) is here too.
//!
//! # What core still legitimately knows
//!
//! [`Source`](crate::generated::Source)'s rank. Precedence between two generated declarations of
//! one member is a **total order across modules** (a column beats an `attribute` beats a
//! `delegate`), so a module cannot pick its own number without making the ladder unreadable. It
//! stays one table, reviewed whole. That coupling is honest; hiding it behind a registry would be
//! worse.

pub mod annotations;
pub mod defines;
pub mod factories;
pub mod i18n;
pub mod mixins;
pub mod rails;
pub mod rspec;
pub mod singletons;
pub mod structs;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::generated::{At, Facts};
use crate::workspace::{DocUri, Features};

/// Which projection a module is asking for, as the module spells it.
///
/// A string, not an enum variant, because core must hold lists it has never heard of. A
/// `&'static str` keeps it cheap (comparing two is a pointer-length compare), and the log line
/// prints it as written. Namespaced by the owning module (`rails.schemas`) so two modules cannot
/// collide by accident, and the registry asserts they have not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListId(pub &'static str);

impl std::fmt::Display for ListId {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(self.0)
    }
}

/// The test that puts a document on one list.
///
/// **The vocabulary is core's; the rows are the module's.** Every predicate filters the definitions
/// and references indexing already recorded, so none opens a file or names a framework, and a
/// generator then reads only files that really call one of these. A module supplies which names go
/// in the slots.
pub struct Wants {
    pub list: ListId,
    /// Receiverless call names that put a document on the list.
    pub calls: &'static [&'static str],
    /// Constant names that put a document on the list, matched on the last segment.
    ///
    /// Matching the last segment means somebody's own `Foo::Struct` puts its file on the list too,
    /// which costs one parse and then declines. Every filter here errs that way, because the
    /// alternative is missing `::Struct` and every spelling nobody thought of.
    pub constants: &'static [&'static str],
    /// Namespace names whose **definition** puts a document on the list, matched on the last
    /// segment.
    ///
    /// The only predicate that reads a `module` rather than a call, a reference or a `def`. It
    /// exists for the same reason as `defines`: a body whose whole content is a hand-written
    /// `module ClassMethods` calls nothing, references nothing and defines no singleton method, so
    /// no other test would list it.
    pub modules: &'static [&'static str],
    /// Singleton method names whose **definition** puts a document on the list.
    ///
    /// The receiver is checked, because an instance method of that name is a different method.
    pub defines: &'static [&'static str],
    /// A file-name suffix that puts a document on the list.
    pub path: Option<&'static str>,
    /// Text that puts a document on the list, wherever it is written.
    ///
    /// For a call rubydex records nothing of: `Post.include(M)` leaves two constant references and
    /// no method reference, so `calls` cannot see it. Looked for **by the indexer**, which holds each
    /// file's text anyway (`indexer::spells`), never by the walk, which has none: reading every
    /// application file again to look cost the largest corpus's cold open a third of a second. At most 64
    /// across the registry ([`Registry::spelled`]).
    pub spells: &'static [&'static str],
    /// Whether a `@return`/`@param` tag in a comment above a `def` puts it on the list.
    pub tags: bool,
    /// Whether a class this document defines, matching the module's own superclass test, puts it on
    /// the list.
    ///
    /// The one predicate whose *question* core cannot ask, because "is this class one of mine" is
    /// the module's convention. Core hands the module the superclass and mixins it read; see
    /// [`Knowledge::claims_by_ancestry`].
    pub inherits: bool,
    /// Whether a **Rails engine's** document may go on this list, or only the user's own code.
    ///
    /// Framework-shaped in name, not in meaning: it is *a tree the workspace ships that is not the
    /// workspace*. A list is open to one when what it reads declares members on a class the reader
    /// can name.
    pub engines: bool,
    /// Whether a **gem's own `lib/`** may go on this list.
    pub gems: bool,
    /// Whether a document on this list is **read** without anything being declared from it.
    ///
    /// Input to a generator that declares nothing by itself: `self.table_name=` renames a table for
    /// a schema that must exist elsewhere, so a workspace whose only listed documents are these has
    /// nothing for the pass to do. [`Context::is_empty`] relies on that;
    /// [`Context::read_by_a_generator`] deliberately does not, because such a file is still read
    /// and the gate that watches generator reads must watch it.
    pub reads_only: bool,
    /// Whether only the documents **the editor holds** are read from this list.
    ///
    /// For a module whose output a closed document has no reader for. Opening or closing one then
    /// changes what the pass reads though no text moved, which no other gate can see: the graph
    /// already holds the text, so nothing is re-indexed. `Analysis` marks such a document touched
    /// on `didOpen` and `didClose`.
    ///
    /// **A module with such a list declares apart**: after every other module, into a map of its
    /// own, reading nobody's facts and read by nobody's phases. That makes its output a function of
    /// its own inputs, so a settle whose only news is a reopened file re-runs it alone.
    pub buffers: bool,
}

/// What one document contributes to one module's own projection.
///
/// A trait object, because what Rails wants per document is heterogeneous (an owner, a
/// `(name, span)` pair, two kinds of string pair), and flattening it into one generic value would
/// lose information rather than decouple.
///
/// **The pass's cheap gate rests on these two methods.** `Analysis::walk` holds one contribution
/// per document, and `context_would_be_the_same` compares the held one with a fresh projection. A
/// module whose `same_as` is wrong makes the gate say *nothing moved* about a document that did.
/// That is the one silent bug possible in this file, so both methods are tested, not assumed.
pub trait Contributes: std::fmt::Debug {
    /// Whether this says exactly what `other` says. `false` for a different module's value.
    fn same_as(&self, other: &dyn Contributes) -> bool;
    /// A copy, because the walk keeps one and the merge takes one.
    fn clone_box(&self) -> Box<dyn Contributes>;
    /// For the module to get its own type back out of what core held for it.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Everything one module made of every document: its half of a [`Context`](crate::analysis).
///
/// The merge of a pass's worth of [`Contributes`], under the same two rules: the pass's wide gate
/// compares two whole projections, so `same_as` decides whether a settle is skipped.
///
/// **`absorb` must not depend on document order.** The walk visits documents in whatever order the
/// graph holds them, and the memo mixes held and fresh contributions, so a projection sorted by
/// arrival would answer differently on two settles over one unchanged workspace. [`Self::settle`]
/// repairs any order-dependent field.
pub trait Projects: std::fmt::Debug {
    /// Take what one document contributed. Called once per document, in no particular order.
    fn absorb(&mut self, uri: &str, contribution: &dyn Contributes);
    /// Put right whatever the merge could only decide once all of it had arrived.
    fn settle(&mut self) {}
    /// Whether there is nothing here for a generator to declare from.
    ///
    /// Asked alongside the lists, because a module may hold something on no list at all: what a
    /// directory conjures comes from paths, not documents.
    fn declares_nothing(&self) -> bool {
        true
    }

    /// Files this module reads that are not graph documents at all.
    ///
    /// `db/*structure.sql` is the one, and the reason this exists: nothing else would notice it
    /// change, so the gate that re-reads what generators read has to be told about it.
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
    /// The one downcast in the contract, and the *caller* names the type: core holds a
    /// `dyn Projects` and never learns what is inside.
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

/// Where a name a document declares was written, and where each of a set of names is spelled inside
/// it.
///
/// A type of its own because it travels as a borrowed closure: the pass computes it from the graph,
/// and a module may only ask it. See [`Seen::confirming`].
pub type Confirming<'a> = &'a dyn Fn(&str, &BTreeSet<&str>) -> HashMap<String, At>;

/// What a module may read of one document, and that is all it may read.
///
/// **Everything here was already read by the one walk**, which makes a module's projection a fold,
/// not a second pass: `Analysis::walk` visits every document once, records the generic facts below,
/// and hands them back. A module needing something not listed here is asking for a second walk;
/// widen this struct instead of opening the graph.
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
    /// Where a name this document declares was written, and where each of `names` is spelled inside
    /// it.
    ///
    /// The one thing a module cannot fold out of the four lists above, because it is about byte
    /// offsets, not names. Generic despite having one caller: *the first definition whose parent is
    /// this name, and the earliest reference to each of these inside it*.
    pub confirming: Confirming<'a>,
}

/// How the text of one source file was identified when a module last parsed it.
///
/// Two variants, because the pass has two authorities for what a file says, and they compare
/// differently:
///
/// - A file on disk is identified by modification time and length. That is the pass gate's own
///   evidence, not a second mechanism: the gate makes "the answer is a function of what is on disk"
///   true by looking at the disk, and a weaker memo would break it.
/// - A file the editor holds has no useful stamp (the disk is behind the buffer), so it is
///   identified by its hashed text. Not by version: a client may send `didChange` without one, and
///   two versions of one buffer would then look unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fresh {
    /// A file on disk: when it was written, and how long. `None` for a file that is not there. A
    /// value, not an error, because a deleted schema and one that never existed must both compare
    /// unequal to one that did.
    Disk(Option<(std::time::SystemTime, u64)>),
    /// A file the editor holds, hashed.
    Buffer(u64),
}

/// What a module may read while it re-parses its own sources.
///
/// **The filesystem stays core's.** A module says which documents it wants and what it made of
/// them, but never opens a file: an open buffer outranks the file on disk, and only the server
/// knows which buffers are open. So the text and its freshness arrive as closures, and the module
/// keeps whatever it parsed.
pub struct Sources<'a> {
    /// What the one walk produced.
    pub context: &'a Context,
    /// Which bodies of knowledge this project asked for.
    pub features: Features,
    /// How the text of a file is identified right now, for a memo to compare against.
    pub fresh: &'a dyn Fn(&DocUri) -> Fresh,
    /// Whether the editor holds a document. A module that reads only open buffers asks this
    /// before [`Self::fresh`], which reads a closed file's modification time: one `stat` for each
    /// of thousands of spec files, to learn each is closed.
    pub held: &'a dyn Fn(&DocUri) -> bool,
    /// The text itself, as the buffer or the file has it. `None` for a file that has gone.
    pub text: &'a dyn Fn(&DocUri) -> Option<String>,
    /// How a file inside the workspace should be spelled to a person: `db/animals_schema.rb`.
    ///
    /// Every reader writes it into a provenance comment. It depends on the URI and the workspace
    /// root, which is core's fact, not a module's.
    pub caption: &'a dyn Fn(&DocUri) -> String,
}

/// Everything a module has already declared, keyed by the source file it came from.
///
/// One entry per source, not per generator: a file feeding two generators gets one generated
/// document holding both, because the side table is keyed by generated URI and a second record for
/// one source would replace the first. [`add`] merges them and enforces precedence between the two
/// generators.
pub type Declared = std::collections::BTreeMap<String, (DocUri, Facts)>;

/// Add one generator's facts to the document its source file names.
///
/// Merging here enforces precedence: a member two generators both name is decided by [`Facts`]'
/// rank instead of written twice as a silent overload. A generator that said nothing adds no entry,
/// so a source nobody had anything to say about is not recorded.
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
/// The one graph question a generator may ask, and deliberately a **name** question, not a resolved
/// one: the pass is writing the text rubydex is about to link, so while a generator runs no name
/// has a resolved answer yet. A name several files reopen answers with the first in URI order:
/// arbitrary, but deterministic.
pub type Declares<'a> = &'a dyn Fn(&BTreeSet<String>) -> BTreeMap<String, DocUri>;

/// Which of a set of constant names some file declares, each with whether it is a `module`.
///
/// [`Declares`]' question with the other half of the answer: the keyword a generated body must
/// open with. For a module writing onto a class it knows only by the name a call spelled
/// (`Paperclip::Attachment.prepend(…)`), which nothing in the [`Context`] has looked up. A name
/// is a `module` only where every file declaring it says so, the rule `Namespaces` keeps.
pub type Kinds<'a> = &'a dyn Fn(&BTreeSet<String>) -> BTreeMap<String, bool>;

/// What a generator may read while it declares.
///
/// What the Rails generators turned out to need: a document's **text**, how a file is **spelled to a
/// person**, whether a document is the **user's own code**, and the **features**; plus two name
/// questions ([`Declares`], [`Kinds`]) and whether the application **loads** a document at all.
/// Anything else a generator wants is in the [`Context`] or in its own memo.
///
/// **No graph and no filesystem.** A generator writes the text rubydex is about to link, so while
/// it runs every question about a gem's classes would answer nothing; and an open buffer outranks
/// the file on disk, which only the server knows. Both are why these arrive as closures, not
/// handles.
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
    /// Which of these names are declared, and as a `module` or not — see [`Kinds`].
    pub kinds: Kinds<'a>,
    /// Whether the application loads a document: not a test tree, a migration or a generator's
    /// template (`environment::Fence::unloadable`'s reading, never a copy of it). For a fact that
    /// changes what the application's own classes are, which a file only the suite loads must not.
    pub loaded: &'a dyn Fn(&DocUri) -> bool,
}

/// What a module may read while it looks for files nothing has indexed.
///
/// The one place a module touches the filesystem, and it goes through core: a `db/*structure.sql`
/// is not a graph document, so nothing else would notice it change or put it on a list.
pub struct Reading<'a> {
    /// The workspace root.
    pub root: &'a std::path::Path,
    /// Whether the project's own `index.include` admits a path.
    pub admits: &'a dyn Fn(&std::path::Path) -> bool,
    /// Which bodies of knowledge this project asked for.
    pub features: Features,
    /// The bundle's gems, as discovery found them: where a gem's own files are.
    pub gems: &'a [crate::workspace::Gem],
    /// What `[i18n]` says: the main locale and where the project's locale files are.
    pub i18n: &'a crate::workspace::config::I18nConfig,
}

/// What one module declared, for the one line that says the pass ran.
///
/// A list of `(what, how many)`, not a struct, because each module counts different things.
pub type Counted = Vec<(&'static str, usize)>;

/// One body of knowledge, and the whole of what the pass knows about it.
pub trait Knowledge {
    /// What it is called, for the log line and for the registry's collision check.
    fn name(&self) -> &'static str;

    /// For a caller that holds the registry and wants one module back as its own type.
    ///
    /// Used by test instruments that add up each module's own read counters.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Which documents it wants, one row per list.
    fn wants(&self) -> &'static [Wants];

    /// Whether this project asked for the list at all.
    ///
    /// The one place a switched-off generator is switched off: [`Wants`] decides which documents
    /// any generator ever sees, so an empty list is a generator that does nothing, with no removal
    /// path to write and nothing downstream to teach.
    fn wanted(&self, list: ListId, features: Features) -> bool;

    /// Names this module declares on or beside, whose namespaces must be spellable.
    ///
    /// `Namespaces::spellable` asks whether every segment **above** a name is declared, and the
    /// answer comes from the bundle, not the application. So a module that writes onto a gem's
    /// class must say which names to look up. Both the name and each of its prefixes are asked,
    /// because a body may be opened for either.
    ///
    /// Empty by default, which suits a module that declares only onto the application's own
    /// classes: those are already in `Context::classes`.
    fn spellable_names(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// The namespaces this module invents, each with what a reader sees in its place:
    /// `(invented, shown)`.
    ///
    /// A generator names a namespace no file declares when Ruby's own has no name or cannot hold
    /// what is written onto it. The name is the graph's, and a reader who types it finds nothing,
    /// so a card spells the class Ruby really builds, or, where `shown` is empty, the member alone
    /// (`render::qualified_name`). Empty by default: most modules write onto real classes.
    fn shown(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// What one document contributes to this module's own projection, or nothing.
    ///
    /// **A fold, never a walk.** Everything in [`Seen`] was already read by the one pass over the
    /// documents. A module that needs a second look at the graph is asking for a second walk; widen
    /// [`Seen`] instead of opening the graph here.
    fn contribute(&self, _seen: &Seen<'_>) -> Option<Box<dyn Contributes>> {
        None
    }

    /// Files this module reads that nothing has indexed and no list can name.
    ///
    /// Asked once per pass, before the projection settles, so what comes back is on the projection
    /// in time for the gate that re-reads what generators read.
    fn discover(&self, _reading: &Reading<'_>) -> Vec<DocUri> {
        Vec::new()
    }

    /// Take what [`Knowledge::discover`] found, so the module can put it on its projection.
    fn discovered(&mut self, _found: Vec<DocUri>) {}

    /// A file nothing indexes changed on disk (a watched event): whether this module reads it, so
    /// the next settle runs the pass again. A module that keeps a walk of such files forgets it
    /// here, so a file added or removed is found.
    fn touched(&mut self, _path: &std::path::Path) -> bool {
        false
    }

    /// Re-read whatever this module parses, before anything declares.
    ///
    /// **The memo is the module's own, and it memoises the *parse*, not the facts.** Memoising
    /// facts is the obvious seam and the worse one: they depend on the text *and* the [`Context`],
    /// so the whole memo would drop whenever the projection moved. A reader that takes only text
    /// has no second input to compare and no invalidation rule to get wrong, and a keystroke adding
    /// a class elsewhere does not throw it away.
    fn refresh(&mut self, _sources: &Sources<'_>) {}

    /// The classes this module's generated members are really defined on, when the definition is in
    /// somebody else's file rather than a line the generator read.
    ///
    /// `[instance side, class side]`, both **names looked up after the resolve**, not matched: a
    /// member is found on one of these or not at all, and not found means no place, like an
    /// unmapped span. ActiveRecord's query interface is the one case: no project file declares
    /// `Story.where`, and activerecord does.
    ///
    /// Empty by default, which suits every module whose generated members' places are all lines it
    /// read.
    fn places_members_on(&self) -> [&'static [&'static str]; 2] {
        [&[], &[]]
    }

    /// Fix up whatever only the **whole** walk decides, before the bundle is asked anything.
    ///
    /// A module may reach into its own projection and its registered lists here: which application
    /// classes are models depends on the chain above each, and the file defining one joins the list
    /// that opens it. Both are decided after every document is seen and before the feature gate
    /// runs, which is why this is a hook and not a phase.
    fn after_the_walk(&self, _context: &mut Context) {}

    /// The same, once the bundle has been asked what it declares.
    ///
    /// Two things need it, both about names, not documents: a directory conjures a name only where
    /// **no `class` declares it**, and which framework classes may be written onto is exactly
    /// which of them the bundle holds. Whatever is returned is declared as a module namespace,
    /// because a module cannot reach `Namespaces` while holding its own projection mutably.
    fn after_the_bundle(&self, _context: &mut Context) -> Vec<String> {
        Vec::new()
    }

    /// What a template's implicit receiver can answer, where this module knows.
    ///
    /// The one output of a body of knowledge that is **not** a declaration: `helper_method` hands
    /// over a permission, and the `def` it names is already indexed. It travels as a core type
    /// because type rungs outside the pass read it.
    fn views(&self, _declaring: &Declaring<'_>) -> Option<crate::analysis::views::Views> {
        None
    }

    /// Phase one: the namespaces this module conjures, before anything is declared inside them.
    ///
    /// Separate from [`Knowledge::declare`] because a generator keyed by a **directory**, not a
    /// file, can never merge into another's document, and because the list then reads in running
    /// order: the namespaces, then what is inside them.
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
    /// member would otherwise derive from a fact not yet said, which is the ordering assumption
    /// [`Facts::returns`] exists to remove.
    fn derive(&mut self, _declaring: &Declaring<'_>, _into: &mut Declared) -> Counted {
        Counted::new()
    }

    /// An empty projection of this module's own, for the walk to merge into.
    ///
    /// `None` for a module that projects nothing, which is normal for one whose generators read
    /// only the documents on its lists.
    fn projection(&self) -> Option<Box<dyn Projects>> {
        None
    }

    /// Where the literal a call passes a member this module declared names something written down:
    /// `create(:user)`'s `:user` is the `factory :user` call. `owner` and `method` spell the
    /// declaration (`FactoryBot::Syntax::Methods`, `create`); the answer is the file's graph URI
    /// and the span of what is written there. Asked by a jump that found the member and no place
    /// for it.
    ///
    /// `None` by default, for every module whose members' places are the lines they read.
    fn literal_place(&self, _owner: &str, _method: &str, _literal: &str) -> Option<(String, At)> {
        None
    }

    /// The RBS type of what a call's literal key names, where this module keeps a table of keys
    ///: `t("users.show.title")` is a `String` where the main locale holds one. Asked
    /// for a member whose signature returns [`crate::generated::KEYED`]; `None` answers nothing.
    fn keyed_type(&self, _keyed: &Keyed<'_>) -> Option<&'static str> {
        None
    }

    /// Where a call's literal key is written and what it holds, for a jump and a card.
    fn keyed_entry(&self, _keyed: &Keyed<'_>) -> Option<KeyedEntry> {
        None
    }

    /// The keys one step under `prefix` (dotted, `""` for the top), for completion inside a call's
    /// literal key, where `member` is one this module keeps keys for.
    fn keyed_under(&self, _member: &str, _prefix: &str) -> Option<Vec<KeyedChild>> {
        None
    }

    /// The declaration whose Ruby `def` is the code a member this module declared without a place
    /// really runs: RSpec's `expect` is the `def expect` rspec-expectations writes inside a
    /// `module_exec`, which rubydex files under `RSpec::Expectations::Syntax`. `owner` and
    /// `method` spell the member's declaration, as for [`Self::literal_place`]; the answer is a
    /// declaration name, `RSpec::Expectations::Syntax#expect()`. Asked by a jump that found the
    /// member and no place for it.
    ///
    /// `None` by default, for every module whose members are placed at the lines they read.
    fn written_in(&self, _owner: &str, _method: &str) -> Option<String> {
        None
    }

    /// Whether a class with this superclass and these mixins is one of the module's own.
    ///
    /// Answers [`Wants::inherits`]. Defaults to no, so a module with no such convention writes
    /// nothing.
    fn claims_by_ancestry(&self, _superclass: Option<&str>, _mixins: &[String]) -> bool {
        false
    }
}

/// A call whose first argument is a literal key: what [`Knowledge::keyed_type`] and its siblings
/// read. Everything here is the call as written; what the key means is the module's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyed<'a> {
    /// The member the call reached, as its declaration is named: `I18n::Base#t()`.
    pub member: &'a str,
    /// The literal key: a string's or a symbol's text.
    pub key: &'a str,
    /// Each keyword the call writes, and what its value is where the text says.
    pub keywords: &'a [(String, Written)],
    /// Whether the call writes a block, which a lookup may hand the translation to.
    pub block: bool,
}

/// A keyword's value as the call writes it, where that is a literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// A string literal, and its text.
    Text(String),
    /// A symbol literal, and its name.
    Symbol(String),
    /// Anything else: a variable, a call, an interpolation.
    Other,
}

/// Where a key is written and what it holds, for a jump and a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedEntry {
    /// The file that writes it, as a graph URI, and the span of the key's name there.
    pub uri: String,
    pub at: (u32, u32),
    /// What it holds, as the YAML a card shows, the key first (`i18n::yaml`).
    pub shown: String,
}

/// One key under a prefix, for completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedChild {
    pub name: String,
    /// Whether more keys lie under it, which completes as a segment rather than a key's end.
    pub branch: bool,
    /// What it holds, for the item's detail.
    pub shown: String,
}

/// Every body of knowledge this build has.
///
/// **The registry is the only place core names a module**, and it is deliberately outside the pass:
/// `Analysis::synthesize` takes a `&Registry`, never a module. A build with an empty registry
/// compiles, runs and declares nothing, which proves the seam is real, and a test holds that.
#[derive(Default)]
pub struct Registry {
    modules: Vec<Box<dyn Knowledge>>,
    /// Every row of every module's [`Knowledge::wants`], flattened once.
    ///
    /// Flattened here because the pass zips it against a per-row filter and a per-row `bool` array,
    /// and rebuilding that per document is what `Filters` avoids. The index into this vector is the
    /// index into all of those, so it is built once and read by reference everywhere.
    wants: Vec<&'static Wants>,
    /// Which module supplied each row of [`Self::wants`], by its index in `modules`.
    owner: Vec<usize>,
}

/// [`Registry::spelled`], for a registry still being built.
fn spelled_in(wants: &[&'static Wants]) -> Vec<&'static str> {
    let mut spelled: Vec<&'static str> = Vec::new();
    for text in wants.iter().flat_map(|row| row.spells) {
        if !spelled.contains(text) {
            spelled.push(text);
        }
    }
    spelled
}

impl Registry {
    /// Build a registry, and refuse two modules that claim one list.
    ///
    /// A collision is a programming error, not a user's, so it panics at construction (once per
    /// server) instead of answering something arbitrary per keystroke. A `ListId` is namespaced by
    /// its module for this reason, so the panic takes two modules spelling one namespace.
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
        assert!(
            spelled_in(&wants).len() <= 64,
            "more than 64 `Wants::spells` texts; the indexer keeps them as bits of a u64"
        );
        Self {
            modules,
            wants,
            owner,
        }
    }

    /// Every [`Wants::spells`] text in the registry, once each, in registration order: what the
    /// indexer looks for in each file it reads (`indexer::spells`), each one bit of a `u64`.
    #[must_use]
    pub fn spelled(&self) -> Vec<&'static str> {
        spelled_in(&self.wants)
    }

    /// Row `at`'s [`Wants::spells`] as bits of the mask [`Self::spelled`] numbers.
    #[must_use]
    pub fn spelled_by(&self, at: usize) -> u64 {
        let spelled = self.spelled();
        self.wants[at]
            .spells
            .iter()
            .filter_map(|text| spelled.iter().position(|known| known == text))
            .fold(0, |mask, bit| mask | 1 << bit)
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
    /// The index, not the module, for the one caller that keys a per-module array by it:
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
    /// Asked of the module that owns the list, the only one that can answer: a list nobody
    /// registered is wanted by nobody, which is the answer an empty registry gives to everything.
    #[must_use]
    pub fn wanted(&self, list: ListId, features: Features) -> bool {
        self.wants
            .iter()
            .position(|row| row.list == list)
            .is_some_and(|at| self.behind(at).wanted(list, features))
    }

    /// Every module's [`Knowledge::shown`], in registration order.
    #[must_use]
    pub fn shown(&self) -> Vec<(&'static str, &'static str)> {
        self.modules
            .iter()
            .flat_map(|module| module.shown().iter().copied())
            .collect()
    }

    /// Every module, in registration order.
    pub fn modules(&self) -> impl Iterator<Item = &dyn Knowledge> {
        self.modules.iter().map(AsRef::as_ref)
    }

    /// And mutably, for the one phase in which a module writes to itself: re-reading its own
    /// sources.
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
/// Every field below is filled from this one document, and it is computed per document, not in one
/// loop writing straight into the merged [`Context`], because otherwise the pass's cheapest
/// question has no answer: *did the document a keystroke touched contribute anything different?*
/// Only [`Context::absorb`] merges one.
///
/// **The kept value answers two questions:**
///
/// - **The per-document gate's evidence.** A keystroke re-projects the touched document and
///   compares it with last time's contribution. That compares *what the document contributes*, not
///   the document, so a comment, a local variable or a method body (anything no field reads) moves
///   nothing.
/// - **The memo `Analysis::walk` reads.** A document rubydex has not re-indexed cannot contribute
///   anything different, so the held value is reused instead of projected again.
///
/// One map answers both, because the second question is the first asked of every document at once;
/// a hash beside the struct would be a second copy of one fact, with a second invalidation rule to
/// get wrong.
///
/// Each field is a `Vec` in the order the document's definitions are recorded, which is
/// deterministic for a given file. The **merge** must not depend on document visit order;
/// [`Context::absorb`] and [`Context::settle`] handle that.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Contribution {
    /// Which registered lists this document joins.
    pub lists: Vec<ListId>,
    /// `(class, the superclass it names)`: the input to both `superclasses` and `defined_in`, which
    /// are filled by one `if let` and must stay that way.
    pub superclasses: Vec<(String, String)>,
    /// `(name, whether the line said `module`)`, feeding `classes`, `modules` and `namespaces`.
    pub declared: Vec<(String, bool)>,
    /// The same pair for a **gem's** own classes and modules, which feed exactly one thing.
    ///
    /// A separate field, not a wider `declared`, because `classes` and `modules` mean *the names
    /// this application declares*: `Elsewhere::known` gates a `delegated_type` on them,
    /// `Namespaces` decides what a generated name may be joined onto, and `models_of` climbs them.
    /// All of those would change for a project whose bundle happens to hold a name. A gem's names
    /// are needed only by `includers_of`: resolving `include ActiveModel::API` written in
    /// `ActiveRecord::Base` needs both names.
    pub foreign: Vec<(String, bool)>,
    /// `(the body that wrote the `include`, the constant it spelled)`.
    pub included: Vec<(String, String)>,
    /// And what each registered module made of all of the above.
    ///
    /// One entry per module in registration order, so the cheap gate compares field by field, and
    /// two modules that contributed nothing compare equal. Core never looks inside one; see
    /// [`Contributed`].
    pub modules: Vec<Contributed>,
}

/// Everything the generators need from the graph, gathered in one pass.
///
/// Nothing here is a decision: which schema files are really schemas, which class claims which
/// table, and which association may be believed are settled by the generator that asks, so this
/// stays one loop over the documents instead of one per generator.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// The documents on each list, sorted. Absent means empty.
    pub documents: BTreeMap<ListId, Vec<String>>,
    /// Every class or module the application defines, **plus every one a Rails engine defines under
    /// its `app/`**, fully spelled.
    ///
    /// The bound on what a macro may name, widened by exactly one directory per gem. A gem that
    /// defines `class Story` in its `lib/` is still not this application's model, so an association
    /// naming it declares nothing, the same answer a misspelling gets. `ActiveStorage::Blob` *is*
    /// nameable, because an engine's `has_many :variant_records` must reach
    /// `ActiveStorage::VariantRecord`, and both sit under `app/`.
    ///
    /// Two fields of this struct deliberately do **not** widen with it, `claims` and `hosts`,
    /// because they mean "the application", not "a class the reader can name". Their docs say so
    /// where they are filled.
    pub classes: BTreeSet<String>,
    /// The subset of `classes` written as `module` rather than `class`.
    ///
    /// **Not read by the macro reader.** Whether a body is a module is a property of the file a
    /// generator is reading, so `read_model` learns it from the source. This answers questions
    /// about a name's **namespace**, which is somebody else's file: `Facts::render` asks it because
    /// a segment may be joined onto a generated name only where the application declares it. The
    /// concern fan-out uses it too: whether `include Storyish` names a concern *this application
    /// defines* is a question about the graph, not the file the `include` is in.
    pub modules: BTreeSet<String>,
    /// Every class and module a **gem** declares, read only by `includers_of`.
    ///
    /// Deliberately not folded into the two above; [`Contribution::foreign`] says why.
    pub foreign_classes: BTreeSet<String>,
    pub foreign_modules: BTreeSet<String>,
    /// A class the application defines, and the superclass it names, spelled as written.
    ///
    /// A mailer and a job have no macro and are recognised by what they inherit. Spelled as
    /// written, not qualified, because the test is a suffix (`ApplicationMailer`), and a lexical
    /// nesting the reference lacks would only make the string longer.
    pub superclasses: BTreeMap<String, String>,
    /// A class the application defines, and the document that says what it inherits.
    ///
    /// The document that names a **superclass**, not any document that reopens the name: a model
    /// reopened in another file to nest something writes its own name twice, and only one says what
    /// it is. `claims` guards against the same thing, and one `if let Some(superclass)` answers
    /// both.
    ///
    /// Where several do (an engine reopening `class Spree::Product < Spree::Base` in its specs),
    /// the **lowest URI** wins, not the last one walked, because the walk visits documents in no
    /// defined order and a generated document that moves between runs is a bug waiting to happen.
    /// It is `settle`'s rule, applied one field earlier.
    ///
    /// It is for emission, and only for a model no macro names: nothing asked for such a model's
    /// relation, so its own document is where the relation goes. Every element that already has a
    /// home **keeps it**; `knowledge::rails`' `model_declarations` explains why moving them costs
    /// answers.
    pub defined_in: BTreeMap<String, String>,
    /// What may be spelled around a generated owner, and on whose authority.
    ///
    /// **`classes` and `modules` answer "may a macro name this"; this answers "may a generated name
    /// be spelled around this". They are different questions.** Everything in `classes` is here
    /// too, and `Analysis::bundle_namespaces` adds what the **bundle** declares about namespaces
    /// *above* those names, the only place a generated name could introduce a segment nobody wrote.
    ///
    /// They are deliberately not merged. Macro names that the application does not declare but the
    /// bundle does are almost always Ruby core classes matched by accident: `has_many :objects`
    /// camelizes to `Object`. A macro names this application's models; a namespace belongs to
    /// whoever wrote it.
    pub namespaces: crate::generated::Namespaces,
    /// A concern, and every **class** that includes it, directly or through another module.
    ///
    /// The one projection whose value is a set, not a name: `scope :expired` in `Expireable` is
    /// `Poll.expired` *and* `Invite.expired`, different relation types for one line, which is why
    /// the macro reader refuses to write it on the module. The module cannot own the declaration;
    /// the includers can, one each.
    ///
    /// **Transitive, as a correctness property.** `ActiveSupport::Concern` chains dependencies (a
    /// concern including a concern hands the inner one's `included` block to whatever includes the
    /// outer), so a one-hop reading would be cheap and wrong, even though real projects rarely hit
    /// the difference.
    ///
    /// Only classes are values. A module that includes a concern is walked *through*, never a
    /// target: `Bigger.expired` is not callable, and the members really land on the class that
    /// includes `Bigger`.
    pub includers: BTreeMap<String, BTreeSet<String>>,
    /// And what each registered module made of all of it.
    ///
    /// One entry per module in registration order, filled by [`Knowledge::projection`] before the
    /// walk starts. Core never looks inside: a reader wanting a module's projection asks by that
    /// module's type, through [`Projected::of`].
    pub projections: Vec<Projected>,
}

impl Context {
    /// The documents on one list, or nothing.
    pub fn documents(&self, list: ListId) -> &[String] {
        self.documents.get(&list).map_or(&[], Vec::as_slice)
    }

    /// Whether any generator has anything to read.
    ///
    /// A list whose row **reads only** is not counted: `self.table_name=` alone declares nothing
    /// (it renames a table for a schema that must exist elsewhere), and neither does any other
    /// input to a generator that declares nothing by itself. A module may also hold something on no
    /// list, which `declares_nothing` asks about.
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
    /// Every list, including the read-only ones [`Context::is_empty`] skips: such a list declares
    /// nothing on its own but is still *read*, and this is about reading. A module's non-document
    /// inputs are here too: a `db/*structure.sql` is not a graph document, yet it can be open in an
    /// editor, which is the one way it reaches this pass without the watcher.
    pub fn read_by_a_generator(&self) -> impl Iterator<Item = &str> {
        self.documents.values().flatten().map(String::as_str).chain(
            self.projections
                .iter()
                .filter_map(|projection| projection.0.as_ref())
                .flat_map(|held| held.also_reads())
                .map(DocUri::as_str),
        )
    }

    /// Every file some generator reads **from disk**, for the stamps the pass gate compares.
    ///
    /// [`Context::read_by_a_generator`] less the lists whose row reads only what the editor holds
    /// ([`Wants::buffers`]): an open document on one is read from its buffer, whose edits touch
    /// it, and a closed one is not read at all. A document on another list too is still here.
    pub fn read_from_disk<'a>(&'a self, knowledge: &Registry) -> impl Iterator<Item = &'a str> {
        let held_only: BTreeSet<ListId> = knowledge
            .wants()
            .iter()
            .filter(|row| row.buffers)
            .map(|row| row.list)
            .collect();
        self.documents
            .iter()
            .filter(move |(list, _)| !held_only.contains(*list))
            .flat_map(|(_, on)| on)
            .map(String::as_str)
            .chain(
                self.projections
                    .iter()
                    .filter_map(|projection| projection.0.as_ref())
                    .flat_map(|held| held.also_reads())
                    .map(DocUri::as_str),
            )
    }

    /// Merge what one document contributes: the **only** place a [`Contribution`] becomes part of a
    /// `Context`.
    ///
    /// **Every line here must be order-independent.** Keeping one value per document is only sound
    /// if the merged answer depends on the *set* of contributions, not the order the graph's map
    /// hands them over. The memo relies on that as much as the gate does, because a walk absorbing
    /// mostly held values visits them in arbitrary order. Two fields need care:
    ///
    /// - **`superclasses` takes the lowest URI**, the same rule as `defined_in`. Both maps are
    ///   filled by one `if let Some(superclass)`, and an engine reopening
    ///   `class Spree::Product < Spree::Base` in its specs would otherwise make the winner
    ///   whichever line the walk reached last.
    /// - **`claims` is sorted** by `settle`, because it is pushed per definition.
    ///
    /// Everything else is a `BTreeSet`, a `BTreeMap` keyed by name, or a list `settle` sorts.
    ///
    /// **Borrowed, and a name copied only where it is new**: the walk absorbs every document's
    /// held contribution on every pass, and a gem's module is declared in hundreds of its files.
    pub fn absorb(
        &mut self,
        uri: &str,
        contribution: &Contribution,
        included: &mut Vec<(String, String)>,
    ) {
        fn insert(set: &mut BTreeSet<String>, name: &str) {
            if !set.contains(name) {
                set.insert(name.to_owned());
            }
        }
        for list in &contribution.lists {
            self.documents
                .entry(*list)
                .or_default()
                .push(uri.to_owned());
        }
        // Each module's own half, handed back to the module that made it. Zipped by position
        // (registration order on both sides), so core never needs to know which projection is
        // whose.
        for (projection, held) in self.projections.iter_mut().zip(&contribution.modules) {
            if let (Some(projection), Some(held)) = (projection.0.as_mut(), held.0.as_ref()) {
                projection.absorb(uri, held.as_ref());
            }
        }
        for (name, superclass) in &contribution.superclasses {
            // `<=`, not `<`: a document that already holds the entry is writing its *second*
            // `class Story < ...`, one file disagreeing with itself, where the last line is the one
            // Ruby runs.
            if self
                .defined_in
                .get(name)
                .is_none_or(|held| uri <= held.as_str())
            {
                self.superclasses.insert(name.clone(), superclass.clone());
                self.defined_in.insert(name.clone(), uri.to_owned());
            }
        }
        for (name, module) in &contribution.declared {
            self.namespaces.declare(name.clone(), *module);
            if *module {
                insert(&mut self.modules, name);
            }
            insert(&mut self.classes, name);
        }
        for (name, module) in &contribution.foreign {
            if *module {
                insert(&mut self.foreign_modules, name);
            }
            insert(&mut self.foreign_classes, name);
        }
        included.extend(contribution.included.iter().cloned());
    }

    /// An empty projection, with a slot for every registered module's own.
    ///
    /// **The slots are positional and made here, not on first use**, because [`Context::absorb`]
    /// zips a document's contributions against them. A `Context` built without them would silently
    /// absorb nothing for every module: a whole body of knowledge answering nothing, with no error
    /// anywhere.
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
    /// The one downcast a reader makes, and core makes none: it holds `dyn Projects`, and the
    /// *caller* names what it wants back.
    pub fn projection<T: 'static>(&self) -> Option<&T> {
        self.projections.iter().find_map(Projected::of)
    }

    /// And mutably, for the fields a projection can only fill once the whole walk is in.
    pub fn projection_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.projections.iter_mut().find_map(Projected::of_mut)
    }

    /// Sort every list, so a project answers the same way on every run whatever order the graph's
    /// map iterates in. Which file writes a shared relation class depends on this, and so does
    /// which of two schemas is read first.
    pub fn settle(&mut self) {
        for documents in self.documents.values_mut() {
            documents.sort_unstable();
        }
        // And every module's own, for the same reason one layer out: a projection that varied with
        // document order would make two walks over one unchanged workspace produce two `Context`s.
        // The wide gate compares those, and the per-document contributions rely on them.
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
            spells: &[],
            tags: false,
            inherits: false,
            engines: false,
            gems: false,
            reads_only: false,
            buffers: false,
        }
    }

    static ONE: [Wants; 1] = [row(ListId("pretend.one"))];
    static TWO: [Wants; 2] = [row(ListId("pretend.two")), row(ListId("pretend.three"))];
    static SAME: [Wants; 1] = [row(ListId("pretend.one"))];

    /// The seam, stated as a test: a build that registers nothing still builds.
    ///
    /// The only thing core may know about its modules is the line that registers them, and that
    /// line is not in the pass. Removing it must leave something that compiles and answers *nothing
    /// wanted* to every question.
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

    /// A `ListId` is namespaced by its module, so this takes two modules spelling one namespace: a
    /// programming error, not a user's. It fails at construction, once per server, instead of
    /// answering something arbitrary per keystroke.
    #[test]
    #[should_panic(expected = "two bodies of knowledge claim the list pretend.one")]
    fn two_bodies_of_knowledge_cannot_claim_one_list() {
        let _ = Registry::new(vec![
            Box::new(Pretend("first", &ONE)),
            Box::new(Pretend("second", &SAME)),
        ]);
    }

    /// The registry the server really builds: every list belongs to the module that registered it.
    ///
    /// Two halves. **Every row in play came from a module that registered it**, checked by the
    /// namespace on its id, not a list written out here (a copy kept by core would be the coupling
    /// this seam removes). And **every switch is answered by the module owning the list**, which
    /// makes `features.md`'s table a property of the modules, not of the pass.
    #[test]
    fn every_list_in_play_came_from_a_module_that_registered_it() {
        let registry = Registry::new(vec![
            Box::new(rails::Rails::default()),
            Box::new(annotations::Annotations::default()),
            Box::new(structs::Structs::default()),
            Box::new(rspec::RSpec::default()),
            Box::new(factories::Factories::default()),
            Box::new(singletons::Singletons),
            Box::new(defines::Defines::default()),
            Box::new(mixins::Mixins::default()),
            Box::new(i18n::Translate::default()),
        ]);
        assert_eq!(
            registry.modules().map(Knowledge::name).collect::<Vec<_>>(),
            vec![
                "rails",
                "annotations",
                "structs",
                "rspec",
                "factories",
                "singletons",
                "defines",
                "mixins",
                "i18n"
            ]
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
            (rails::ZONES, only(|f| f.schema = true)),
            (rails::MODELS, only(|f| f.models = true)),
            (rails::CONCERNS, only(|f| f.models = true)),
            (rails::ROUTES, only(|f| f.routes = true)),
            (rails::ENGINES, only(|f| f.routes = true)),
            (rails::ENTRYPOINTS, only(|f| f.entrypoints = true)),
            (rails::FRAMEWORK, only(|f| f.rails = true)),
            (rails::CONFIGURED, only(|f| f.rails = true)),
            (annotations::ANNOTATED, only(|f| f.annotations = true)),
            (structs::STRUCTS, only(|f| f.structs = true)),
            (rspec::GROUPS, only(|f| f.rspec = true)),
            (rspec::CONFIGS, only(|f| f.rspec = true)),
            (factories::DEFINITIONS, only(|f| f.factories = true)),
        ] {
            assert!(registry.wanted(list, features), "{list} was not wanted");
            assert!(
                !registry.wanted(list, nothing()),
                "{list} was wanted anyway"
            );
        }
        // Ruby's own library is no switch's, nor is Ruby itself.
        assert!(registry.wanted(singletons::SINGLETONS, nothing()));
        assert!(registry.wanted(defines::DEFINES, nothing()));
        assert!(registry.wanted(mixins::MIXINS, nothing()));

        // A list nobody registered is nobody's, and so is one asked of the wrong module: the arm
        // each `wanted` ends with. The registry never reaches it, because it finds the row first;
        // it is there so that adding a row and forgetting its switch declines instead of answering
        // something arbitrary.
        let elsewhere = ListId("nobody.list");
        assert!(!registry.wanted(elsewhere, all()));
        assert!(!rails::Rails::default().wanted(elsewhere, all()));
        assert!(!annotations::Annotations::default().wanted(elsewhere, all()));
        assert!(!structs::Structs::default().wanted(elsewhere, all()));
        assert!(!rspec::RSpec::default().wanted(elsewhere, all()));
        assert!(!factories::Factories::default().wanted(elsewhere, all()));
        assert!(!singletons::Singletons.wanted(elsewhere, all()));
        assert!(!defines::Defines::default().wanted(elsewhere, all()));
        assert!(!mixins::Mixins::default().wanted(elsewhere, all()));
        assert!(!i18n::Translate::default().wanted(elsewhere, all()));
        // And the one predicate whose question core cannot ask is the module's.
        assert!(rails::Rails::default().claims_by_ancestry(Some("ApplicationJob"), &[]));
        assert!(
            !annotations::Annotations::default().claims_by_ancestry(Some("ApplicationJob"), &[])
        );

        // Each comes back as its own type, which is how a caller that knows which module it wants
        // reads that module's memo. A type nobody registered comes back as nothing.
        assert!(registry.of::<rails::Rails>().is_some());
        assert!(registry.of::<annotations::Annotations>().is_some());
        assert!(registry.of::<structs::Structs>().is_some());
        assert!(registry.of::<rspec::RSpec>().is_some());
        assert!(registry.of::<factories::Factories>().is_some());
        assert!(registry.of::<singletons::Singletons>().is_some());
        assert!(registry.of::<defines::Defines>().is_some());
        assert!(registry.of::<mixins::Mixins>().is_some());
        assert!(registry.of::<i18n::Translate>().is_some());
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
            rspec: true,
            factories: true,
            i18n: true,
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
            rspec: false,
            factories: false,
            i18n: false,
        }
    }
}
