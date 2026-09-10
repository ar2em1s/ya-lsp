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
use std::sync::Arc;
use std::time::Instant;

use rubydex::model::{
    definitions::{Definition, Mixin, Receiver},
    document::Document,
    ids::{NameId, StringId, UriId},
    name::ParentScope,
};

use super::{Analysis, annotations, locator::Site, render, structs, synthesized, views::Views};
use crate::generated::{Facts, Owner};
use crate::workspace::{DocUri, rails};

/// Which projection of the user's own documents a generator reads.
///
/// A generator names one of these rather than writing its own loop, which is what keeps
/// [`Analysis::walk`] one pass over the graph however many generators there are. The
/// projections are not disjoint and are not meant to be: a model file with a `@return` tag is
/// on two of these lists and feeds two generators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum List {
    /// Documents whose name ends `schema.rb`. Whether one really *is* a schema is
    /// [`rails::is_schema`]'s decision, not this list's — the suffix is the cheap half.
    Schemas,
    /// Documents that say something about the **name** of a table rather than about its
    /// columns: `self.table_name=`, which is the escape from every naming convention the schema
    /// generator applies, and the two ways a namespace declares the prefix every table under
    /// it carries.
    ///
    /// One list and not two, because [`rails::read_table_names`] is one walk: a file that says
    /// both would otherwise be parsed twice to be told the same thing.
    Renamed,
    /// Documents that call one of [`rails::MACROS`].
    Models,
    /// Documents holding a `sig` call, or a `@return`/`@param` tag above a `def`.
    Annotated,
    /// Documents defining a class that [`rails::convention_of`] recognises — a mailer, a job or
    /// a Sidekiq worker. The only list filled by what a class *inherits* rather than by
    /// anything the file calls, because these two conventions have no macro to look for.
    Entrypoints,
    /// Documents that reference the constant `Struct` or `Data`.
    ///
    /// The one list filled by a **constant** reference rather than by a call, a path or a
    /// superclass, and it has to be: what puts a document here is `Struct.new`, whose *method*
    /// name is `new` — a filter no file in any corpus would fail. The constant is the rare half
    /// (136 of discourse's 11,875 `.rb` files mention either name) and rubydex records it at
    /// index time exactly as it records a method reference.
    Structs,
    /// Documents named `routes.rb`. Whether one really is an application's routes is
    /// [`rails::is_routes`]' decision, exactly as the schema list defers to [`rails::is_schema`];
    /// the files a routes file *draws* are not on this list at all, because which they are is
    /// something only the file that draws them says.
    Routes,
}

/// The test that puts a document on one list.
///
/// Three predicates, and the whole of what a generator may ask the graph for before anything is
/// resolved. Each is a filter over the definitions and references indexing already recorded, so
/// none of them opens a file: the count of files a generator then reads is the count of files
/// that really do call one of these.
struct Wants {
    list: List,
    /// Receiverless call names that put a document on the list.
    calls: &'static [&'static str],
    /// Constant names that put a document on the list, matched on the last segment.
    ///
    /// The fourth predicate. Matching the last segment rather than the whole
    /// path means somebody's own `Foo::Struct` puts its file on the list too, which costs one
    /// parse and declines — the direction every filter here errs in, because the alternative is
    /// missing `::Struct` and every spelling of a reference nobody has thought of.
    constants: &'static [&'static str],
    /// Singleton method names whose **definition** puts a document on the list.
    ///
    /// The fifth predicate, and the only one that reads a `def` rather than a reference. It has to: `def self.table_name_prefix` is how a module says every class
    /// under it reads a prefixed table, and a module that writes it calls nothing at all — it
    /// would be on no list by any of the four tests above. The receiver is checked because an
    /// instance method of that name is a different method.
    defines: &'static [&'static str],
    /// A file-name suffix that puts a document on the list.
    path: Option<&'static str>,
    /// Whether a `@return`/`@param` tag in a comment above a `def` puts it on the list.
    tags: bool,
    /// Whether a class this document defines being one of [`rails::convention_of`]'s puts it on
    /// the list.
    inherits: bool,
    /// Whether a **Rails engine's** document may go on this list, or only the user's own code.
    ///
    /// One sentence: a list is open to an engine when what it reads
    /// declares members on a class the reader can name, and closed when it declares something
    /// scoped to an application. `has_many` on `ActiveStorage::Blob` is the first; a
    /// `db/schema.rb` is the second — it is *this application's* database, and an engine ships
    /// migrations rather than a dump of one anyway.
    ///
    /// An engine's `config/routes.rb` is the case that looks like the second and is the first,
    /// which is why gate 1 walks a gem's `config/` and this row is open. Four of the seven
    /// engines that ship one draw into `Rails.application.routes`, whose helpers really are the
    /// application's; the other three draw into their own, reached as `blazer.queries_path`
    /// after a `mount`. [`rails::Whose`] is the discriminator, and it is receiver **and** file
    /// location rather than either alone.
    engines: bool,
}

/// Every list, and what fills it. A generator that wants a new list adds a row.
const WANTS: [Wants; 7] = [
    Wants {
        list: List::Schemas,
        calls: &[],
        constants: &[],
        defines: &[],
        path: Some("schema.rb"),
        tags: false,
        inherits: false,
        // an engine ships migrations, never a `schema.rb`, and gate 1 does not walk a gem's `db/`
        engines: false,
    },
    Wants {
        list: List::Renamed,
        calls: &["table_name=", "isolate_namespace"],
        constants: &[],
        defines: &["table_name_prefix", "table_name_suffix"],
        path: None,
        tags: false,
        inherits: false,
        // the input to a generator that is itself closed
        engines: false,
    },
    Wants {
        list: List::Models,
        calls: &rails::MACROS,
        constants: &[],
        defines: &[],
        path: None,
        tags: false,
        inherits: false,
        // `has_many` on `ActiveStorage::Blob` is a member of a class the user names
        engines: true,
    },
    Wants {
        list: List::Annotated,
        calls: &["sig"],
        constants: &[],
        defines: &[],
        path: None,
        tags: true,
        inherits: false,
        // a `@return` an engine's author wrote is about the engine's own method
        engines: true,
    },
    Wants {
        list: List::Entrypoints,
        calls: &[],
        constants: &[],
        defines: &[],
        path: None,
        tags: false,
        inherits: true,
        // `ActiveStorage::AnalyzeJob` really does get `perform_later`
        engines: true,
    },
    Wants {
        list: List::Routes,
        calls: &[],
        constants: &[],
        defines: &[],
        path: Some("routes.rb"),
        tags: false,
        inherits: false,
        // an engine's `config/routes.rb` may name the *host application's* helpers, and
        // `rails::Whose` is what decides whether this one does
        engines: true,
    },
    Wants {
        list: List::Structs,
        calls: &[],
        constants: &["Struct", "Data"],
        defines: &[],
        path: None,
        tags: false,
        inherits: false,
        // `Point = Struct.new(:x)` in a gem's `app/` declares `Point#x`, which is the engine
        // rule read straight: the members are on a class the reader can name, and nothing about
        // the call is scoped to an application
        engines: true,
    },
];

/// The `StringId`s one document is filtered against, hashed once.
///
/// A struct rather than two locals inside the loop because the loop body is callable for
/// **one** document: the gate re-asks [`Analysis::contribution`] of the document a keystroke
/// touched, and building the filter per call would hash all of [`rails::MACROS`] to answer about
/// a single file.
struct Filters {
    /// Per [`WANTS`] row, the hashes of its `calls`.
    calls: Vec<Vec<StringId>>,
    /// Per [`WANTS`] row, the hashes of its `constants`.
    constants: Vec<Vec<StringId>>,
}

