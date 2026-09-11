//! Rails, as a body of knowledge the pass drives rather than one it is written around.
//!
//! **This is the orchestration and not the reading.** Every line that turns text into facts is in
//! [`workspace::rails`](crate::workspace::rails), whose property is *pure text in, text out, no
//! I/O and no graph* and whose every file is held at 100% coverage. What lives here is the other
//! half: which documents each reader is handed, what this project has to have said for a list to
//! be worth filling, and which conventions decide a class is one of Rails'.
//!
//! Nothing in this file may open a file or read the graph either — it is handed what the one walk
//! already saw, in a [`Seen`](super::Seen), and folds it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::{Context, Counted, Declared, Declaring, Fresh, ListId, Reading, Seen, Sources, Wants};
use crate::analysis::views::Views;
use crate::generated::{At, Facts, Owner, candidates};
use crate::workspace::{DocUri, Features, rails};

/// Documents whose name ends `schema.rb`. Whether one really *is* a schema is
/// [`rails::is_schema`]'s decision, not this list's — the suffix is the cheap half.
pub const SCHEMAS: ListId = ListId("rails.schemas");
/// Documents that say something about the **name** of a table rather than about its columns:
/// `self.table_name=`, which is the escape from every naming convention the schema generator
/// applies, and the two ways a namespace declares the prefix every table under it carries.
///
/// One list and not two, because [`rails::read_table_names`] is one walk: a file that says both
/// would otherwise be parsed twice to be told the same thing.
pub const RENAMED: ListId = ListId("rails.renamed");
/// Documents that call one of [`rails::MODEL_CALLS`].
pub const MODELS: ListId = ListId("rails.models");
/// Documents that install a class method on every class including a module: a `class_methods do`,
/// a hand-written `module ClassMethods`, or an `included do` holding a bare `extend`.
///
/// **The one list a gem's `lib/` may join**, and the only reason this is a list of its own rather
/// than a flag on [`MODELS`]. The edge it reads is written almost entirely in gems — `validates`,
/// `scope`, `belongs_to` and `has_many` are all a Rails concern's `ClassMethods`, and
/// `ActiveModel::API`'s two `extend` lines install `model_name` on every model in every
/// application. Opening [`MODELS`] to gems instead would declare a gem's own `has_many` and `enum`
/// onto its own classes, which is a different decision with a different argument, and this list
/// needs none of it.
pub const CONCERNS: ListId = ListId("rails.concerns");
/// Documents defining a class that [`rails::convention_of`] recognises — a mailer, a job or a
/// Sidekiq worker. The only list filled by what a class *inherits* rather than by anything the
/// file calls, because these two conventions have no macro to look for.
pub const ENTRYPOINTS: ListId = ListId("rails.entrypoints");
/// Documents named `routes.rb`. Whether one really is an application's routes is
/// [`rails::is_routes`]' decision, exactly as the schema list defers to [`rails::is_schema`]; the
/// files a routes file *draws* are not on this list at all, because which they are is something
/// only the file that draws them says.
pub const ROUTES: ListId = ListId("rails.routes");
/// The one document named `config/application.rb`, which hosts what the **framework's own
/// singletons** return — `Rails.root`, `Rails.cache`, `Rails.application`, `Time.zone`.
///
/// **The only list whose generator reads almost nothing out of the file it is keyed by**, and the
/// only one that could have been keyed by anything: the four returns are a property of the
/// framework rather than of any document, so this list answers *where to write them down* rather
/// than *what to read*. `config/application.rb` is the honest host because it is the file every
/// Rails application has and no other kind of project does — the same marker
/// [`Features::resolve`](crate::workspace::Features::resolve) detects `auto` by — and because the
/// one thing the generator *does* read is in it: the `class < Rails::Application` whose instance
/// `Rails.application` is.
///
/// A full path and not a suffix, unlike the two lists above: `application.rb` alone is
/// `app/models/application.rb` in three of the six corpora.
pub const FRAMEWORK: ListId = ListId("rails.framework");

/// The rows, and the nine Rails literals that used to be spelled in the pass.
static WANTS: [Wants; 7] = [
    Wants {
        list: SCHEMAS,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("schema.rb"),
        tags: false,
        inherits: false,
        // an engine ships migrations, never a `schema.rb`, and gate 1 does not walk a gem's `db/`
        engines: false,
        gems: false,
        reads_only: false,
    },
    Wants {
        list: RENAMED,
        calls: &["table_name=", "isolate_namespace"],
        constants: &[],
        modules: &[],
        defines: &["table_name_prefix", "table_name_suffix"],
        path: None,
        tags: false,
        inherits: false,
        // the input to a generator that is itself closed
        engines: false,
        gems: false,
        // and it declares nothing on its own: it renames a table for a schema that has to exist
        // somewhere else
        reads_only: true,
    },
    Wants {
        list: MODELS,
        calls: &rails::MODEL_CALLS,
        constants: &[],
        modules: &[],
        defines: &[],
        path: None,
        tags: false,
        inherits: false,
        // `has_many` on `ActiveStorage::Blob` is a member of a class the user names
        engines: true,
        gems: false,
        reads_only: false,
    },
    Wants {
        list: CONCERNS,
        calls: &["class_methods", "included"],
        constants: &[],
        modules: &[rails::CLASS_METHODS],
        defines: &[],
        path: None,
        tags: false,
        inherits: false,
        // a concern's class methods are members of whatever includes it, which is a class the
        // reader names in their own file
        engines: true,
        // and the same sentence one directory further out: `ActiveModel::Validations`'
        // `ClassMethods` holds `validates`, which every model in the project calls
        gems: true,
        reads_only: false,
    },
    Wants {
        list: ENTRYPOINTS,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: None,
        tags: false,
        inherits: true,
        // `ActiveStorage::AnalyzeJob` really does get `perform_later`
        engines: true,
        gems: false,
        reads_only: false,
    },
    Wants {
        list: ROUTES,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("routes.rb"),
        tags: false,
        inherits: false,
        // an engine's `config/routes.rb` may name the *host application's* helpers, and
        // `rails::Whose` is what decides whether this one does
        engines: true,
        gems: false,
        reads_only: false,
    },
    Wants {
        list: FRAMEWORK,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("config/application.rb"),
        tags: false,
        inherits: false,
        // an engine has no `config/application.rb`, and the application it is loaded into does:
        // these are the framework's singletons, declared once wherever the application is
        engines: false,
        gems: false,
        reads_only: false,
    },
];