impl Filters {
    fn new() -> Self {
        Self {
            calls: WANTS
                .iter()
                .map(|want| {
                    want.calls
                        .iter()
                        .map(|name| StringId::from(*name))
                        .collect()
                })
                .collect(),
            constants: WANTS
                .iter()
                .map(|want| {
                    want.constants
                        .iter()
                        .map(|name| StringId::from(*name))
                        .collect()
                })
                .collect(),
        }
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
/// **`Hash` is the point of the type rather than a convenience.** What is stored per document is
/// [`fingerprint`]'s eight bytes, so the gate compares those instead of the strings; and it is a
/// hash of *what the document contributes* and not of the document, so a comment, a local
/// variable, a whole method body — anything no field here reads — moves nothing.
///
/// Every field is a `Vec` in the order the document's definitions are recorded, which is
/// deterministic for a given file. The **merge** is what must not depend on the order documents
/// are visited in, and [`Context::absorb`] and [`Context::settle`] are where that is paid for.
#[derive(Debug, Default, PartialEq, Eq, Hash)]
struct Contribution {
    /// Which [`WANTS`] lists this document joins.
    lists: Vec<List>,
    /// `(table, the top-level class whose own name implies it)`.
    claims: Vec<(String, String)>,
    /// The nested classes it defines.
    nested: Vec<String>,
    /// The route-helper hosts it defines.
    hosts: Vec<Owner>,
    /// The helper modules it defines, when the document is one of Rails' helper files.
    helpers: Vec<String>,
    /// `(class, the superclass it names)` — the input to both `superclasses` and `defined_in`,
    /// which are filled by one `if let` and must stay that way.
    superclasses: Vec<(String, String)>,
    /// `(name, whether the line said `module`)`, feeding `classes`, `modules` and `namespaces`.
    declared: Vec<(String, bool)>,
    /// `(the body that wrote the `include`, the constant it spelled)`.
    included: Vec<(String, String)>,
}

/// The eight bytes stored per document, so the walk can be skipped.
///
/// `DefaultHasher` rather than anything stronger because the comparison is always *this
/// document against its own previous value* within one process: a collision would have to be
/// between two contributions of one file, at 2^-64 per keystroke.
fn fingerprint(contribution: &Contribution) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contribution.hash(&mut hasher);
    hasher.finish()
}

/// Everything the generators need from the graph, gathered in one pass.
///
/// Nothing here is a decision: which schema files are really schemas, which class claims which
/// table and which association may be believed are all settled by the generator that asks, so
/// that this stays one loop over the documents rather than one per generator.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Context {
    /// The documents on each list, sorted. Absent means empty.
    pub(super) documents: BTreeMap<List, Vec<String>>,
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
    pub(super) classes: BTreeSet<String>,
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
    pub(super) modules: BTreeSet<String>,
    /// A class the application defines, and the superclass it names, spelled as written.
    ///
    /// A mailer and a job have no macro at all and are recognised by what
    /// they inherit. Spelled as written rather than qualified, because the test is a suffix —
    /// `ApplicationMailer` — and a lexical nesting the reference does not have would only make
    /// the string longer.
    pub(super) superclasses: BTreeMap<String, String>,
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
    /// [`Analysis::model_declarations`] has what moving them cost.
    pub(super) defined_in: BTreeMap<String, String>,
    /// Every ActiveRecord model the application defines, fully spelled.
    ///
    /// [`models_of`] is the walk and its docstring is the argument. Computed once after the
    /// loop below rather than asked per name, because the question is a *chain* — `Spree::Order`
    /// is two hops from `ActiveRecord::Base` and 15 of solidus' models are four or five — and a
    /// chain can only be walked when every link is in `superclasses`.
    pub(super) models: BTreeSet<String>,
    /// Table name -> the top-level classes whose own name implies it.
    pub(super) claims: BTreeMap<String, Vec<String>>,
    /// Every **nested** class the application's own code defines, fully spelled.
    ///
    /// The table-name reader's input, and it is a second list rather than a widening of `claims` because the
    /// table a nested class reads cannot be computed here at all: `compute_table_name` puts
    /// `full_table_name_prefix` in front of it, and that is a `def self.table_name_prefix` in
    /// somebody else's file — a *document*, which no projection of the definitions can read.
    /// So the inflection moves to [`Analysis::model_tables`], which is where the prefixes are,
    /// and this list is only the names that are eligible to have one.
    ///
    /// `own` for `claims`' own reason: a table is the *application's* database table. It is not
    /// filtered to models here either, because whether a class is one is a walk up
    /// `superclasses`, and that map is only complete when this loop is.
    pub(super) nested: BTreeSet<String>,
    /// The classes a **gem** declares that the long-tail macro table names, and that this
    /// bundle has.
    ///
    /// [`rails::framework_classes`] is the list — three of them — and this is the subset the
    /// graph actually holds. It cannot come from the loop below, which visits the application's
    /// own documents and an engine's `app/`: `ActiveStorage::Attached::One` is in
    /// activestorage's `lib/`, one directory from `Blob` and on the far side of the engine
    /// gate. So it is three lookups rather than a projection, asked once per pass, and a
    /// workspace whose bundle has no Active Storage declares no `has_one_attached` rather than
    /// one typed as a class nothing can reach.
    pub(super) framework: BTreeSet<String>,
    /// The `db/*structure.sql` files under the workspace root, if any.
    ///
    /// The one input in this struct that is **not** a projection of the graph, and it has to be:
    /// a `.sql` file is not a document, so no [`WANTS`] row can reach one however it is spelled.
    /// It is a path under the root rather than a URI out of the graph — one `read_dir` of
    /// `<root>/db` per settle, which is the same rule the file watcher registers
    /// (`db/*structure.sql`) so that what is watched and what is read cannot disagree.
    ///
    /// Held here rather than found by the generator so that [`Context::is_empty`] can count it:
    /// an application with a `structure.sql`, models that write no macro and no routes file has
    /// nothing on any of the six lists, and the pass would otherwise return before reading it.
    pub(super) dumps: Vec<DocUri>,
    /// What may be spelled around a generated owner, and by whose authority.
    ///
    /// **`classes` and `modules` above answer "may a macro name this"; this answers "may a
    /// generated name be spelled around this", and they are not one question.** Everything in `classes` is in here too, and then
    /// [`Analysis::bundle_namespaces`] adds what the **bundle** declares about the namespaces
    /// *above* those names — which is the only place a generated name can introduce a segment
    /// nobody wrote.
    ///
    /// The two are deliberately not merged, and the corpus is the argument rather than caution:
    /// at 3,979 association sites over six applications, 541 name a class no file in the
    /// application declares and the whole graph supplies **21 of them, every one a Ruby core
    /// class matched by accident** — `has_many :objects` camelizes to `Object`, nineteen times
    /// in mastodon alone. A macro names this application's models; a namespace belongs to
    /// whoever wrote it.
    pub(super) namespaces: crate::generated::Namespaces,
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
    pub(super) includers: BTreeMap<String, BTreeSet<String>>,
    /// Every module under `app/helpers/**/*_helper.rb` the application's own code defines.
    ///
    /// The one projection here that is a question about a **path** rather than
    /// about a definition: [`rails::is_helper`] is Rails' own glob, and what makes a module a
    /// helper is which file it is written in and nothing it says. `own` and not
    /// `is_generator_source`, because an engine's helpers are included into the engine's
    /// controllers, not into this application's views.
    pub(super) helpers: BTreeSet<String>,
    /// Every body the route helpers are `include`d into, as the owner of that `include`.
    ///
    /// Collected here rather than by the generator because the question is about the *graph* —
    /// which classes this application defines and what each inherits — and a generator in
    /// `workspace::rails` never sees one. [`rails::hosts_routes`] is the rule, asked once per
    /// definition on the same walk that already reads the superclass for the entry points.
    pub(super) hosts: BTreeSet<Owner>,
}

impl Context {
    /// The documents on one list, or nothing.
    pub(super) fn documents(&self, list: List) -> &[String] {
        self.documents.get(&list).map_or(&[], Vec::as_slice)
    }

    /// Whether `name` is an ActiveRecord model.
    ///
    /// The gate on a new claimant, and it is load-bearing rather than tidy: `claims` is every
    /// top-level class the application defines and not only its models, which costs nothing
    /// while a table has one claimant. Once a nested class may claim one it costs a great deal
    /// — the six corpora hold **75** nested non-model classes whose name inflects onto a real
    /// table under a different last segment, and every one of them would take that table away
    /// from the model that reads it. A gate on the superclass chain declines all 75 and keeps
    /// every legitimate claimant.
    ///
    /// A lookup rather than a walk, because it is asked of every class the application defines
    /// rather than of the nested few: [`models_of`] does the walk once.
    /// A name this application does not define at all is in neither, which is the same answer
    /// either way.
    fn is_model(&self, name: &str) -> bool {
        self.models.contains(name)
    }

    /// Whether any generator has anything to read.
    ///
    /// [`List::Renamed`] is deliberately not counted: `self.table_name=` on its own declares
    /// nothing at all — it renames a table for a schema that has to exist somewhere else.
    fn is_empty(&self) -> bool {
        self.documents(List::Schemas).is_empty()
            && self.dumps.is_empty()
            && self.documents(List::Models).is_empty()
            && self.documents(List::Annotated).is_empty()
            && self.documents(List::Entrypoints).is_empty()
            && self.documents(List::Routes).is_empty()
            && self.documents(List::Structs).is_empty()
    }

    /// Every file some generator opens, for the pass gate.
    ///
    /// Every list, including [`List::Renamed`] which [`Context::is_empty`] leaves out: that list
    /// declares nothing on its own and is still *read*, and this question is about reading. The
    /// dumps are here too — a `db/*structure.sql` is not a graph document, and it can still be
    /// open in an editor, which is the one way it reaches this pass without the watcher.
    fn read_by_a_generator(&self) -> impl Iterator<Item = &str> {
        self.documents
            .values()
            .flatten()
            .map(String::as_str)
            .chain(self.dumps.iter().map(DocUri::as_str))
    }

    /// Merge what one document contributes — the **only** place a [`Contribution`] becomes part
    /// of a `Context`.
    ///
    /// **Every line here must be order-independent.** A fingerprint per document is only sound if the merged answer
    /// is a function of the *set* of contributions and not of the order the graph's map happens
    /// to hand them over in — so the two fields that were not are fixed here and in
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
    fn absorb(
        &mut self,
        uri: &str,
        contribution: Contribution,
        included: &mut Vec<(String, String)>,
    ) {
        for list in contribution.lists {
            self.documents.entry(list).or_default().push(uri.to_owned());
        }
        for (table, name) in contribution.claims {
            self.claims.entry(table).or_default().push(name);
        }
        self.nested.extend(contribution.nested);
        self.hosts.extend(contribution.hosts);
        self.helpers.extend(contribution.helpers);
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
        included.extend(contribution.included);
    }