/// Which of the readers one document's place on the lists calls for.
///
/// A document is usually on one list and may be on five, and the reads are per *document* rather
/// than per list — which is a saving in its own right: read per list, a model file that also
/// carries a `self.table_name=` is read twice.
///
/// Compared as a whole, so a document that has joined a list since the last pass is read again
/// rather than topped up. That is not only simpler: **a document can join a list without its own
/// text changing.** The walk puts every file that *defines* a class some other file's macro names
/// onto [`MODELS`], so writing `has_many :widgets` in one file adds `widget.rb` to a list it was
/// not on — and if `widget.rb` was already read for a `self.table_name=`, its entry is fresh and
/// holds the wrong things.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    model: bool,
    /// Whether the schema reader is wanted, and whether the file is a dump rather than a
    /// `schema.rb` — which decides which of the two readers runs, and is a property of the path.
    schema: Option<bool>,
    names: bool,
    entrypoints: bool,
}

/// One source file as this module's readers last saw it, and the evidence it has not changed.
#[derive(Debug)]
pub struct Source {
    fresh: Fresh,
    /// The readers this entry was built for — see [`Wanted`] for why a document can start wanting
    /// more of them without its own text changing.
    wanted: Wanted,
    /// The path every reader writes into its provenance, which is a function of the URI.
    pub name: String,
    pub model: Option<Arc<rails::Model>>,
    pub schema: Option<Arc<rails::Schema>>,
    pub names: Option<Arc<rails::TableNames>>,
    pub entrypoints: Option<Arc<rails::Entrypoints>>,
}

impl Source {
    /// Run every reader `wanted` asks for, once, over text just read.
    fn read(fresh: Fresh, wanted: Wanted, name: String, source: &str) -> Self {
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
        }
    }
}

/// Rails.
#[derive(Debug, Default)]
pub struct Rails {
    /// Every file this module's readers have parsed, by URI.
    ///
    /// **The module's own and not the pass's**, which is what lets the readers keep their Rails
    /// syntax types: core holds a `dyn Knowledge` and learns nothing about a `rails::Model`.
    sources: HashMap<String, Source>,
    /// How many files it has read, for the one line that says the pass ran.
    pub reads: u64,
    /// The `db/*structure.sql` files core last found for it.
    ///
    /// Kept here as well as on the projection because the pass gate asks for them once before the
    /// projection exists: a dump that has appeared or gone is a change nothing else would notice.
    found: Vec<DocUri>,
}

impl Rails {
    /// What this module last parsed of one document, or nothing.
    #[must_use]
    pub fn source(&self, uri: &str) -> Option<&Source> {
        self.sources.get(uri)
    }

    /// Whose routes file this is: the project's own, or a gem's.
    fn whose(declaring: &Declaring<'_>, uri: &DocUri) -> rails::Whose {
        if (declaring.own)(uri.as_str()) {
            rails::Whose::Own
        } else {
            rails::Whose::Gem
        }
    }

    /// The view context, or an empty one a project has said it does not want.
    ///
    /// **Empty and not absent**, because `Views` is a field rather than an option and the
    /// difference a user asked for is in what it answers: `Views::default()` is switched off, so
    /// `reachable` declines before it reads a path — which is the half of `rails.views` that is
    /// not in the pass at all. A Sinatra application with an `app/views/` is exactly who that
    /// matters for.
    fn view_context_if_wanted(
        &self,
        declaring: &Declaring<'_>,
        models: &[(DocUri, String, Arc<rails::Model>)],
    ) -> Views {
        if declaring.features.views {
            view_context(declaring.context, models)
        } else {
            Views::default()
        }
    }