    /// Sort every list, so that a project answers the same way on every run whatever order the
    /// graph's map happens to iterate in. Which file writes a shared relation class depends on
    /// this, and so does which of two schemas is read first.
    fn settle(&mut self) {
        for documents in self.documents.values_mut() {
            documents.sort_unstable();
        }
        // The other half of [`Context::absorb`]'s argument. `claims` is pushed once per
        // *definition* in the graph's iteration order, which is a `HashMap`'s and therefore
        // nobody's. It changes no answer — `model_tables` sorts and dedups its own copy before
        // it reads one — and what it does change is that two walks over one workspace produce
        // the same `Context`, which is what the outer gate compares and the per-document
        // fingerprints stand on.
        for classes in self.claims.values_mut() {
            classes.sort_unstable();
        }
        // A `read_dir` has no defined order either, and for the same reason: which of two
        // schema sources is read first decides nothing here only because the ambiguity rule
        // runs first, and a list that varies per run is a difference waiting to matter.
        self.dumps.sort_unstable();
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
/// The mailers come out of `superclasses` rather than out of a list of their own, because the
/// question is exactly [`rails::is_mailer`]'s and asking it of a map this pass already holds is
/// cheaper than a seventh row in [`WANTS`] that would read the same definitions again.
fn view_context(context: &Context, models: &[(DocUri, String, Arc<rails::Model>)]) -> Views {
    let mut exports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut included: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_, _, model) in models {
        for (owner, names) in model.exports() {
            exports
                .entry(owner.to_owned())
                .or_default()
                .extend(names.iter().cloned());
        }
        for (owner, modules) in model.helper_modules() {
            included
                .entry(owner.to_owned())
                .or_default()
                .extend(modules.iter().cloned());
        }
    }
    let mailers: BTreeSet<String> = context
        .superclasses
        .iter()
        .filter(|(_, superclass)| rails::is_mailer(superclass))
        .map(|(name, _)| name.clone())
        .collect();
    Views::new(
        context.helpers.iter().cloned().collect(),
        exports,
        included,
        mailers,
    )
}

/// Every ActiveRecord model, by climbing what each class says it inherits.
///
/// The chain and not one hop: solidus writes 101 models two hops from `ActiveRecord::Base` and
/// 15 four or five, so a one-hop test would call `Spree::Order` no model at all. Each hop
/// resolves the spelling the way Rails does — [`rails::candidates`] is `compute_type`'s own
/// list, innermost nesting first and the bare name last — because `class Address < Spree::Base`
/// inside `module Spree` says `Spree::Base` and means it, while `class LineItem < Base` says
/// `Base` and means the same class.
///
/// `seen` is not caution about Ruby, which cannot have a superclass cycle — it is about
/// *source*, which can be written with one, and this walks the text rather than the run.
///
/// **The chain stops at a name the application does not define**, which is not a defect and is
/// worth stating because the relation set turns on it: forem's `Tag < ActsAsTaggableOn::Tag`
/// and `EmailMessage < Ahoy::Message` are real models whose base class is in a gem, and this
/// answers `false` for both. It is why that set is a **union** with what the macros ask for
/// rather than a replacement of it — 11 classes over six corpora are a collection element and
/// not a model by this walk, and deleting their relation classes is the one way this could make
/// an answer worse rather than absent.
fn models_of(superclasses: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut models: BTreeSet<String> = BTreeSet::new();
    for name in superclasses.keys() {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut current = name.as_str();
        while seen.insert(current) {
            let Some(written) = superclasses.get(current) else {
                break;
            };
            if rails::is_record_base(written) {
                models.insert(name.clone());
                break;
            }
            // The candidate list is owned and `current` outlives it, so the name it walks on to
            // has to be the map's own copy rather than the list's.
            let Some((next, _)) = rails::candidates(current, written)
                .into_iter()
                .find_map(|candidate| superclasses.get_key_value(&candidate))
            else {
                break;
            };
            current = next;
        }
    }
    models
}

/// The class a model's class side goes on: the topmost model of its own superclass chain.
///
/// Where the class side goes, and it is [`models_of`]'s walk with a
/// different stopping rule. That one climbs until it reaches `ActiveRecord::Base` and answers
/// *whether*; this one climbs while the next class up is a model the application itself
/// declares and answers *which* — so `Spree::Order` under `Spree::Base` under
/// `ApplicationRecord` answers `ApplicationRecord`, and `Story` directly under it answers the
/// same. One copy of the query interface there is inherited by every model beneath it, which is
/// what Rails does and is why 119 declarations per model become 119 per application.
///
/// **The base has to be a class the application declares.** forem's `Tag < ActsAsTaggableOn::Tag`
/// stops at `Tag` itself, because the chain leaves the application and nothing may be declared
/// on a gem's class — so such a model is its own base and pays for its own copy. The walk is
/// bounded the same way [`models_of`]'s is, and for the same reason: `seen` is about *source*,
/// which can be written with a cycle.
///
/// Each hop resolves the spelling the way Rails does — [`rails::candidates`] is
/// `compute_type`'s own list — because `class Address < Spree::Base` inside `module Spree` says
/// one thing and means another.
fn base_of(
    name: &str,
    superclasses: &BTreeMap<String, String>,
    models: &BTreeSet<String>,
    namespaces: &crate::generated::Namespaces,
) -> String {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut current = name;
    let mut top = None;
    while seen.insert(current) {
        let Some(written) = superclasses.get(current) else {
            break;
        };
        top = Some(written);
        let Some((next, _)) = rails::candidates(current, written)
            .into_iter()
            .find_map(|candidate| superclasses.get_key_value(&candidate))
        else {
            break;
        };
        if !models.contains(next) {
            break;
        }
        current = next;
    }
    // One hop past the application, and only ever onto `ActiveRecord::Base` itself. Discourse
    // has no `ApplicationRecord` at all, so every one of its models is its own base and the
    // walk above saves it nothing; the class Rails really installs the interface on is in a
    // gem. Reopening somebody else's class is safe for the `MessageDelivery` stub's reason —
    // nothing declared here is mapped, so the gem keeps every place it has — and the gate is the
    // namespace rule: the **bundle** has to declare the name, or a generated body would
    // introduce a constant nothing wrote.
    //
    // `ActiveRecord::Base` **exactly**, and not [`rails::is_record_base`]'s other spelling. An
    // `ApplicationRecord` the application declares is reached by the walk above, because a
    // class that inherits `ActiveRecord::Base` is a model; one it does *not* declare is a name
    // this pass may not write on at all. The two are not one question and reading them as one
    // put the interface on a bare `class ApplicationRecord` that inherits nothing.
    if let Some(written) = top.filter(|written| *written == rails::RECORD_BASE)
        && let Some(resolved) = rails::candidates(current, written)
            .into_iter()
            .find(|candidate| namespaces.declares(candidate) && namespaces.spellable(candidate))
    {
        return resolved;
    }
    current.to_owned()
}

/// Which classes each concern's macros really land on, resolved and then closed over.
///
/// Two steps, and both are somebody else's rule copied rather than invented. An `include` names
/// a constant, and Ruby resolves it against the nesting of the body that wrote it — which is
/// [`rails::candidates`], the same list `compute_type` gives an association's `class_name`. A module the application does not define resolves to nothing and
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
        let Some(target) = rails::candidates(owner, written)
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
/// [`rails::MESSAGE_DELIVERY`], and no other segment can appear above one.
///
/// Bounding it is the whole reason this is affordable. Measured over lobsters, spelling **every**
/// class and module in the graph costs 20.7 ms a settle against a resolve of 19–23; filtering on
/// the last segment first, and spelling only what matches, costs **2.5 ms** — because the filter
/// is a `StringId` compare and the walk never touches the name of the 26,514 namespaces nobody
/// asked about. Discourse is 27.2 ms against 3.6.
fn wanted_namespaces(context: &Context) -> BTreeSet<String> {
    fn prefixes(name: &str, into: &mut BTreeSet<String>) {
        for (at, _) in name.match_indices("::") {
            into.insert(name[..at].to_owned());
        }
    }
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for name in &context.classes {
        prefixes(name, &mut wanted);
    }
    for name in rails::framework_classes() {
        prefixes(name, &mut wanted);
        wanted.insert(name.to_owned());
    }
    prefixes(rails::MESSAGE_DELIVERY, &mut wanted);
    wanted.insert(rails::MESSAGE_DELIVERY.to_owned());
    wanted.insert(rails::ROUTE_HELPERS.to_owned());
    wanted.insert(rails::RELATION_BASE.to_owned());
    // The class side's own base. An application with no `ApplicationRecord`
    // of its own — discourse writes `class Post < ActiveRecord::Base` 217 times — has no shared
    // base in its own code, and the one Rails uses is in a gem. Asking whether the bundle
    // declares it is what decides whether this pass may write there at all.
    prefixes(rails::RECORD_BASE, &mut wanted);
    wanted.insert(rails::RECORD_BASE.to_owned());
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

/// Add one generator's facts to the document its source file names.
///
/// A file that feeds two generators — a model with a `has_many` and a `@return` tag — gets one
/// generated document holding both, because the side table is keyed by generated URI and a
/// second `record` for one source would replace the first rather than add to it. Merging here
/// is also where precedence is enforced: a member two generators both name is decided by
/// [`Facts`]' rank rather than written twice as a silent overload. A
/// generator that said nothing adds no entry, so a source none of them had anything to say
/// about is not recorded and is not kept.
fn merge(into: &mut BTreeMap<String, (DocUri, Facts)>, uri: &DocUri, facts: Facts) {
    if facts.is_empty() {
        return;
    }
    into.entry(uri.as_str().to_owned())
        .or_insert_with(|| (uri.clone(), Facts::default()))
        .1
        .extend(facts);
}

/// Which columns a model file re-types, keyed by the class the macro is written on.
///
/// The whole of what one generator in this pass tells another, and it is an *input* rather than
/// a fact: `Facts::returns` answers within a document, and these two declarations are never in
/// one. `story.status` is the label `enum :status` names — a `String` — and the column it is
/// stored in is an `Integer`, so the schema has to be told to say nothing about it.
///
/// The second macro is on the same wire, and Rails' own documentation is why: an
/// `attribute` with a cast type "will override the type of existing attributes if needed". Only
/// the calls that name a type this crate has a class for are here, so `attribute :payload, :json`
/// leaves a `t.string "payload"` answering exactly as it did.
/// Every `(class, member)` some document already holds, as `Elsewhere::columns` wants it.
///
/// Called once, between the schemas and the models, so what it holds is exactly the schemas'.
fn declared_members(generated: &BTreeMap<String, (DocUri, Facts)>) -> BTreeSet<(String, String)> {
    generated
        .values()
        .flat_map(|(_, facts)| facts.declared())
        .filter_map(|(owner, name)| match owner {
            Owner::Instance(class) => Some((class.clone(), name.to_owned())),
            _ => None,
        })
        .collect()
}

fn retyped_columns(
    models: &[(DocUri, String, Arc<rails::Model>)],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut columns: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_, _, model) in models {
        for (class, attribute) in model.retyped_columns() {
            columns
                .entry(class.to_owned())
                .or_default()
                .insert(attribute.to_owned());
        }
    }
    columns
}

/// How the text of one source file was identified when a reader last parsed it.
///
/// Two variants because the pass has two authorities for what a file says and they cannot be
/// compared the same way. A file on disk is identified by [`stamp_of`], which is the pass
/// gate's own evidence and not a second mechanism: that gate earns "the answer is a function of
/// what is on disk" by looking at the disk, and a memo that trusted anything weaker would take
/// it away.
/// A file the editor holds has no useful stamp at all — the disk is behind the buffer — so it
/// is identified by its text, hashed. Not by the version: a client may send `didChange` with no
/// version, and two versions of one buffer would then look like no change at all.
#[derive(Debug, PartialEq, Eq)]
enum Fresh {
    Disk(Option<(std::time::SystemTime, u64)>),
    Buffer(u64),
}

/// Which of the readers one document's place on the lists calls for.
///
/// A document is usually on one list and may be on five, and the reads are per *document*
/// rather than per list — which is a saving in its own right: read per list, a model file that
/// also carries a `@return` tag and a `self.table_name=` is read three times.
///
/// Compared as a whole, so a document that has joined a list since the last pass is read again
/// rather than topped up. That is not only simpler: **a document can join a list without its own
/// text changing.** [`Analysis::walk`] puts every file that *defines* a class some other file's
/// macro names onto [`List::Models`], so writing `has_many :widgets` in one file adds
/// `widget.rb` to a list it was not on — and if `widget.rb` was already read for a `@return`
/// tag, its entry is fresh and holds the wrong things.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    model: bool,
    /// Whether the schema reader is wanted, and whether the file is a dump rather than a
    /// `schema.rb` — which decides which of the two readers runs, and is a property of the path.
    schema: Option<bool>,
    names: bool,
    entrypoints: bool,
    annotated: bool,
}

/// One source file as the readers last saw it, and the evidence it has not changed.
///
/// **The memo is deliberately of the *parse* rather than of the [`Facts`].** Memoising the
/// facts is the obvious seam and the worse one: they are a function of the text *and* the
/// `Context`, so the whole memo has to be dropped whenever the `Context` moves. `read_model`
/// and the four beside it take **nothing but the text**, so there is no second input to
/// compare, no invalidation rule to get wrong, and a keystroke that adds a class somewhere else
/// in the project does not throw the memo away. What the `Context` is an argument to is
/// `signatures`, measured at **12 ms of a 1,220 ms pass**.
#[derive(Debug)]
pub(super) struct Cached {
    fresh: Fresh,
    /// The readers this entry was built for — see [`Wanted`] for why a document can start
    /// wanting more of them without its own text changing.
    wanted: Wanted,
    /// The path every reader writes into its provenance, which is a function of the URI.
    name: String,
    model: Option<Arc<rails::Model>>,
    schema: Option<Arc<rails::Schema>>,
    names: Option<Arc<rails::TableNames>>,
    entrypoints: Option<Arc<rails::Entrypoints>>,
    /// The annotations reader ends at [`Facts`] directly rather than at a syntax type, so this
    /// is what it produced. Handed to [`merge`] by value, and cloning a hundred-odd files'
    /// worth of facts is the parse this saves several hundred times over.
    annotated: Option<Facts>,
}

impl Cached {
    /// Run every reader `wanted` asks for, once, over text just read.
    fn read(fresh: Fresh, wanted: Wanted, name: String, source: &str) -> Self {
        let annotated = wanted.annotated.then(|| annotations::read(source, &name));
        Self {
            fresh,
            wanted,
            name,
            model: wanted.model.then(|| Arc::new(rails::read_model(source))),
            schema: wanted.schema.map(|dumped| {
                Arc::new(if dumped {
                    rails::read_structure(source)
                } else {
                    rails::read_schema(source)
                })
            }),
            names: wanted
                .names
                .then(|| Arc::new(rails::read_table_names(source))),
            entrypoints: wanted
                .entrypoints
                .then(|| Arc::new(rails::read_entrypoints(source))),
            annotated,
        }
    }
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
    /// **Six generators, one pass, one generated document per source file.** The schema —
    /// `db/*schema.rb` and `db/*structure.sql`, which are one generator because they end at one
    /// `rails::Schema` — the model macros, the annotations somebody wrote by hand, the mailer
    /// and job conventions, the routing DSL and `delegate` all end at a [`Facts`], and a file
    /// that feeds two of them has both merged into the one document its URI names. The last is
    /// a second *phase* and runs last, because what it derives from is what the other five
    /// said.
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
        // generators have to run and the walk does not. On discourse the walk is 105 ms of
        // every such keystroke.
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
        let (context, contributions) = if same {
            // The projection is the one already held, and the fingerprints beside it are still
            // its own. **Taken and not cloned**: every path out of this function from here on
            // ends at `remember`, which puts both back.
            (
                self.generated_from
                    .take()
                    .expect("`context_would_be_the_same` answered about a projection it holds"),
                std::mem::take(&mut self.contributions),
            )
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
            // The walk just ran, so the fingerprints are fresh and the `Context` they belong to
            // is the one already held — `previous == context` is what got us here. Keeping them
            // is what lets the *next* keystroke take the gate above: a document whose
            // contribution moved without moving the merged answer would otherwise re-fail
            // the cheap comparison for the rest of the session.
            self.contributions = contributions;
            self.touched.clear();
            return;
        }
        self.passes += 1;
        // Every file any generator is about to open, read and parsed here and only here — and
        // only where the text has moved since the last pass.
        self.refresh_sources(&context);
        if context.is_empty() && self.generated.is_empty() {
            // The view context is rebuilt here too, and with no model sources rather than not
            // at all: its helpers half is a projection of the walk above and costs nothing, and a
            // workspace that has just had its last macro deleted has to *lose* the exports it
            // had rather than keep them. There are none to find — a `helper_method` is on
            // `MACROS`, so a file that writes one is on `List::Models` and this branch is not
            // reached — and passing the empty list says that once instead of asserting it.
            self.views = view_context(&context, &[]);
            self.remember(context, contributions);
            return;
        }

        let mut generated: BTreeMap<String, (DocUri, Facts)> = BTreeMap::new();
        // The model files are read before the schema and not after it, which is the one ordering
        // in this pass that is a data dependency rather than a habit: an `enum` and a typed
        // `attribute` each re-type the column they are stored in, the two declarations land in
        // two different generated documents, and `Facts`' precedence is per document — so the
        // rank is spent by the schema declining to declare the column at all.
        let sources = self.model_sources(&context);
        self.views = view_context(&context, &sources);
        let retyped = retyped_columns(&sources);
        let schemas = self.schema_declarations(&context, &retyped, &mut generated);
        // What the schemas just said, so an untyped `attribute` of the same name can
        // decline to it. The rank says the column wins — `Source::Column` is 4 and
        // `Source::Attribute` 7 — and `Facts::declare` settles a collision *inside* one
        // document, which these two are not; so the loser declines, exactly as an `enum` and a
        // `delegate` do.
        let columns = declared_members(&generated);
        let models = self.model_declarations(&context, &sources, &columns, &mut generated);
        let annotations = self.annotation_declarations(&context, &mut generated);
        let entrypoints = self.entrypoint_declarations(&context, &mut generated);
        let routes = self.route_declarations(&context, &mut generated);
        let structs = self.struct_declarations(&context, &mut generated);
        // Phase two, and it is last because it reads the other five. Nothing after it may
        // declare, or a `delegate` would be deriving from a fact that had not been said when it
        // asked, which is the ordering assumption `Facts::returns` exists to remove.
        let delegated = self.delegate_declarations(&sources, &mut generated);

        let mut kept: HashSet<String> = HashSet::new();
        for (uri, facts) in generated.values() {
            // Rendered here and only here: every span in the document is computed against the
            // text as it is finally written, so no generator's offsets have to be shifted by
            // the length of another's.
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
            if let Ok(needle) = std::env::var("YA_LSP_DUMP")
                && uri.as_str().contains(&needle)
            {
                eprintln!("=== {} ===\n{}", uri.as_str(), declarations.rbs);
            }
            self.synthesized.record(
                &mut self.graph,
                &mut self.types,
                uri,
                &declarations.rbs,
                mappings,
            );
            kept.insert(uri.as_str().to_owned());
        }
        self.forget_stale(&kept);