    fn schema_dumps(reading: &Reading<'_>) -> Vec<DocUri> {
        let Ok(entries) = std::fs::read_dir(reading.root.join("db")) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                reading.features.schema && rails::is_structure(path) && (reading.admits)(path)
            })
            .filter_map(|path| DocUri::from_path(&path))
            .collect()
    }

    /// The namespaces a **directory** declares, as module bodies holding nothing.
    ///
    /// The one generator that opens no file and reads no text: its whole input is
    /// [`Context::autoloaded`], which is a projection of the paths the walk already visited.
    ///
    /// **It declares a name and says nothing else about it**, which is all Zeitwerk does. The
    /// classes under it are in the files below it and rubydex already holds every one; what was
    /// missing is the constant they hang off.
    ///
    /// **One declaration per confirming file, and that is what gives the name its places.** A
    /// directory is not a line and can never be jumped to, so keying the generated document by
    /// the directory left a card reading `module Mod` above a jump that went nowhere — the one
    /// disagreement between `hover` and `definition` this crate is built to refuse. The file
    /// that writes `class Mod::FlaggedController` is a line, it is the line this generator read
    /// to believe the directory at all, and Ruby's own operational test says it declares the
    /// constant: delete it and `Mod` survives only because its siblings declare it too. So each
    /// of them writes the body, each in the generated document its own file keys, and the
    /// declaration rubydex merges them into has as many definitions as there are files — which
    /// is what `definitions_of` then answers with.
    ///
    /// A `user/policy.rb` written tomorrow still takes the name out of `autoloaded`, and
    /// [`Analysis::forget_stale`] still drops what declared it — now one document per file
    /// rather than one per directory, which is a shorter life and not a longer one.
    ///
    /// Every name here is spellable by construction — [`Analysis::walk`] declares the whole
    /// surviving chain into [`Context::namespaces`] before this runs — so there is no candidate
    /// to decline and no rank to lose.
    fn autoloaded_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        let mut declared = 0;
        for (name, confirmed) in &projection_of(declaring.context).autoloaded {
            for (uri, at) in confirmed
                .iter()
                .filter_map(|(uri, at)| Some((DocUri::from_uri_str(uri)?, at)))
            {
                let mut facts = Facts::default();
                facts.namespace(Owner::Module(name.clone()), *at);
                super::add(into, &uri, facts);
                declared += 1;
            }
        }
        declared
    }

    /// What `db/*schema.rb` and `db/*structure.sql` declare, and how many columns.
    fn schema_declarations(
        &self,
        declaring: &Declaring<'_>,
        retyped: &BTreeMap<String, BTreeSet<String>>,
        into: &mut Declared,
    ) -> usize {
        // Every one of them before any of them writes a line, because the ambiguity rule below
        // is about tables *across* files. Both readers end at a `rails::Schema`, so from here
        // down the ambiguity rule, the claims, the provenance line and
        // everything downstream cannot tell a dump from a `schema.rb`. Which of the two parsed
        // it, and whether the suffix on the list really named a schema at all, are
        // [`Analysis::refresh_sources`]' decisions: a document with no `schema` slot was never
        // one.
        let schemas: Vec<(DocUri, String, Arc<rails::Schema>)> = declaring
            .context
            .documents(SCHEMAS)
            .iter()
            .map(String::as_str)
            .chain(
                projection_of(declaring.context)
                    .dumps
                    .iter()
                    .map(DocUri::as_str),
            )
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
        let mut tables = self.model_tables(declaring.context);
        tables.retain(|table, _| once.get(table.as_str()) == Some(&1));

        let mut columns = 0;
        for (uri, name, schema) in &schemas {
            let facts = schema.signatures(name, &tables, retyped);
            columns += facts.len();
            super::add(into, uri, facts);
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
        self.sources_on(context, MODELS)
    }

    /// The same, for the list a gem's own concerns join.
    fn concern_sources(&self, context: &Context) -> Vec<(DocUri, String, Arc<rails::Model>)> {
        self.sources_on(context, CONCERNS)
    }

    fn sources_on(
        &self,
        context: &Context,
        list: ListId,
    ) -> Vec<(DocUri, String, Arc<rails::Model>)> {
        context
            .documents(list)
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
        declaring: &Declaring<'_>,
        models: &[(DocUri, String, Arc<rails::Model>)],
        columns: &BTreeSet<(String, String)>,
        into: &mut Declared,
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
            .flat_map(|(_, _, model)| model.collections(&declaring.context.classes))
            .map(str::to_owned)
            .chain(projection_of(declaring.context).models.iter().cloned())
            .filter(|element| declaring.context.classes.contains(element))
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
        let interface = !declaring.context.namespaces.declares(rails::RELATION_BASE);
        let relations: BTreeSet<String> = modelled
            .iter()
            .filter(|_| interface)
            .filter(|element| {
                !declaring
                    .context
                    .classes
                    .contains(&rails::relation_of(element))
            })
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
            for element in model.collections(&declaring.context.classes) {
                if relations.contains(element) && written.insert(element) {
                    assigned[at].insert(element.to_owned());
                }
            }
        }
        for element in &relations {
            if written.contains(element.as_str()) {
                continue;
            }
            if let Some(at) = declaring
                .context
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
                &declaring.context.superclasses,
                &projection_of(declaring.context).models,
                &declaring.context.namespaces,
            );
            let at = declaring
                .context
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
                    known: &declaring.context.classes,
                    framework: &projection_of(declaring.context).framework,
                    models: &modelled,
                    relations: &relations,
                    emit: &assigned[at],
                    bases: &bases[at],
                    includers: &declaring.context.includers,
                    namespaces: &declaring.context.namespaces,
                    columns,
                },
            );
            if shared == Some(at) {
                rails::relation_base(&mut facts);
            }
            members += facts.len();
            super::add(into, uri, facts);
        }
        members
    }

    /// What a `sig` block or a YARD tag says, and how many methods that typed.
    /// What every concern in the project installs on the classes that include it.
    ///
    /// Two of the three spellings, read out of the concern's own file: a `class_methods do` block
    /// and a hand-written `module ClassMethods`. The third is [`Self::concern_declarations`],
    /// which needs a second file.
    ///
    /// Keyed by the **concern's** file, because that is where the `def`s are — the span rule this
    /// pass keeps everywhere, *the source this generator read*.
    fn class_method_declarations(
        &self,
        declaring: &Declaring<'_>,
        concerns: &[(DocUri, String, Arc<rails::Model>)],
        into: &mut Declared,
    ) -> usize {
        let mut members = 0;
        for (uri, name, model) in concerns {
            let facts = model.class_methods(
                name,
                &declaring.context.includers,
                &declaring.context.namespaces,
            );
            members += facts.len();
            super::add(into, uri, facts);
        }
        members
    }

    /// The class methods an `included do … extend M` puts on every class that includes the
    /// concern, which is the one convention in this directory finished against the graph.
    ///
    /// **Why it cannot be finished where it was read.** Every other reader in `workspace/rails`
    /// answers about the file it was handed. This one reads `extend ActiveModel::Naming` in
    /// `active_model/api.rb`, and what a class gains by it is `def model_name` in
    /// `active_model/naming.rb` — a second file, named by the first, which no predicate over
    /// documents could have selected in advance. So the *name* comes out of the reader, this asks
    /// the graph which document declares it, hands that document's text back to the same reader,
    /// and declares the answer.
    ///
    /// **The generated document is keyed by the module's own file and not by the concern's**, and
    /// that is what gives the member a place: a span is an offset into the text the generator
    /// read, so writing these into `api.rb`'s document would point every jump at bytes of a file
    /// that holds no such `def`. Keyed by `naming.rb`, `Spree::TaxCategory.model_name` lands on
    /// the line Rails wrote.
    ///
    /// **It reads a gem's file, which no other generator does**, and it is safe for the reason the
    /// engine rule already gives: what is declared is a member of a class the user really names,
    /// and a generated document is not [`Analysis::is_own_code`] however it is keyed — so nothing
    /// published, renamed or searched moves for it.
    fn concern_declarations(
        &self,
        declaring: &Declaring<'_>,
        models: &[(DocUri, String, Arc<rails::Model>)],
        into: &mut Declared,
    ) -> usize {
        // `module -> the concerns whose `included do` extends it`. A module two concerns extend
        // is one row with two entries: the includer sets differ and each sentence names its own
        // concern.
        let mut wanted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (_, _, model) in models {
            for (concern, written, _) in model.extended() {
                // Nothing to write where no class includes the concern, asked before the graph is
                // searched because the search is the expensive half.
                if !declaring.context.includers.contains_key(concern) {
                    continue;
                }
                for candidate in candidates(concern, written) {
                    wanted
                        .entry(candidate)
                        .or_default()
                        .insert(concern.to_owned());
                }
            }
        }
        if wanted.is_empty() {
            return 0;
        }
        let mut members = 0;
        for (module, uri) in (declaring.declares)(&wanted.keys().cloned().collect()) {
            let name = (declaring.caption)(&uri);
            let Some(installed) =
                (declaring.text)(&uri).map(|text| rails::concern_members(&text, &module))
            else {
                continue;
            };
            let mut facts = Facts::default();
            for concern in wanted.get(&module).into_iter().flatten() {
                rails::declare_concern_members(
                    &mut facts,
                    &rails::ConcernSource {
                        file: &name,
                        concern,
                        via: Some(&module),
                    },
                    &installed,
                    &declaring.context.includers,
                    &declaring.context.namespaces,
                );
            }
            members += facts.len();
            super::add(into, &uri, facts);
        }
        members
    }

    /// What the framework's own singletons return.
    ///
    /// The one generator whose facts are a property of the **framework** rather than of the file
    /// it is keyed by, so it writes them once: `config/application.rb` is the host, and the
    /// first in URI order where a workspace somehow holds two — an arbitrary choice made
    /// deterministic, which is all it has to be, because nothing it declares is mapped to any
    /// file. `Rails.root` really is declared in railties and `Time.zone` in activesupport; what
    /// is written here is only what each hands back, so the declaration rubydex already holds
    /// keeps the place it already had.
    ///
    /// The one thing read out of the document is the `class < Rails::Application` in it, which
    /// is what `Rails.application` is an instance of. A file that declares none — and an
    /// application whose class is written somewhere this list does not reach — falls back to
    /// [`rails::APPLICATION`](crate::workspace::rails) and loses only the members the project
    /// hung off its own.
    fn framework_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        let Some(uri) = declaring
            .context
            .documents(FRAMEWORK)
            .iter()
            .filter_map(|uri| DocUri::from_uri_str(uri))
            .min_by(|left, right| left.as_str().cmp(right.as_str()))
        else {
            return 0;
        };
        let application =
            (declaring.text)(&uri).and_then(|source| rails::application_class(&source));
        let facts = rails::read_framework(application.as_deref(), &declaring.context.namespaces);
        let declared = facts.len();
        super::add(into, &uri, facts);
        declared
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
    fn entrypoint_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        let sources: Vec<(DocUri, String, Arc<rails::Entrypoints>)> = declaring
            .context
            .documents(ENTRYPOINTS)
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
        let delivery = (!declaring
            .context
            .namespaces
            .declares(rails::MESSAGE_DELIVERY))
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
            super::add(into, uri, facts);
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
    /// `def story_path:` lines as an overload set. First in URI order writes it, the same rule a
    /// shared relation class settles a collision by, and arbitrary in the same harmless way.
    fn route_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        // An application that declares the constant itself meant something by it, and a module
        // this pass wrote into would answer with both its members and ours. The rule for
        // `Comment::Relation`, and the whole feature is what it costs — which is the right price
        // for never shadowing a name somebody chose.
        if declaring.context.namespaces.declares(rails::ROUTE_HELPERS) {
            return 0;
        }
        let mains: Vec<DocUri> = declaring
            .context
            .documents(ROUTES)
            .iter()
            .filter_map(|uri| DocUri::from_uri_str(uri))
            .filter(|uri| uri.to_path().is_some_and(|path| rails::is_routes(&path)))
            .collect();
        if mains.is_empty() {
            return 0;
        }
        let mut sources: Vec<(DocUri, String, rails::Routes)> = Vec::new();
        for uri in mains {
            let Some(source) = (declaring.text)(&uri) else {
                continue;
            };
            // The receiver of a `draw` means nothing in the project's own routes file and
            // everything in a gem's — `rails::Whose` is the argument for it.
            let whose = Self::whose(declaring, &uri);
            let routes = rails::read_routes(&source, &[], whose);
            for draw in routes.draws() {
                if let Some(drawn) = Self::drawn(&uri, &draw.name)
                    && let Some(source) = (declaring.text)(&drawn)
                {
                    let name = (declaring.caption)(&drawn);
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
            let name = (declaring.caption)(&uri);
            sources.push((uri, name, routes));
        }
        // The project's own files first, and it is the first-wins assignment below that makes
        // this load-bearing rather than cosmetic: an application that names `rails_blob_path`
        // itself must be the one that declares it, and URI order between a workspace path and a
        // gem path is whichever string happens to sort lower. It also keeps the `include`s in a
        // file the user has, since they go in `sources[0]`.
        sources.sort_by(|left, right| {
            let key = |uri: &DocUri| (!(declaring.own)(uri.as_str()), uri.as_str().to_owned());
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
                facts.extend(rails::mixins(&projection_of(declaring.context).hosts));
            }
            helpers += facts.len();
            super::add(into, uri, facts);
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
        into: &mut Declared,
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
            super::add(into, uri, facts);
        }
        declared
    }

    /// The document `draw :admin` reads, which is `config/routes/admin.rb` beside the drawer.
    ///
    /// `None` when the path cannot be built or the file is not indexed, and the second is not a
    /// failure: a routes file that draws something outside `index.include` draws nothing here,
    /// which is the same answer the rest of this pass gives for a file it was told not to read.
    fn drawn(drawer: &DocUri, name: &str) -> Option<DocUri> {
        let path = drawer.to_path()?;
        DocUri::from_path(&path.parent()?.join("routes").join(format!("{name}.rb")))
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
            .documents(RENAMED)
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
        for (table, classes) in &projection_of(context).claims {
            for class in classes.iter().filter(|class| {
                !overrides.contains_key(class.as_str())
                    && (!named.contains(table.as_str()) || projection_of(context).is_model(class))
            }) {
                claimed
                    .entry(table.clone())
                    .or_default()
                    .push(class.clone());
            }
        }
        for name in &projection_of(context).nested {
            if overrides.contains_key(name.as_str()) || !projection_of(context).is_model(name) {
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
            if projection_of(context).is_model(parent) {
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
        projection_of(context).helpers.iter().cloned().collect(),
        exports,
        included,
        mailers,
    )
}

/// Every ActiveRecord model, by climbing what each class says it inherits.
///
/// The chain and not one hop: solidus writes 101 models two hops from `ActiveRecord::Base` and
/// 15 four or five, so a one-hop test would call `Spree::Order` no model at all. Each hop
/// resolves the spelling the way Rails does — [`candidates`] is `compute_type`'s own
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
            let Some((next, _)) = candidates(current, written)
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
/// Each hop resolves the spelling the way Rails does — [`candidates`] is
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
        let Some((next, _)) = candidates(current, written)
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
        && let Some(resolved) = candidates(current, written)
            .into_iter()
            .find(|candidate| namespaces.declares(candidate) && namespaces.spellable(candidate))
    {
        return resolved;
    }
    current.to_owned()
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
fn declared_members(generated: &Declared) -> BTreeSet<(String, String)> {
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

impl super::Knowledge for Rails {
    fn name(&self) -> &'static str {
        "rails"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    /// Whether this project asked for the body of knowledge one list feeds.
    ///
    /// Seven rows and five keys, because [`SCHEMAS`] and [`RENAMED`] are one family — a
    /// `schema.rb` and the macros that rename the tables in it are the same knowledge read from
    /// two places — and [`MODELS`] and [`CONCERNS`] are the same macros seen from either side.
    /// `rails.enabled` is already folded into every Rails flag by
    /// [`Features::resolve`](crate::workspace::Features::resolve), so no reader here asks twice.
    ///
    /// **[`FRAMEWORK`] is gated by the umbrella and has no key of its own**, and that is a
    /// decision rather than an omission. Every other key exists so a project may decline a body of
    /// knowledge that does not apply to it — its own schema, its own macros, its own routes. There
    /// is nothing project-specific to decline here: a workspace whose bundle holds railties cannot
    /// sensibly say that `Rails.root` is not a `Pathname`, and one whose bundle does not is
    /// already declaring nothing, because both ends of every row are checked against the graph
    /// first.
    fn wanted(&self, list: ListId, features: Features) -> bool {
        match list {
            SCHEMAS | RENAMED => features.schema,
            MODELS | CONCERNS => features.models,
            ROUTES => features.routes,
            ENTRYPOINTS => features.entrypoints,
            FRAMEWORK => features.rails,
            _ => false,
        }
    }

    /// Every name this module writes onto or beside that is **not** the application's own.
    ///
    /// The framework's own singletons and what they return; the mailer's `MessageDelivery`; the
    /// module the route helpers go in; the relation class every model gets; and
    /// `ActiveRecord::Base`, which is the class side's own base — an application with no
    /// `ApplicationRecord` of its own (discourse writes `class Post < ActiveRecord::Base` 217
    /// times) has no shared base in its own code, so whether the bundle declares that one is what
    /// decides whether this pass may write there at all.
    ///
    /// The singletons' **owners** are the half that is not obvious: `Rails` and `Time` decide
    /// whether a body may be opened for them at all and *which keyword opens it* — railties writes
    /// `module Rails`, activesupport reopens Ruby's `class Time`, and the render key is
    /// `(is_module, name)`.
    fn spellable_names(&self) -> Vec<&'static str> {
        let mut names = rails::framework_classes();
        names.extend(rails::singleton_classes());
        names.push(rails::MESSAGE_DELIVERY);
        names.push(rails::ROUTE_HELPERS);
        names.push(rails::RELATION_BASE);
        names.push(rails::RECORD_BASE);
        names
    }

    /// Five projections, and every one of them a fold over what the walk already saw.
    ///
    /// The shape that makes this cheap: `claims`, `nested`, `hosts` and `helpers` are questions
    /// about the **names a document declares** and the superclass and `include`s beside each, all
    /// of which are in [`Seen`] already. Only `autoloaded` needs anything else, and what it needs
    /// is byte offsets rather than names — [`Seen::confirming`].
    ///
    /// The order is [`Seen::declared`]'s, which is the order the document's definitions were
    /// recorded in, so what this pushes is what the one loop used to push where it used to push
    /// it.
    fn contribute(&self, seen: &Seen<'_>) -> Option<Box<dyn super::Contributes>> {
        let mut contribution = Contribution::default();
        // The suffix test is the whole reason this is not a path parse per document:
        // `_helper.rb` leaves a handful of files in the largest corpus, and [`rails::is_helper`]
        // — which is the rule, and reads the `app/helpers` anchor as well — is asked only of
        // those.
        let helper = seen.own
            && seen.uri.ends_with("_helper.rb")
            && DocUri::from_uri_str(seen.uri)
                .and_then(|uri| uri.to_path())
                .is_some_and(|path| rails::is_helper(&path));
        for (name, is_module) in seen.declared {
            if *is_module {
                if helper {
                    contribution.helpers.push(name.clone());
                }
                // A module hosts the route helpers on its own name and never through a
                // superclass, which it has none of.
                if seen.own && rails::hosts_routes(name, None, &[], true) {
                    contribution.hosts.push(Owner::Module(name.clone()));
                }
                continue;
            }
            // A table is claimed by pluralizing a **top-level** class's name.
            // `Admin::Setting`'s table depends on `table_name_prefix`, which is Ruby that only
            // runs, so it is declined here and left to `self.table_name`, which is the escape
            // that still works for it.
            //
            // `own` and not the wider width: a table is the *application's* database table, and
            // an engine that defined a top-level class would otherwise claim one by pluralizing
            // its name and compete with the model that really owns it. Two of these five mean
            // "the application" rather than "a class the reader can name", and this is the
            // first; `hosts` is the other.
            if seen.own
                && !name.contains("::")
                && let Some(table) = rails::table_of(name)
            {
                contribution.claims.push((table, name.clone()));
            }
            // The table-name half of the same sentence, and the reason it is a name and not a
            // table: `Spree::Order` reads `spree_orders` and the `spree_` comes out of a file
            // this fold is not allowed to open.
            if seen.own && name.contains("::") {
                contribution.nested.push(name.clone());
            }
            // And it stays `own`-only even though the routes list does not. A host is a class
            // the *application's* route helpers are `include`d into: an engine's own controllers
            // are hosts in Rails and reach their own helpers by their own mechanism, so
            // `ActiveStorage`'s six would each cost an `include` of a module holding nothing for
            // them.
            if seen.own {
                let superclass = seen
                    .superclasses
                    .iter()
                    .find(|(class, _)| class == name)
                    .map(|(_, superclass)| superclass.as_str());
                let mixins: Vec<String> = seen
                    .included
                    .iter()
                    .filter(|(body, _)| body == name)
                    .map(|(_, written)| written.clone())
                    .collect();
                if rails::hosts_routes(name, superclass, &mixins, false) {
                    contribution.hosts.push(Owner::Instance(name.clone()));
                }
            }
        }
        contribution.autoloaded = autoloaded(seen);
        (contribution != Contribution::default()).then(|| {
            let held: Box<dyn super::Contributes> = Box::new(contribution);
            held
        })
    }

    fn projection(&self) -> Option<Box<dyn super::Projects>> {
        Some(Box::new(Projection::default()))
    }

    /// Which of the application's classes are models, and the files that define them.
    ///
    /// **The only membership decided after the walk rather than during it, and it has to be:**
    /// every predicate in a [`Wants`] row is a question about one document, and whether a class is
    /// a model is a question about the chain above it — which is only complete when every document
    /// has been seen. A model that writes no macro at all is on no list, and it is exactly the
    /// model whose query interface had to be answered by whatever its abstract parent happened to
    /// own; its own file is where its relation class belongs, so the file joins the list that
    /// opens it. `settle` sorts the list afterwards, so appending here cannot change which
    /// document writes what.
    fn after_the_walk(&self, context: &mut Context) {
        let models = models_of(&context.superclasses);
        let joining: BTreeSet<String> = {
            let listed: std::collections::HashSet<&str> = context
                .documents(MODELS)
                .iter()
                .map(String::as_str)
                .collect();
            models
                .iter()
                .filter_map(|name| context.defined_in.get(name))
                .filter(|uri| !listed.contains(uri.as_str()))
                .cloned()
                .collect()
        };
        let found = self.found.clone();
        if let Some(projection) = context.projection_mut::<Projection>() {
            projection.models = models;
            // The dumps core found for this module, put where every reader of the projection
            // looks for them.
            projection.dumps = found;
        }
        context.documents.entry(MODELS).or_default().extend(joining);
    }

    /// What a directory conjures is only conjured where **nothing else declares the name** — a
    /// `user.rb` beside the `user/` directory, a `module Chat` in a plugin, a gem. So the filter
    /// runs here, after the application's own names and the bundle's have both been recorded.
    ///
    /// The survivors are then declared by the caller, which is what makes every prefix of a
    /// conjured name spellable: `Chat::Thread::Policy` needs `Chat::Thread`, and `Chat::Thread` is
    /// either declared already or is itself in this map, because `rails::autoloaded_namespaces`
    /// answers the whole chain rather than its last link.
    fn after_the_bundle(&self, context: &mut Context) -> Vec<String> {
        let conjured: Vec<String> = projection_of(context)
            .autoloaded
            .keys()
            .filter(|name| !context.namespaces.declares(name))
            .cloned()
            .collect();
        // And the class side's own base, plus every gem class the long-tail macros install on:
        // whether the bundle declares one is what decides whether this pass may write there.
        let framework: BTreeSet<String> = rails::framework_classes()
            .into_iter()
            .filter(|name| context.namespaces.declares(name))
            .map(str::to_owned)
            .collect();
        if let Some(projection) = context.projection_mut::<Projection>() {
            projection
                .autoloaded
                .retain(|name, _| conjured.contains(name));
            projection.framework = framework;
        }
        conjured
    }

    fn views(&self, declaring: &Declaring<'_>) -> Option<Views> {
        Some(self.view_context_if_wanted(declaring, &self.model_sources(declaring.context)))
    }

    /// Where Rails writes the query interface: the relation on the instance side, and the four
    /// class-side bases on the other.
    fn places_members_on(&self) -> [&'static [&'static str]; 2] {
        [&rails::RAILS_RELATION, &rails::RAILS_CLASS_SIDE]
    }

    fn discover(&self, reading: &Reading<'_>) -> Vec<DocUri> {
        Self::schema_dumps(reading)
    }

    fn discovered(&mut self, found: Vec<DocUri>) {
        self.found = found;
    }

    /// The namespaces a directory conjures, and nothing else.
    ///
    /// First, and it shares nothing with the generators below it: every other one is keyed by the
    /// **file** it read and this one by a **directory**, so no two of them can ever merge into one
    /// document.
    fn conjure(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        vec![(
            "conjured namespaces",
            self.autoloaded_declarations(declaring, into),
        )]
    }

    /// The seven that read a file, in the order their data dependencies force.
    ///
    /// **Four of those edges are real and the rest is habit made deterministic.** The model files
    /// are read before the schema, because an `enum` and a typed `attribute` each re-type the
    /// column they are stored in and the schema has to decline to them; the schema is read before
    /// the models, because an untyped `attribute` has to decline to a column; the concerns come
    /// after the models, because they read what a model file said about `included do … extend`.
    /// The two channels those first three edges travel on are `retyped` and `columns`, and both
    /// are this module's own — no other module sees either.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let sources = self.model_sources(declaring.context);
        let retyped = retyped_columns(&sources);
        let schemas = self.schema_declarations(declaring, &retyped, into);
        // What the schemas just said, so an untyped `attribute` of the same name can decline to
        // it. The rank says the column wins — `Source::Column` is 4 and `Source::Attribute` 7 —
        // and `Facts::declare` settles a collision *inside* one document, which these two are
        // not; so the loser declines, exactly as an `enum` and a `delegate` do.
        let columns = declared_members(into);
        let models = self.model_declarations(declaring, &sources, &columns, into);
        let entrypoints = self.entrypoint_declarations(declaring, into);
        let routes = self.route_declarations(declaring, into);
        let framework = self.framework_declarations(declaring, into);
        let concerns = self.concern_sources(declaring.context);
        let installed = self.class_method_declarations(declaring, &concerns, into);
        let extended = self.concern_declarations(declaring, &concerns, into);
        vec![
            ("columns", schemas),
            ("members", models),
            ("entry points", entrypoints),
            ("route helpers", routes),
            ("framework returns", framework),
            ("class methods a concern installs", installed),
            ("more a concern's `included do` extends", extended),
        ]
    }

    /// `delegate`, which derives from what every other generator said.
    fn derive(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let sources = self.model_sources(declaring.context);
        vec![(
            "delegated names",
            self.delegate_declarations(&sources, into),
        )]
    }

    /// Every file this module's four readers open, read and parsed once — and only where the text
    /// has moved since the last pass.
    ///
    /// The reads are per **document** and not per list: a model file that also writes a
    /// `self.table_name=` is on two lists and is parsed once, which is what [`Wanted`] is for.
    fn refresh(&mut self, sources: &Sources<'_>) {
        let context = sources.context;
        let mut wants: BTreeMap<String, Wanted> = BTreeMap::new();
        for uri in context.documents(MODELS) {
            wants.entry(uri.clone()).or_default().model = true;
        }
        // The same reader and so the same flag: a concern is read by `read_model` like any other
        // body, and a file on both lists is parsed once.
        for uri in context.documents(CONCERNS) {
            wants.entry(uri.clone()).or_default().model = true;
        }
        for uri in context.documents(RENAMED) {
            wants.entry(uri.clone()).or_default().names = true;
        }
        for uri in context.documents(ENTRYPOINTS) {
            wants.entry(uri.clone()).or_default().entrypoints = true;
        }
        // The same filter the schema generator applies, asked here so that a `schema.rb` that is
        // not one is never read: the suffix puts a document on the list and [`rails::is_schema`]
        // decides whether it really is a schema.
        for uri in context.documents(SCHEMAS) {
            if DocUri::from_uri_str(uri)
                .and_then(|uri| uri.to_path())
                .is_some_and(|path| rails::is_schema(&path))
            {
                wants.entry(uri.clone()).or_default().schema = Some(false);
            }
        }
        for uri in &projection_of(context).dumps {
            wants.entry(uri.as_str().to_owned()).or_default().schema = Some(true);
        }

        // A file that has left every list keeps nothing here. Not an optimisation: the memo is
        // keyed by URI and a file that is deleted and written again is a different file, so an
        // entry nothing asks for is an entry nothing will ever check the freshness of.
        self.sources.retain(|uri, _| wants.contains_key(uri));

        for (key, wanted) in wants {
            let Some(uri) = DocUri::from_uri_str(&key) else {
                continue;
            };
            let fresh = (sources.fresh)(&uri);
            if self
                .sources
                .get(&key)
                .is_some_and(|held| held.fresh == fresh && held.wanted == wanted)
            {
                continue;
            }
            // A file that has gone must take its entry with it rather than leave a parse nothing
            // can refresh.
            let Some(text) = (sources.text)(&uri) else {
                self.sources.remove(&key);
                continue;
            };
            self.reads += 1;
            let name = (sources.caption)(&uri);
            self.sources
                .insert(key, Source::read(fresh, wanted, name, &text));
        }
    }

    /// The two entry-point conventions, asked once and for both callers: the same
    /// [`rails::convention_of`] the reader itself asks, so which documents are worth opening and
    /// which classes are worth reading cannot disagree.
    fn claims_by_ancestry(&self, superclass: Option<&str>, mixins: &[String]) -> bool {
        rails::convention_of(superclass, mixins).is_some()
    }
}

/// What one document contributes to [`Projection`].
///
/// **Every field of it is a fold over what the one walk already saw** — the names the document
/// declares, the superclass each names, the `include`s each writes — plus the document's own URI.
/// Nothing here reads the graph or opens a file, which is why splitting the projection out cost
/// no second pass rather than the 105 ms a second pass costs on discourse.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Contribution {
    /// `(table, the top-level class whose own name implies it)`.
    pub claims: Vec<(String, String)>,
    /// The nested classes it defines.
    pub nested: Vec<String>,
    /// The route-helper hosts it defines.
    pub hosts: Vec<Owner>,
    /// The helper modules it defines, when the document is one of Rails' helper files.
    pub helpers: Vec<String>,
    /// `(the namespace a directory spells, the line in *this* document that confirmed it)` for
    /// every directory between this document's autoload root and it — and nothing at all unless
    /// the document's own constant agrees with the deepest of them.
    ///
    /// **The span is in this projection and not left to the generator, which is where every other
    /// span in this pass is read.** A namespace's generated document is keyed by the file that
    /// confirmed the directory, and that file is typically a plain controller on no generator's
    /// list — so the *sources changed* gate never sees an edit to it, and the only gate that can
    /// is the one that compares this struct against the one the document contributed last time. A
    /// span left out of here would be a mapping that goes stale the first time somebody presses
    /// return above the `class` line, which is the one failure `Synthesized::record` cannot
    /// catch: it compares the *text*, and the text is byte-identical.
    pub autoloaded: Vec<(String, Option<At>)>,
}