        // Debug rather than info: this runs before every resolve, so it is one line per settle
        // and it would drown the log of an ordinary editing session. It is the only place these
        // numbers exist, and they are what a report about this feature needs.
        tracing::debug!(
            "{} files declare {schemas} columns, {models} members, {annotations} annotated \
             methods, {entrypoints} entry points, {routes} route helpers, {structs} struct \
             members and {delegated} delegated names, in {:.2?} ({walk:.2?} of it the walk)",
            kept.len(),
            started.elapsed()
        );
        self.remember(context, contributions);
    }

    /// Whether the generators would write exactly what they wrote last time.
    ///
    /// `settle` calls this pass before every `resolve` and a forced settle sits in front of
    /// every graph-reading request, so without this gate one keystroke in a file with no macro
    /// in it pays for a whole-workspace regeneration: 262 ms on discourse, in front of
    /// completion's own 10.
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
    /// The **walk is not skipped here**, and it was 36% of the pass — 101 ms of 276–285,
    /// measured on an idle machine — because building the evidence for *this* gate is the walk.
    /// [`Analysis::context_would_be_the_same`] runs before this one and needs no walk at all.
    /// This gate stays because it is strictly wider — it catches a document whose contribution
    /// moved without moving the merged answer, which eight bytes per document cannot — and it
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
    /// that is 101 ms of a 276–285 ms pass. This asks the same question of **one document**.
    ///
    /// **It must not borrow the other gate's file clause.** "Is a touched file on a generator's
    /// list" is about what a generator *reads* and says nothing at all about what the walk
    /// *builds*; asked on its own, this answers yes for the commonest edit in a Rails
    /// application and the projection is reused while the generators run — 105 ms of every
    /// keystroke in a discourse model file, and 7 of one in a lobsters model file.
    ///
    /// **Three things make eight bytes per document enough**, and each of them is a property
    /// something else in this module had to be given:
    ///
    /// 1. The merged `Context` is a function of the *set* of contributions and not of the order
    ///    the graph hands them over in. [`Context::absorb`] and [`Context::settle`] are where
    ///    that is paid for, and two fields had to change to make it true.
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
        let filters = Filters::new();
        for uri in &self.touched {
            // One `let … else` and not two, because "the graph has no such document" and "the
            // walk does not visit it" are the same answer here: no fingerprint was stored, so
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
                .map(|contribution| fingerprint(&contribution))
            else {
                return false;
            };
            if self.contributions.get(&id) != Some(&now) {
                return false;
            }
        }
        // Not a projection of the graph and therefore not covered by anything above — a `.sql`
        // is not a document, which is why `Context::dumps` exists at all.
        let mut dumps = self.schema_dumps();
        dumps.sort_unstable();
        dumps == previous.dumps
    }

    fn remember(&mut self, context: Context, contributions: HashMap<UriId, u64>) {
        self.contributions = contributions;
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
    /// The walk's answer on its own, for the tests that assert on the projection rather than on
    /// what a generator did with it.
    #[cfg(test)]
    pub(super) fn context(&mut self) -> Context {
        self.walk().0
    }

    /// The same walk, and the eight bytes per document stored beside it.
    ///
    /// Two returns rather than one because the fingerprints are a *by-product* of the loop and
    /// must not be a second one: computing them anywhere else would be the walk again, which is
    /// the cost the gate exists to remove.
    fn walk(&mut self) -> (Context, HashMap<UriId, u64>) {
        self.walks += 1;
        let filters = Filters::new();
        let mut context = Context::default();
        // `(the body that wrote the `include`, the constant it spelled)`, resolved after the
        // loop — see [`Context::includers`].
        let mut included: Vec<(String, String)> = Vec::new();
        let mut fingerprints: HashMap<UriId, u64> = HashMap::new();
        for (uri_id, document) in self.graph.documents() {
            let Some(contribution) = self.contribution(document, &filters) else {
                continue;
            };
            // Every document the walk **visits** gets an entry, including one that contributes
            // nothing: absent has to mean *not visited*, because that is the case the gate
            // cannot reason about. See [`Analysis::context_would_be_the_same`].
            fingerprints.insert(*uri_id, fingerprint(&contribution));
            context.absorb(document.uri(), contribution, &mut included);
        }
        context.models = models_of(&context.superclasses);
        context.includers = includers_of(&included, &context.classes, &context.modules);
        // A model that writes no macro at all is on no list, and it is exactly the
        // model whose query interface had to be answered by whatever its abstract parent
        // happened to own. Its own file is where its relation class belongs, so the file joins
        // the list that opens it.
        //
        // The **only** membership decided after the walk rather than during it, and it has to
        // be: every predicate in [`WANTS`] is a question about one document, and whether a
        // class is a model is a question about the chain above it — which is only complete when
        // every document has been seen. `settle` sorts the list afterwards, so appending here
        // cannot change which document writes what.
        let joining: BTreeSet<String> = {
            let listed: HashSet<&str> = context
                .documents(List::Models)
                .iter()
                .map(String::as_str)
                .collect();
            context
                .models
                .iter()
                .filter_map(|name| context.defined_in.get(name))
                .filter(|uri| !listed.contains(uri.as_str()))
                .cloned()
                .collect()
        };
        context
            .documents
            .entry(List::Models)
            .or_default()
            .extend(joining);
        // One walk rather than three lookups, because `Graph::get` reads the
        // **declarations**, which `Resolver::resolve` builds and this
        // pass runs before — so it answers a settle late, and over lobsters it holds 2,431
        // entries against 174,919 definitions at the moment it is asked. The definitions are
        // there; only the index over them is not.
        for (name, module) in self.bundle_namespaces(&wanted_namespaces(&context)) {
            context.namespaces.declare(name, module);
        }
        context.framework = rails::framework_classes()
            .into_iter()
            .filter(|name| context.namespaces.declares(name))
            .map(str::to_owned)
            .collect();
        context.dumps = self.schema_dumps();
        context.settle();
        (context, fingerprints)
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
    /// document that is not visited for exactly that reason, which is what lets the fingerprint
    /// stop at this function's own outputs.
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
        if (!own && !self.is_generator_source(document.uri())) || document.uri().ends_with(".rbs") {
            return None;
        }
        let mut contribution = Contribution::default();
        // The suffix test is the whole reason this is not a path parse per document: `_helper.rb` leaves a handful of files in the largest corpus, and
        // [`rails::is_helper`] — which is the rule, and reads the `app/helpers` anchor as
        // well — is asked only of those.
        let helper = own
            && document.uri().ends_with("_helper.rb")
            && DocUri::from_uri_str(document.uri())
                .and_then(|uri| uri.to_path())
                .is_some_and(|path| rails::is_helper(&path));
        let mut tagged = false;
        let mut inherits = false;
        let mut defines = [false; WANTS.len()];
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
                    // A table is claimed by pluralizing a **top-level** class's name.
                    // `Admin::Setting`'s table depends on `table_name_prefix`, which is
                    // Ruby that only runs, so it is declined here and left to
                    // `self.table_name`, which is the escape that still works for it.
                    // `own` and not `is_generator_source`: a table is the *application's*
                    // database table, and an engine that defines a top-level class would
                    // otherwise claim one by pluralizing its name and compete with the
                    // model that really owns it. Two of this loop's six outputs mean "the
                    // application" rather than "a class the reader can name", and this is
                    // the first; `hosts` is the other.
                    if own
                        && !name.contains("::")
                        && let Some(table) = rails::table_of(&name)
                    {
                        contribution.claims.push((table, name.clone()));
                    }
                    // The table-name half of the same sentence, and the reason it is a name and
                    // not a table: `Spree::Order` reads `spree_orders` and the `spree_`
                    // comes out of a file this loop is not allowed to open.
                    if own && name.contains("::") {
                        contribution.nested.push(name.clone());
                    }
                    // The two entry-point conventions, asked once and for both callers: the
                    // same `rails::convention_of` the reader itself asks, so which
                    // documents are worth opening and which classes are worth reading
                    // cannot disagree. `include` only — an `extend Sidekiq::Worker` puts
                    // the hook nowhere and is not the shape.
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
                    inherits |= rails::convention_of(superclass.as_deref(), &mixins).is_some();
                    // The includer edge, read here for `superclasses`' reason: an
                    // `include` is recorded on the definition at index time, so which
                    // module it names is a question about the graph rather than about the
                    // file the macro is written in. Spelled as written and resolved after
                    // the loop, because the set it is resolved against is only complete
                    // when every document has been seen.
                    contribution
                        .included
                        .extend(mixins.iter().map(|written| (name.clone(), written.clone())));
                    // The other one, and it stays `own`-only even though `List::Routes`
                    // does not. A host is a class the *application's* route helpers are
                    // `include`d into: an engine's own controllers are hosts in Rails and
                    // reach their own helpers by their own mechanism, so `ActiveStorage`'s
                    // six would each cost an `include` of a module holding nothing for
                    // them.
                    if own && rails::hosts_routes(&name, superclass.as_deref(), &mixins, false) {
                        contribution.hosts.push(Owner::Instance(name.clone()));
                    }
                    if let Some(superclass) = superclass {
                        contribution.superclasses.push((name.clone(), superclass));
                    }
                    contribution.declared.push((name, false));
                }
                Definition::Module(module) => {
                    if let Some(name) = self.qualified_name(module.name_id()) {
                        if helper {
                            contribution.helpers.push(name.clone());
                        }
                        if own && rails::hosts_routes(&name, None, &[], true) {
                            contribution.hosts.push(Owner::Module(name.clone()));
                        }
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
                        contribution.declared.push((name, true));
                    }
                }
                // A YARD tag is a comment above a `def`, and indexing already carried the
                // comments into the graph — so which files are worth parsing for tags is a
                // question the graph answers without opening one. The name is read for
                // `def self.table_name_prefix`, which is the one thing on any of these lists
                // that a file *defines* rather than calls or references.
                Definition::Method(method) => {
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
                        for (found, want) in defines.iter_mut().zip(&WANTS) {
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
        for (((want, names), constants), defined) in WANTS
            .iter()
            .zip(&filters.calls)
            .zip(&filters.constants)
            .zip(defines)
        {
            if !own && !want.engines {
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
                || (want.inherits && inherits)
                || defined
            {
                contribution.lists.push(want.list);
            }
        }
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
    /// and `rails::read_routes` takes a prefix that another file's parse computed. A memo for
    /// either would need a second input compared, which is exactly the design this one rejects.
    fn refresh_sources(&mut self, context: &Context) {
        let mut wants: BTreeMap<String, Wanted> = BTreeMap::new();
        for uri in context.documents(List::Models) {
            wants.entry(uri.clone()).or_default().model = true;
        }
        for uri in context.documents(List::Renamed) {
            wants.entry(uri.clone()).or_default().names = true;
        }
        for uri in context.documents(List::Annotated) {
            wants.entry(uri.clone()).or_default().annotated = true;
        }
        for uri in context.documents(List::Entrypoints) {
            wants.entry(uri.clone()).or_default().entrypoints = true;
        }
        // The same filter `schema_declarations` applies, asked here so that a `schema.rb` that
        // is not one is never read: the suffix puts a document on the list and
        // [`rails::is_schema`] decides whether it really is a schema.
        for uri in context.documents(List::Schemas) {
            if DocUri::from_uri_str(uri)
                .and_then(|uri| uri.to_path())
                .is_some_and(|path| rails::is_schema(&path))
            {
                wants.entry(uri.clone()).or_default().schema = Some(false);
            }
        }
        for uri in &context.dumps {
            wants.entry(uri.as_str().to_owned()).or_default().schema = Some(true);
        }

        // A file that has left every list keeps nothing here. Not an optimisation: the memo is
        // keyed by URI and a file that is deleted and written again is a different file, so an
        // entry nothing asks for is an entry nothing will ever check the freshness of.
        self.sources.retain(|uri, _| wants.contains_key(uri));

        let wants: Vec<(String, DocUri, Wanted)> = wants
            .into_iter()
            .filter_map(|(key, wanted)| Some((DocUri::from_uri_str(&key)?, key, wanted)))
            .map(|(uri, key, wanted)| (key, uri, wanted))
            .collect();
        for (key, uri, wanted) in wants {
            let fresh = self.freshness(&uri);
            if self
                .sources
                .get(&key)
                .is_some_and(|held| held.fresh == fresh && held.wanted == wanted)
            {
                continue;
            }
            // Read before the map is touched at all, because `with_text` borrows the whole of
            // `self` — and because a file that has gone must take its entry with it rather than
            // leave a parse nothing can refresh.
            let Some(source) = self.with_text(&uri, |text| text.text().to_owned()) else {
                self.sources.remove(&key);
                continue;
            };
            self.reads += 1;
            let name = self.workspace_relative(&uri);
            self.sources
                .insert(key, Cached::read(fresh, wanted, name, &source));
        }
    }

    /// How the text of one document is identified for [`Cached`] — see [`Fresh`] for why the
    /// two authorities cannot share one answer.
    fn freshness(&self, uri: &DocUri) -> Fresh {
        match self.open.get(uri) {
            Some(open) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                open.text.text().hash(&mut hasher);
                Fresh::Buffer(hasher.finish())
            }
            None => Fresh::Disk(uri.to_path().as_deref().and_then(stamp_of)),
        }
    }

    fn schema_dumps(&self) -> Vec<DocUri> {
        let Ok(entries) = std::fs::read_dir(self.workspace.root().join("db")) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| rails::is_structure(path) && self.workspace.admits(path))
            .filter_map(|path| DocUri::from_path(&path))
            .collect()
    }

    /// What `db/*schema.rb` and `db/*structure.sql` declare, and how many columns.
    fn schema_declarations(
        &self,
        context: &Context,
        retyped: &BTreeMap<String, BTreeSet<String>>,
        into: &mut BTreeMap<String, (DocUri, Facts)>,
    ) -> usize {
        // Every one of them before any of them writes a line, because the ambiguity rule below
        // is about tables *across* files. Both readers end at a `rails::Schema`, so from here
        // down the ambiguity rule, the claims, the provenance line and
        // everything downstream cannot tell a dump from a `schema.rb`. Which of the two parsed
        // it, and whether the suffix on the list really named a schema at all, are
        // [`Analysis::refresh_sources`]' decisions: a document with no `schema` slot was never
        // one.
        let schemas: Vec<(DocUri, String, Arc<rails::Schema>)> = context
            .documents(List::Schemas)
            .iter()
            .map(String::as_str)
            .chain(context.dumps.iter().map(DocUri::as_str))
            .filter_map(|key| {
                let held = self.sources.get(key)?;
                Some((
                    DocUri::from_uri_str(key)?,
                    held.name.clone(),
                    held.schema.clone()?,
                ))
            })
            .collect();
        if schemas.is_empty() {
            return 0;
        }

        // A table two schemas declare is declared by neither, for the same reason a table two
        // classes claim is claimed by neither: the model would answer with two schemas at once,
        // and — worse, because it is silent — `Types::harvest` would keep whichever column was
        // read last, which is a type that depends on document order.
        let mut once: HashMap<&str, usize> = HashMap::new();
        for (_, _, schema) in &schemas {
            for table in schema.table_names() {
                *once.entry(table).or_default() += 1;
            }
        }
        let mut tables = self.model_tables(context);
        tables.retain(|table, _| once.get(table.as_str()) == Some(&1));

        let mut columns = 0;
        for (uri, name, schema) in &schemas {
            let facts = schema.signatures(name, &tables, retyped);
            columns += facts.len();
            merge(into, uri, facts);
        }
        columns
    }

    /// Every file on the model list, read and parsed once.
    ///
    /// Separate from the declaring below because two generators read it: the schema needs
    /// [`retyped_columns`] before it writes a line, and the models themselves need every file
    /// parsed before any of them does — which element types need a relation class is a fact
    /// across files.
    fn model_sources(&self, context: &Context) -> Vec<(DocUri, String, Arc<rails::Model>)> {
        context
            .documents(List::Models)
            .iter()
            .filter_map(|key| {
                let held = self.sources.get(key)?;
                Some((
                    DocUri::from_uri_str(key)?,
                    held.name.clone(),
                    held.model.clone()?,
                ))
            })
            .collect()
    }

    /// What the association macros and `enum` declare.
    ///
    /// **Exactly one** file may write each relation class. The one that does is the first in URI
    /// order that asked for it — an arbitrary choice made deterministic, which is all it has to
    /// be, because a relation class is mapped to no line of anybody's code and so is the same
    /// class whichever document holds it.
    fn model_declarations(
        &self,
        context: &Context,
        models: &[(DocUri, String, Arc<rails::Model>)],
        columns: &BTreeSet<(String, String)>,
        into: &mut BTreeMap<String, (DocUri, Facts)>,
    ) -> usize {
        // A class gets a relation class for either of two reasons: **some macro made it a
        // collection**, or **it is a model** — every class ActiveRecord will answer `where` and
        // `first` on, whether or not it writes a macro. Neither implies the other, so this is a
        // union and deliberately not a replacement; [`models_of`] has the classes that are the
        // first and not the second.
        //
        // Both are then filtered the same way: the application has to define the class itself,
        // and the name the relation would take has to be one the application has *not* already
        // used. A project that wrote its own `Comment::Relation` meant something by it, and
        // shadowing it is the one way this pass can make an answer worse rather than absent.
        //
        // The host test asks the union itself, one question earlier: whether the class a macro is
        // written **on** is a model. The two are the same set and must stay one — a serializer
        // is not a model because a filter about naming collided, it is not a model because
        // nothing says it is — so `relations` is derived from this rather than built beside it.
        let modelled: BTreeSet<String> = models
            .iter()
            .flat_map(|(_, _, model)| model.collections(&context.classes))
            .map(str::to_owned)
            .chain(context.models.iter().cloned())
            .filter(|element| context.classes.contains(element))
            .collect();
        // The element half is deliberately **not** gated by the host test. A serializer's
        // `has_many :statuses` is still evidence that `Status` is a collection somewhere in the
        // application — it is serializing a model's association — and gating the input of the
        // set the gate reads would make it circular. What the host test removes is the
        // declaration, never the relation.
        // **A project that declares [`rails::RELATION_BASE`] itself meant something by it**, and
        // a class this pass wrote into would answer with both its members and ours — while every
        // relation in the workspace would inherit whatever they meant. It is the rule
        // `Comment::Relation` and `ROUTE_HELPERS` follow, asked of the one class the query
        // interface invents. It withdraws the **relations**, which is that same collision
        // behaviour reached by a second road rather than a rule of its own: with no
        // relation class there is nothing for a `has_many` to return, so the whole half declines
        // together instead of leaving a `-> Comment::Relation` naming a class nothing declares.
        let interface = !context.namespaces.declares(rails::RELATION_BASE);
        let relations: BTreeSet<String> = modelled
            .iter()
            .filter(|_| interface)
            .filter(|element| !context.classes.contains(&rails::relation_of(element)))
            // A name **rubydex invented** is not a constant path, and a generated declaration
            // on one costs the whole document — see [`generated::is_constant_path`]. An
            // anonymous `Class.new(Spree::Base)` in a spec is an ActiveRecord model by every
            // rule this crate has, and solidus writes 38 of them.
            //
            // The name and **not** `Namespaces::spellable`, which is the rule about a
            // *namespace*. A relation class is joined onto whatever namespace its element is
            // in, and holding it to that rule here would withdraw every relation whose element
            // sits under a Zeitwerk-conjured
            // module, which over chatwoot is 21 classes — every `Channel::` and every
            // `Captain::` — 285 declarations and **173 positions that answer nothing at all**.
            .filter(|element| crate::generated::is_constant_path(element))
            .cloned()
            .collect();

        // Which document writes each relation class: **the first that asks for it**, exactly as
        // before, and only then — for a model no macro anywhere asks about — the document that
        // defines the class.
        //
        // The order of these two loops is load-bearing and was measured rather than reasoned.
        // Preferring the defining document for *every* element moves relation classes that
        // already had a home, and moving them loses answers: over chatwoot it cost **32
        // positions**, `Channel::Telegram.find_by` among them, which stopped resolving to its
        // own class side and started resolving to the one it inherits from `ApplicationRecord`
        // — the very defect the per-model class side exists to fix. Nothing in a relation
        // class is mapped, so where it lives should not matter and empirically does; **until
        // that is understood, nothing that already had a home may be relocated.**
        let index: HashMap<&str, usize> = models
            .iter()
            .enumerate()
            .map(|(at, (uri, _, _))| (uri.as_str(), at))
            .collect();
        let mut assigned: Vec<BTreeSet<String>> = vec![BTreeSet::new(); models.len()];
        let mut written: BTreeSet<&str> = BTreeSet::new();
        for (at, (_, _, model)) in models.iter().enumerate() {
            for element in model.collections(&context.classes) {
                if relations.contains(element) && written.insert(element) {
                    assigned[at].insert(element.to_owned());
                }
            }
        }
        for element in &relations {
            if written.contains(element.as_str()) {
                continue;
            }
            if let Some(at) = context
                .defined_in
                .get(element.as_str())
                .and_then(|uri| index.get(uri.as_str()))
            {
                assigned[*at].insert(element.clone());
            }
        }

        // The query interface goes in **one** document and it is the first that writes a
        // relation at all, for the reason exactly one file writes the `MessageDelivery` stub:
        // it is one class however many relations inherit it, and N copies would be N
        // declarations of one class saying the same hundred and twenty things. A workspace with
        // no relation in it writes no base class, because nothing would inherit one.
        let shared = assigned.iter().position(|elements| !elements.is_empty());

        // The class side's own base. `Story.where` is *inherited* — Ruby
        // follows a class object's singleton chain up the class chain — so the interface goes
        // once on each model's **base**, and a project pays for it per base rather than per
        // model. Which document writes it is the one that defines the base, exactly as an
        // unasked-for relation class goes in the document that defines its element; a base whose
        // file is not on this list falls in with the shared half.
        //
        // The set is built from `relations` rather than from `modelled` so that the *reach* is
        // the one that shipped: every class that had a class side has one, through its base.
        // What it widens is a real gap — a model with no `has_many` and no `scope` is on no
        // list and inherits one anyway — and that is a consequence of putting the declaration
        // where Rails puts it rather than a second rule.
        let mut bases: Vec<BTreeSet<String>> = vec![BTreeSet::new(); models.len()];
        for element in &relations {
            let base = base_of(
                element,
                &context.superclasses,
                &context.models,
                &context.namespaces,
            );
            let at = context
                .defined_in
                .get(base.as_str())
                .and_then(|uri| index.get(uri.as_str()))
                .copied()
                .or(shared);
            if let Some(at) = at {
                bases[at].insert(base);
            }
        }

        let mut members = 0;
        for (at, (uri, name, model)) in models.iter().enumerate() {
            let mut facts = model.signatures(
                name,
                &rails::Elsewhere {
                    known: &context.classes,
                    framework: &context.framework,
                    models: &modelled,
                    relations: &relations,
                    emit: &assigned[at],
                    bases: &bases[at],
                    includers: &context.includers,
                    columns,
                },
            );
            if shared == Some(at) {
                rails::relation_base(&mut facts);
            }
            members += facts.len();
            merge(into, uri, facts);
        }
        members
    }

    /// What a `sig` block or a YARD tag says, and how many methods that typed.
    fn annotation_declarations(
        &self,
        context: &Context,
        into: &mut BTreeMap<String, (DocUri, Facts)>,
    ) -> usize {
        let mut typed = 0;
        for key in context.documents(List::Annotated) {
            let Some(facts) = self
                .sources
                .get(key)
                .and_then(|held| held.annotated.clone())
            else {
                continue;
            };
            let Some(uri) = DocUri::from_uri_str(key) else {
                continue;
            };
            typed += facts.len();
            merge(into, &uri, facts);
        }
        typed
    }

    /// What a `Struct.new` or a `Data.define` installs on the constant it is assigned.
    ///
    /// The same shape as the annotations above and for the same reason — a reader that needs
    /// nothing but the text and the file's name needs nothing from the graph either — and the
    /// only generator in the pass that is not Rails'. [`structs::read`] decides which of the
    /// documents this list holds really writes one; a file that mentions `Struct` and never
    /// calls it says nothing and is not recorded.
    fn struct_declarations(
        &self,
        context: &Context,
        into: &mut BTreeMap<String, (DocUri, Facts)>,
    ) -> usize {
        let mut members = 0;
        for uri in context
            .documents(List::Structs)
            .iter()
            .filter_map(|uri| DocUri::from_uri_str(uri))
        {
            let Some(source) = self.with_text(&uri, |text| text.text().to_owned()) else {
                continue;
            };
            let facts = structs::read(&source, &self.workspace_relative(&uri), &context.namespaces);
            members += facts.len();
            merge(into, &uri, facts);
        }
        members
    }

    /// What a mailer's actions and a job's `perform` install on the class side.
    ///
    /// **Exactly one file writes the [`rails::MESSAGE_DELIVERY`] stub**, for the reason exactly
    /// one file writes a relation class: it is one type however many mailers reach it, and N
    /// copies of it would be N declarations of one class saying the same four things. The one
    /// that does is the first mailer in URI order — an arbitrary choice made deterministic,
    /// which is all it has to be, because nothing in that class is mapped to any file.
    ///
    /// An application that declares `ActionMailer::MessageDelivery` itself writes it instead,
    /// and this says nothing: a project that spelled that constant meant something by it. What
    /// this cannot ask is whether the *gem* declares it, because declarations do not exist
    /// until `resolve` — and it does not need to, because both are then one declaration and
    /// this one carries no place.
    fn entrypoint_declarations(
        &self,
        context: &Context,
        into: &mut BTreeMap<String, (DocUri, Facts)>,
    ) -> usize {
        let sources: Vec<(DocUri, String, Arc<rails::Entrypoints>)> = context
            .documents(List::Entrypoints)
            .iter()
            .filter_map(|key| {
                let held = self.sources.get(key)?;
                Some((
                    DocUri::from_uri_str(key)?,
                    held.name.clone(),
                    held.entrypoints.clone()?,
                ))
            })
            .collect();
        let delivery = (!context.namespaces.declares(rails::MESSAGE_DELIVERY))
            .then(|| {
                sources
                    .iter()
                    .find(|(_, _, entrypoints)| entrypoints.delivers())
                    .map(|(uri, _, _)| uri.as_str().to_owned())
            })
            .flatten();

        let mut installed = 0;
        for (uri, name, entrypoints) in &sources {
            let facts = entrypoints.signatures(name, delivery.as_deref() == Some(uri.as_str()));
            installed += facts.len();
            merge(into, uri, facts);
        }
        installed
    }

    /// What the routing DSL names, and where each helper is `include`d.
    ///
    /// **Two reads and the second needs the first.** A `draw :admin` is the only thing that says
    /// where `config/routes/admin.rb` sits in the scope, so a drawn file cannot be read until
    /// the file that draws it has been, and it is read *at that prefix*. Its declarations then
    /// go into its **own** generated document, because a span is a byte range with no URI and a
    /// mapping recorded against the wrong file opens the wrong line confidently.
    ///
    /// **Exactly one file declares each helper.** Two routes files naming one — an engine and
    /// the application, or the same name in `config/routes.rb` and a drawn file — would land in
    /// two generated documents, where [`Facts`]' precedence cannot see them and RBS holds two
    /// `def story_path:` lines as an overload set. First in URI order writes it, which is item
    /// 13's rule for a shared relation class and is arbitrary in the same harmless way.
    fn route_declarations(
        &self,
        context: &Context,
        into: &mut BTreeMap<String, (DocUri, Facts)>,
    ) -> usize {
        // An application that declares the constant itself meant something by it, and a module
        // this pass wrote into would answer with both its members and ours. The rule for
        // `Comment::Relation`, and the whole feature is what it costs — which is the right price
        // for never shadowing a name somebody chose.
        if context.namespaces.declares(rails::ROUTE_HELPERS) {
            return 0;
        }
        let mains: Vec<DocUri> = context
            .documents(List::Routes)
            .iter()
            .filter_map(|uri| DocUri::from_uri_str(uri))
            .filter(|uri| uri.to_path().is_some_and(|path| rails::is_routes(&path)))
            .collect();
        if mains.is_empty() {
            return 0;
        }
        let mut sources: Vec<(DocUri, String, rails::Routes)> = Vec::new();
        for uri in mains {
            let Some(source) = self.with_text(&uri, |text| text.text().to_owned()) else {
                continue;
            };
            // The receiver of a `draw` means nothing in the project's own routes file and
            // everything in a gem's — `rails::Whose` is the argument for it.
            let whose = self.whose(&uri);
            let routes = rails::read_routes(&source, &[], whose);
            for draw in routes.draws() {
                if let Some(drawn) = self.drawn(&uri, &draw.name)
                    && let Some(source) = self.with_text(&drawn, |text| text.text().to_owned())
                {
                    let name = self.workspace_relative(&drawn);
                    // `Whose::Own` however the drawer was reached, and that is not a
                    // shortcut: a drawn file has no wrapper at all — its statements *are* the
                    // body — and the file that drew it has already cleared the gate. Asking a
                    // gem's drawn file for its own `Rails.application.routes.draw` would decline
                    // every one of them.
                    sources.push((
                        drawn,
                        name,
                        rails::read_routes(&source, &draw.prefix, rails::Whose::Own),
                    ));
                }
            }
            let name = self.workspace_relative(&uri);
            sources.push((uri, name, routes));
        }
        // The project's own files first, and it is the first-wins assignment below that makes
        // this load-bearing rather than cosmetic: an application that names `rails_blob_path`
        // itself must be the one that declares it, and URI order between a workspace path and a
        // gem path is whichever string happens to sort lower. It also keeps the `include`s in a
        // file the user has, since they go in `sources[0]`.
        sources.sort_by(|left, right| {
            let key = |uri: &DocUri| (!self.is_own_code(uri.as_str()), uri.as_str().to_owned());
            key(&left.0).cmp(&key(&right.0))
        });

        let mut written: BTreeSet<String> = BTreeSet::new();
        let mut assigned: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for (uri, _, routes) in &sources {
            for helper in routes.names() {
                if written.insert(helper.to_owned()) {
                    assigned
                        .entry(uri.as_str())
                        .or_default()
                        .insert(helper.to_owned());
                }
            }
        }

        let nothing = BTreeSet::new();
        let mut helpers = 0;
        for (index, (uri, name, routes)) in sources.iter().enumerate() {
            let emit = assigned.get(uri.as_str()).unwrap_or(&nothing);
            let mut facts = routes.signatures(name, emit);
            // The `include`s go in one document and it is the first, for the reason exactly one
            // file writes the `MessageDelivery` stub: they are a property of the *application*
            // rather than of any routes file, and N copies would be N identical declarations.
            if index == 0 {
                facts.extend(rails::mixins(&context.hosts));
            }
            helpers += facts.len();
            merge(into, uri, facts);
        }
        helpers
    }

    /// What `delegate` declares, and the two hops each name's type needs.
    ///
    /// This is the whole of phase two. `delegate :name, to: :user` on `Story` needs
    /// `Story#user -> User` and then `User#name -> String`, and both of those are facts the
    /// generators above wrote — into *other files'* documents, in this same pass, with nothing
    /// resolved and nothing indexed. [`Facts::returns`] is the question and the union below is
    /// what it is asked of.
    ///
    /// **The union is built once, and only when something asks for it.** Once, because a
    /// `delegate` may derive from any file's facts and building it per file would be a merge per
    /// file; only on demand, because a workspace with no `delegate` in it must not pay a merge
    /// of every fact in the project for a feature it does not use.
    ///
    /// **What phase two writes is not in the union**, which is what makes a `delegate` through a
    /// `delegate` answer `untyped` rather than starting a fixed-point iteration over a graph a
    /// user can write a cycle into. The declarations still land in the source file's own
    /// document, where [`Facts`]' precedence can see them: a column named `title` outranks a
    /// `delegate :title`, because the schema is what the database *is*.
    fn delegate_declarations(
        &self,
        models: &[(DocUri, String, Arc<rails::Model>)],
        into: &mut BTreeMap<String, (DocUri, Facts)>,
    ) -> usize {
        if !models.iter().any(|(_, _, model)| model.derives()) {
            return 0;
        }
        let mut project = Facts::default();
        for (_, facts) in into.values() {
            project.absorb(facts);
        }

        let mut declared = 0;
        for (uri, name, model) in models {
            let facts = model.derived(name, &project);
            declared += facts.len();
            merge(into, uri, facts);
        }
        declared
    }

    /// The document `draw :admin` reads, which is `config/routes/admin.rb` beside the drawer.
    ///
    /// `None` when the path cannot be built or the file is not indexed, and the second is not a
    /// failure: a routes file that draws something outside `index.include` draws nothing here,
    /// which is the same answer the rest of this pass gives for a file it was told not to read.
    fn drawn(&self, drawer: &DocUri, name: &str) -> Option<DocUri> {
        let path = drawer.to_path()?;
        DocUri::from_path(&path.parent()?.join("routes").join(format!("{name}.rb")))
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

    /// Which of the user's own classes read which table, and there is more than one of them.
    ///
    /// The direction is the safety argument, and it is the opposite of the obvious one: every
    /// table is looked up **from** a class that exists, by pluralizing its name, rather than
    /// singularizing a table name and hoping a class answers to it. Both directions need the
    /// same irregular rules; only this one fails toward *nothing*. A class whose plural names
    /// no table declares nothing, and a table no class claims declares nothing — where
    /// singularizing `statuses` badly could land on a class that exists and is not a model.
    ///
    /// # A table really is read by more than one class
    ///
    /// Answering a **single** class per table follows from "a table two classes claim is claimed
    /// by neither". That rule protects against one
    /// thing — the inflector landing two different names on one table, where at most one of
    /// them can be right — and it was paying for that protection with a case that is not a
    /// collision at all. `Account` and `Mastodon::CLI::Maintenance::Account` compute the same
    /// table because they are the same convention applied twice, and both of them really do
    /// read `accounts`; mastodon writes **150** such classes.
    ///
    /// So the guard is narrowed to exactly what it was built for: several claimants are kept
    /// when they all **demodulize to the same name**, and a table two *different* names reach
    /// is still claimed by neither. Measured over six applications that discriminator declines
    /// four tables — discourse's `GroupUser` against `GroupUsers` three times, which is the
    /// inflector collision the rule exists for and which was already declined — and keeps all
    /// **171** of the legitimate extra claimants.
    ///
    /// A class that **names its own table** is not inflected at all and never makes anything
    /// ambiguous: `self.table_name = "settings"` is a fact the author wrote, not a guess this
    /// crate made. Letting an override *replace* the conventional claimant costs **four models
    /// in six corpora** their columns — mastodon's throwaway `MoveUserSettings::LegacySetting`
    /// takes `settings` off the `Setting` model, and discourse's `Post` and `Category` go the
    /// same way — so the override joins the claim rather than replacing it, unless the class
    /// whose name implies the table is not a model.
    fn model_tables(&self, context: &Context) -> BTreeMap<String, Vec<String>> {
        let names: Vec<Arc<rails::TableNames>> = context
            .documents(List::Renamed)
            .iter()
            .filter_map(|key| self.sources.get(key)?.names.clone())
            .collect();

        // Rails' own precedence, and it is one clause of `isolate_namespace`:
        // `unless mod.respond_to?(:table_name_prefix)`. A module that writes the method out
        // wins over the engine that isolated it, so the isolated ones go in first.
        let mut prefixes: BTreeMap<&str, String> = BTreeMap::new();
        for candidates in names.iter().flat_map(|read| &read.isolated) {
            if let Some(owner) = candidates
                .iter()
                .find(|name| context.classes.contains(*name))
                // `isolate_namespace` names its module by the constant it *resolves to*, so the
                // prefix cannot be spelled until the candidate list has been settled here.
                && let Some(prefix) = rails::engine_prefix(owner)
            {
                prefixes.insert(owner, prefix);
            }
        }
        for (owner, prefix) in names.iter().flat_map(|read| &read.prefixes) {
            prefixes.insert(owner, prefix.clone());
        }
        let suffixes: BTreeMap<&str, String> = names
            .iter()
            .flat_map(|read| &read.suffixes)
            .map(|(owner, suffix)| (owner.as_str(), suffix.clone()))
            .collect();
        let overrides: BTreeMap<&str, &str> = names
            .iter()
            .flat_map(|read| &read.overrides)
            .map(|(class, table)| (class.as_str(), table.as_str()))
            .collect();

        // The tables some class has *said* it reads, which is the one place an inflected claim
        // now meets a written one. Letting the written one simply replace it is right where
        // the writer is the real model — discourse's `TopicViewItem` says
        // `topic_views` and the `TopicView` whose name implies it is a view object — and wrong
        // where the writer is a throwaway: mastodon's `MoveUserSettings::LegacySetting` says
        // `settings` and took them off the `Setting` model, and discourse's three test doubles
        // say `posts` and take them off `Post`. So the guess survives the meeting only when the
        // class it is about is a model, and both readings are right for the reason they are
        // right.
        let named: BTreeSet<&str> = overrides.values().copied().collect();
        let mut claimed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (table, classes) in &context.claims {
            for class in classes.iter().filter(|class| {
                !overrides.contains_key(class.as_str())
                    && (!named.contains(table.as_str()) || context.is_model(class))
            }) {
                claimed
                    .entry(table.clone())
                    .or_default()
                    .push(class.clone());
            }
        }
        for name in &context.nested {
            if overrides.contains_key(name.as_str()) || !context.is_model(name) {
                continue;
            }
            let Some((parent, last)) = name.rsplit_once("::") else {
                continue;
            };
            // `compute_table_name`'s other branch: a model nested inside another *model* is
            // `parent_singular_child_plural`, which needs the parent's own table and then the
            // parent's parent's. It is declined rather than approximated, and the decline is
            // measured: 22 classes in six applications are nested this way and **not one** of
            // them names a table any of those applications has.
            if context.is_model(parent) {
                continue;
            }
            // The joined name a declaration on this would be written under must introduce no
            // namespace. It declines nothing in six corpora and is here because the damage it
            // prevents is silent.
            if !context.namespaces.spellable(name) {
                continue;
            }
            let Some(table) = rails::table_of(last) else {
                continue;
            };
            claimed
                .entry(format!(
                    "{}{table}{}",
                    affix(&prefixes, name),
                    affix(&suffixes, name)
                ))
                .or_default()
                .push(name.clone());
        }

        let mut tables: BTreeMap<String, Vec<String>> = claimed
            .into_iter()
            .map(|(table, mut classes)| {
                // One class written in two places is **one claimant**. `claims` is filled
                // per *definition*, so a model reopened to nest something under it pushed its
                // own name twice — and the old rule, which asked for exactly one entry,
                // declined it. **Three models in six corpora lost every column to that**:
                // forem writes `class AuditLog` again in `app/queries/audit_log/`, and
                // discourse writes `class Reviewable < ActiveRecord::Base` in six
                // `lib/reviewable/` files. The narrowed rule below already admits them, because
                // one name demodulizes to itself; this is here so that "one claimant" means one
                // class rather than one `class` keyword, and so the list handed to the schema
                // is what it says it is.
                classes.sort_unstable();
                classes.dedup();
                (table, classes)
            })
            .filter(|(_, classes)| {
                // The narrowed ambiguity rule. One `demodulize` shared by every claimant is the
                // convention applied more than once; two are the inflector having landed two
                // names on one table, where at most one of them can be right.
                let mut demodulized = classes
                    .iter()
                    .map(|class| class.rsplit("::").next().unwrap_or(class));
                let first = demodulized.next();
                demodulized.all(|last| Some(last) == first)
            })
            .collect();
        for (class, table) in overrides {
            tables
                .entry(table.to_owned())
                .or_default()
                .push(class.to_owned());
        }
        tables
    }
}

/// The innermost enclosing name that declares one, or nothing at all.
///
/// `full_table_name_prefix` is
/// `(module_parents.detect { |p| p.respond_to?(:table_name_prefix) } || self).table_name_prefix`,
/// and `module_parents` is innermost first — so `Spree::Admin::Order` asks `Spree::Admin` before
/// it asks `Spree`. The `|| self` branch is the empty string for every class in an application
/// that has not set `ActiveRecord::Base.table_name_prefix` globally, which is Ruby that only
/// runs and is therefore what the empty answer here means.
fn affix<'a>(affixes: &'a BTreeMap<&str, String>, name: &str) -> &'a str {
    name.rmatch_indices("::")
        .find_map(|(at, _)| affixes.get(&name[..at]))
        .map_or("", String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One document's contribution, as [`Analysis::contribution`] would have built it.
    fn declaring(name: &str, superclass: &str, table: &str) -> Contribution {
        Contribution {
            claims: vec![(table.to_owned(), name.to_owned())],
            superclasses: vec![(name.to_owned(), superclass.to_owned())],
            declared: vec![(name.to_owned(), false)],
            ..Contribution::default()
        }
    }

    fn merged(order: [(&str, Contribution); 2]) -> Context {
        let mut context = Context::default();
        let mut included = Vec::new();
        for (uri, contribution) in order {
            context.absorb(uri, contribution, &mut included);
        }
        context.settle();
        context
    }

    #[test]
    fn two_documents_merge_to_the_same_context_in_either_order() {
        // The property the per-document fingerprints stand on, stated where it is decided
        // rather than asserted through a walk that cannot be made to change its order.
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
            let mut contribution = declaring("Double", "Object", "doubles");
            contribution
                .claims
                .push(("doubles".to_owned(), "Stunt".to_owned()));
            contribution
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
            forwards.claims["doubles"],
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
        let mut context = Context::default();
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
    fn a_fingerprint_is_of_the_contribution_and_not_of_the_document() {
        // The property, stated on the function that decides it: two documents whose
        // contributions are equal have one fingerprint, whatever else is different about them.
        let one = declaring("Story", "ApplicationRecord", "stories");
        let two = declaring("Story", "ApplicationRecord", "stories");
        assert_eq!(fingerprint(&one), fingerprint(&two));
        assert_ne!(
            fingerprint(&one),
            fingerprint(&declaring("Story", "Object", "stories"))
        );
        // And an empty contribution is a value like any other, which is what lets the walk
        // store one for a document that declares nothing.
        assert_eq!(
            fingerprint(&Contribution::default()),
            fingerprint(&Contribution::default())
        );
    }
}