impl super::Contributes for Contribution {
    fn same_as(&self, other: &dyn super::Contributes) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    fn clone_box(&self) -> Box<dyn super::Contributes> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Everything Rails made of every document.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Projection {
    /// Which classes claim which table, by pluralizing a top-level class's own name.
    pub claims: BTreeMap<String, Vec<String>>,
    /// Every class the application defines under a namespace.
    pub nested: BTreeSet<String>,
    /// Every class or module the application's route helpers are `include`d into.
    pub hosts: BTreeSet<Owner>,
    /// Every helper module, out of Rails' own helper files.
    pub helpers: BTreeSet<String>,
    /// Each namespace a directory conjures, and every file that confirmed it.
    ///
    /// The *map* is the point: a namespace two directories spell — `app/models/user/` and
    /// `app/services/user/` — is one namespace with files under both, and which of them a reader
    /// is offered first may not depend on the order the graph hands the documents over in. Sorted
    /// rather than compared, so there is no order to get wrong.
    pub autoloaded: BTreeMap<String, BTreeMap<String, Option<At>>>,
    /// Which of the application's classes are ActiveRecord models.
    ///
    /// **The one membership decidable only after the walk**, which is why it is not a
    /// contribution: a model is a class whose superclass chain reaches `ActiveRecord::Base`, and
    /// the chain is spread across files. Filled once the merge is complete.
    pub models: BTreeSet<String>,
    /// The gem classes the long-tail macros install members on, which the bundle has to declare
    /// before this pass may write there.
    pub framework: BTreeSet<String>,
    /// `db/*structure.sql`, which is the one input that is not a graph document at all.
    ///
    /// A `read_dir` has no defined order, and which of two schema sources is read first decides
    /// nothing here only because the ambiguity rule runs first — but a list that varies per run is
    /// a difference waiting to matter, so [`Projects::settle`](super::Projects::settle) sorts it.
    pub dumps: Vec<DocUri>,
}

impl Projection {
    /// An empty one, for a build this module is not registered in.
    ///
    /// `const` so that a reader can fall back to a `static` rather than to an `Option` at every
    /// call site: every generator in the pass reads several of these fields, and *Rails is not
    /// registered* has to read as *there are no models* rather than as a branch per read.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            claims: BTreeMap::new(),
            nested: BTreeSet::new(),
            hosts: BTreeSet::new(),
            helpers: BTreeSet::new(),
            autoloaded: BTreeMap::new(),
            models: BTreeSet::new(),
            framework: BTreeSet::new(),
            dumps: Vec::new(),
        }
    }
}

impl Projection {
    /// Whether `name` is an ActiveRecord model.
    ///
    /// The gate on a new claimant, and it is load-bearing rather than tidy: `claims` is every
    /// top-level class the application defines and not only its models, which costs nothing while
    /// a table has one claimant. Once a nested class may claim one it costs a great deal — the six
    /// corpora hold **75** nested non-model classes whose name inflects onto a real table under a
    /// different last segment, and every one of them would take that table away from the model
    /// that reads it. A gate on the superclass chain declines all 75 and keeps every legitimate
    /// claimant.
    ///
    /// A lookup rather than a walk, because it is asked of every class the application defines
    /// rather than of the nested few: the chain is walked once, after the merge.
    #[must_use]
    pub fn is_model(&self, name: &str) -> bool {
        self.models.contains(name)
    }
}

impl super::Projects for Projection {
    fn absorb(&mut self, uri: &str, contribution: &dyn super::Contributes) {
        let Some(contribution) = contribution.as_any().downcast_ref::<Contribution>() else {
            return;
        };
        for (table, name) in &contribution.claims {
            self.claims
                .entry(table.clone())
                .or_default()
                .push(name.clone());
        }
        self.nested.extend(contribution.nested.iter().cloned());
        self.hosts.extend(contribution.hosts.iter().cloned());
        self.helpers.extend(contribution.helpers.iter().cloned());
        for (name, at) in &contribution.autoloaded {
            self.autoloaded
                .entry(name.clone())
                .or_default()
                .insert(uri.to_owned(), *at);
        }
    }

    /// `claims` is pushed once per definition in the graph's iteration order, which is a
    /// `HashMap`'s and therefore nobody's. It changes no answer — `model_tables` sorts and dedups
    /// its own copy before it reads one — and what it does change is that two walks over one
    /// workspace produce the same projection, which is what the wide gate compares and what the
    /// per-document contributions stand on.
    fn settle(&mut self) {
        for classes in self.claims.values_mut() {
            classes.sort_unstable();
        }
        self.dumps.sort_unstable();
    }

    /// What a directory conjures is the one thing here that is on no list: a namespace is a
    /// projection of the **paths** the walk visited rather than of any document a generator opens,
    /// so a workspace whose only Rails fact is a conjured namespace still has something to
    /// declare.
    fn declares_nothing(&self) -> bool {
        self.autoloaded.is_empty() && self.dumps.is_empty()
    }

    fn also_reads(&self) -> &[DocUri] {
        &self.dumps
    }

    fn same_as(&self, other: &dyn super::Projects) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// What this module made of the whole walk, or an empty projection.
///
/// A module reading its own projection back out of a [`Context`] is the ordinary case and the
/// downcast is its own: core holds `dyn Projects` and names nothing.
#[must_use]
pub fn projection_of(context: &super::Context) -> &Projection {
    static NONE: Projection = Projection::empty();
    context.projection().unwrap_or(&NONE)
}

/// Zeitwerk's implicit namespaces, and **the file has to agree with the directory**.
///
/// `app/services/user/policy/not_already_silenced.rb` conjures `User::Policy` because the constant
/// it writes is `User::Policy::NotAlreadySilenced` — the directory's name plus exactly one
/// segment, which is Zeitwerk's own contract. A file that writes something else is a file Rails
/// would not have loaded from there, and a directory whose only files are like that has conjured
/// nothing anybody can reach.
///
/// Asked of `own` code only: a gem's `app/` is loaded by the *gem's* own autoloader, whose roots
/// are not these.
///
/// **The declared name is asked first and the path second**, for the reason the helper test above
/// is a suffix test: parsing a path builds a vector per document and this runs before every
/// resolve, so the expensive half may only be reached by the rare one. A file declaring no `::`
/// name cannot agree with any directory whatever its path says, and that is most of the files in
/// an application.
fn autoloaded(seen: &Seen<'_>) -> Vec<(String, Option<At>)> {
    if !seen.own || !seen.declared.iter().any(|(name, _)| name.contains("::")) {
        return Vec::new();
    }
    let Some(path) = DocUri::from_uri_str(seen.uri).and_then(|uri| uri.to_path()) else {
        return Vec::new();
    };
    let proposed = rails::autoloaded_namespaces(&path);
    // The chain the *file* spells, which is the same chain whenever the project's inflector is
    // Zeitwerk's own and the right one when it is not. Asked of every constant the document
    // declares and not only of the first: one file may write `class Api::V1::Foo` under a
    // `spec`-shaped sibling class, and the one that agrees is the one that confirms the directory.
    let Some((conjured, declared)) = seen.declared.iter().find_map(|(name, _)| {
        let (parent, _) = name.rsplit_once("::")?;
        Some((rails::confirmed_spelling(&proposed, parent)?, parent))
    }) else {
        return Vec::new();
    };
    let names: BTreeSet<&str> = conjured.iter().map(|(_, name)| name.as_str()).collect();
    let confirmed = (seen.confirming)(declared, &names);
    conjured
        .iter()
        .map(|(_, name)| (name.clone(), confirmed.get(name).copied()))
        .collect()
}
