//! Rails, as a body of knowledge the pass drives. The pass is not written around it.
//!
//! **This file orchestrates; it does not read.** Every line that turns text into facts lives in
//! [`workspace::rails`](crate::workspace::rails): pure text in, text out, no I/O, no graph, 100%
//! coverage. This file decides:
//!
//! - which documents each reader is handed
//! - what a project must have said for a list to be worth filling
//! - which conventions make a class one of Rails'
//!
//! It may not open a file or read the graph either. It gets what the one walk already saw, in a
//! [`Seen`](super::Seen), and folds it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::{Context, Counted, Declared, Declaring, Fresh, ListId, Reading, Seen, Sources, Wants};
use crate::analysis::views::Views;
use crate::generated::{At, Facts, Owner, candidates};
use crate::workspace::{DocUri, Features, rails};

/// Documents whose name ends `schema.rb`. The suffix is the cheap half; [`rails::is_schema`]
/// decides whether one really is a schema.
pub const SCHEMAS: ListId = ListId("rails.schemas");
/// Documents that say something about a table's **name**, not its columns: `self.table_name=` (the
/// escape from every naming convention), and the two ways a namespace declares a prefix for every
/// table under it.
///
/// One list, not two, because [`rails::read_table_names`] is one walk. Two lists would parse a file
/// that says both twice.
pub const RENAMED: ListId = ListId("rails.renamed");
/// Documents that call one of [`rails::MODEL_CALLS`].
pub const MODELS: ListId = ListId("rails.models");
/// Documents that install a class method on every class that includes a module: a
/// `class_methods do`, a hand-written `module ClassMethods`, or an `included do` holding a bare
/// `extend`.
///
/// **The one list a gem's `lib/` may join**, which is why it is not a flag on [`MODELS`]. The edge
/// it reads is written almost entirely in gems: `validates`, `scope`, `belongs_to` and `has_many`
/// all live in a Rails concern's `ClassMethods`, and `ActiveModel::API`'s two `extend` lines
/// install `model_name` on every model. Opening [`MODELS`] to gems instead would declare a gem's
/// own `has_many` and `enum` onto its own classes, which is a different decision.
pub const CONCERNS: ListId = ListId("rails.concerns");
/// Documents defining a class [`rails::convention_of`] recognises: a mailer, a job or a Sidekiq
/// worker. The only list filled by what a class *inherits*, because these conventions have no macro
/// to look for.
pub const ENTRYPOINTS: ListId = ListId("rails.entrypoints");
/// Documents named `routes.rb`. [`rails::is_routes`] decides whether one really is an application's
/// routes, as [`rails::is_schema`] does for schemas. Files a routes file *draws* are not on this
/// list: only the drawing file says which they are.
pub const ROUTES: ListId = ListId("rails.routes");
/// The one document named `config/application.rb`, read for the one thing only it says: the
/// `class < Rails::Application` that `Rails.application` is an instance of.
///
/// **It hosts nothing.** It once hosted every framework row, which made the rows depend on the
/// project being an application: an engine or a gem monorepo got none. Each row is now hosted on the
/// file declaring the class or module it is on, the component's own (see
/// `Rails::framework_declarations`).
///
/// A full path, not a suffix: `app/models/application.rb` is a common model file.
pub const FRAMEWORK: ListId = ListId("rails.framework");
/// Documents that move Rails' time-zone default ([`rails::TIME_ZONE_SETTINGS`]).
///
/// **Membership is the whole fact**, so nothing on it is read: while the list is empty, a
/// `datetime` column is the `ActiveSupport::TimeWithZone` Rails' railtie makes it, and once a file
/// writes one of the settings, which class it holds is a value this pass does not follow.
pub const ZONES: ListId = ListId("rails.zones");

/// Documents that name an engine: `engine_name` and `isolate_namespace`, which say the helper Rails
/// defines where it is mounted (`spree.admin_orders_path`). Read by the routes generator, which
/// declares each on the route helpers; nothing is declared from this list alone.
pub const ENGINES: ListId = ListId("rails.engines");

/// Documents that may register a connection adapter: a gem's `*adapter.rb` files, where every
/// adapter gem read writes its `ActiveRecord::ConnectionAdapters.register` and its class
/// (`activerecord-postgis-adapter.rb`, `sqlserver_adapter.rb`). Read only, for
/// [`rails::Resolver`].
pub const ADAPTERS: ListId = ListId("rails.adapters");

/// Documents that call `config` or `configuration`, where a project assigns its own settings
/// (`Rails.configuration.dispatcher = …`): [`rails::read_config_writes`].
pub const CONFIGURED: ListId = ListId("rails.configured");

/// The eleven lists, one row each.
static WANTS: [Wants; 11] = [
    Wants {
        list: CONFIGURED,
        calls: &["config", "configuration"],
        constants: &[],
        modules: &[],
        defines: &[],
        path: None,
        spells: &[],
        tags: false,
        inherits: false,
        // an engine's `initializer` writes `app.config.<name>` for the application it is loaded into
        engines: true,
        gems: false,
        reads_only: false,
        buffers: false,
    },
    Wants {
        list: ADAPTERS,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("adapter.rb"),
        spells: &[],
        tags: false,
        inherits: false,
        // an adapter is a gem's, or the application's own; an engine names none
        engines: false,
        gems: true,
        // what it registers decides which class a connection is, declared elsewhere
        reads_only: true,
        buffers: false,
    },
    Wants {
        list: SCHEMAS,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("schema.rb"),
        spells: &[],
        tags: false,
        inherits: false,
        // an engine ships migrations, never a `schema.rb`, and gate 1 does not walk a gem's `db/`
        engines: false,
        gems: false,
        reads_only: false,
        buffers: false,
    },
    Wants {
        list: RENAMED,
        calls: &["table_name=", "isolate_namespace"],
        constants: &[],
        modules: &[],
        defines: &["table_name_prefix", "table_name_suffix"],
        path: None,
        spells: &[],
        tags: false,
        inherits: false,
        // the input to a generator that is itself closed
        engines: false,
        gems: false,
        // and it declares nothing on its own: it renames a table for a schema that has to exist
        // somewhere else
        reads_only: true,
        buffers: false,
    },
    Wants {
        list: MODELS,
        calls: &rails::MODEL_CALLS,
        constants: &[],
        modules: &[],
        defines: &[],
        path: None,
        spells: &[],
        tags: false,
        inherits: false,
        // `has_many` on `ActiveStorage::Blob` is a member of a class the user names
        engines: true,
        gems: false,
        reads_only: false,
        buffers: false,
    },
    Wants {
        list: CONCERNS,
        calls: &["class_methods", "included"],
        constants: &[],
        modules: &[rails::CLASS_METHODS],
        defines: &[],
        path: None,
        spells: &[],
        tags: false,
        inherits: false,
        // a concern's class methods are members of whatever includes it, which is a class the
        // reader names in their own file
        engines: true,
        // and the same sentence one directory further out: `ActiveModel::Validations`'
        // `ClassMethods` holds `validates`, which every model in the project calls
        gems: true,
        reads_only: false,
        buffers: false,
    },
    Wants {
        list: ENTRYPOINTS,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: None,
        spells: &[],
        tags: false,
        inherits: true,
        // `ActiveStorage::AnalyzeJob` really does get `perform_later`
        engines: true,
        gems: false,
        reads_only: false,
        buffers: false,
    },
    Wants {
        list: ROUTES,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("routes.rb"),
        spells: &[],
        tags: false,
        inherits: false,
        // an engine's `config/routes.rb` may name the *host application's* helpers, and
        // `rails::Whose` is what decides whether this one does
        engines: true,
        gems: false,
        reads_only: false,
        buffers: false,
    },
    Wants {
        list: FRAMEWORK,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("config/application.rb"),
        spells: &[],
        tags: false,
        inherits: false,
        // an engine has no `config/application.rb`, and the application it is loaded into does:
        // these are the framework's singletons, declared once wherever the application is
        engines: false,
        gems: false,
        reads_only: false,
        buffers: false,
    },
    Wants {
        list: ZONES,
        calls: &rails::TIME_ZONE_SETTINGS,
        constants: &[],
        modules: &[],
        defines: &[],
        path: None,
        spells: &[],
        tags: false,
        inherits: false,
        // the application's configuration and its own models; an engine or a gem that turned the
        // default off would be turning it off for an application it does not own
        engines: false,
        gems: false,
        // and it declares nothing: it decides what the schema's columns are
        reads_only: true,
        buffers: false,
    },
    Wants {
        list: ENGINES,
        calls: &["engine_name", "isolate_namespace"],
        constants: &[],
        modules: &[],
        defines: &[],
        path: None,
        spells: &[],
        tags: false,
        inherits: false,
        // the project's own engines, which its routes files draw for; a gem's is mounted, and so
        // named, only by an application's `mount`, which is not read
        engines: false,
        gems: false,
        // the routes generator declares from it, and only where there are routes
        reads_only: true,
        buffers: false,
    },
];

/// Which readers one document's place on the lists calls for.
///
/// A document is usually on one list and may be on five. Reads are per *document*, not per list, so
/// a model file that also has a `self.table_name=` is read once.
///
/// Compared as a whole: a document that joined a list since the last pass is read again, not topped
/// up. **A document can join a list without its own text changing.** The walk puts every file that
/// *defines* a class another file's macro names onto [`MODELS`], so writing `has_many :widgets` in
/// one file adds `widget.rb`. If `widget.rb` was already read for a `self.table_name=`, its entry
/// would look fresh and hold the wrong things.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    model: bool,
    /// Whether the schema reader is wanted, and whether the file is a dump rather than a
    /// `schema.rb`. That picks which of the two readers runs, and it is a property of the path.
    schema: Option<bool>,
    names: bool,
    entrypoints: bool,
    /// A gem's `*adapter.rb`, for the connection adapters it registers.
    adapters: bool,
    /// The application's `config/database.yml`.
    database: bool,
    /// A file that may assign a setting of the project's own.
    configured: bool,
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
    pub registered: Option<Arc<rails::Registered>>,
    pub database: Option<Arc<rails::DatabaseConfig>>,
    /// The settings it assigns ([`rails::read_config_writes`]).
    pub configured: Option<Arc<Vec<(String, At)>>>,
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
            registered: wanted
                .adapters
                .then(|| Arc::new(rails::read_registered(source))),
            database: wanted
                .database
                .then(|| Arc::new(rails::read_database_config(source))),
            configured: wanted
                .configured
                .then(|| Arc::new(rails::read_config_writes(source))),
        }
    }
}

/// Rails.
#[derive(Debug, Default)]
pub struct Rails {
    /// Every file this module's readers have parsed, by URI.
    ///
    /// **Owned by this module, not the pass**, so the readers keep their Rails syntax types: core
    /// holds a `dyn Knowledge` and learns nothing about a `rails::Model`.
    sources: HashMap<String, Source>,
    /// How many files it has read, for the one line that says the pass ran.
    pub reads: u64,
    /// The `db/*structure.sql` files core last found for this module.
    ///
    /// Kept here as well as on the projection, because the pass gate asks for them before the
    /// projection exists: a dump that appeared or vanished is a change nothing else would notice.
    found: Vec<DocUri>,
}

impl Rails {
    /// Every `config.<name> =` a file writes, where the bundle declares the class that keeps it.
    fn setting_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        if !declaring
            .context
            .namespaces
            .declares(rails::RAILTIE_CONFIGURATION)
        {
            return 0;
        }
        let mut settings = 0;
        for uri in declaring.context.documents(CONFIGURED) {
            let (Some(source), Some(document)) =
                (self.sources.get(uri), DocUri::from_graph_uri(uri))
            else {
                continue;
            };
            let Some(writes) = source.configured.as_deref() else {
                continue;
            };
            settings += writes.len();
            super::add(into, &document, rails::config_facts(writes, &source.name));
        }
        settings
    }

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

    /// The view context, or an empty one where the project turned views off.
    ///
    /// **Empty, not absent**, because `Views` is a field, not an option. `Views::default()` is
    /// switched off, so `reachable` declines before reading a path; that half of `rails.views` is
    /// not in the pass at all. A Sinatra application with an `app/views/` is who this is for.
    fn view_context_if_wanted(
        &self,
        declaring: &Declaring<'_>,
        models: &[(DocUri, String, Arc<rails::Model>)],
    ) -> Views {
        let views = if declaring.features.views {
            view_context(
                declaring.context,
                models,
                self.template_paths(declaring.context),
            )
        } else {
            Views::default()
        };
        let (callbacks, loose) = controller_callbacks(models);
        views.with_callbacks(callbacks, loose)
    }

    /// Every mailer that wrote `default template_path:`, and where to, from the entry-point reader,
    /// which opens every mailer by its superclass. A class two files reopen keeps the later file's.
    fn template_paths(&self, context: &Context) -> BTreeMap<String, rails::TemplatePath> {
        let mut moved = BTreeMap::new();
        for key in context.documents(ENTRYPOINTS) {
            let Some(entrypoints) = self
                .sources
                .get(key)
                .and_then(|held| held.entrypoints.as_ref())
            else {
                continue;
            };
            for (mailer, setting) in entrypoints.template_paths() {
                moved.insert(mailer.to_owned(), setting.clone());
            }
        }
        moved
    }

    /// The files this module reads that are not graph documents: every `db/*structure.sql`, and
    /// the application's `config/database.yml`.
    fn unindexed(reading: &Reading<'_>) -> Vec<DocUri> {
        let dumps = std::fs::read_dir(reading.root.join("db"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| reading.features.schema && rails::is_structure(path));
        let database = Some(reading.root.join("config").join("database.yml"))
            .filter(|path| reading.features.rails && path.is_file());
        dumps
            .chain(database)
            .filter(|path| (reading.admits)(path))
            .filter_map(|path| DocUri::from_path(&path))
            .collect()
    }

    /// The namespaces a **directory** declares, as empty module bodies.
    ///
    /// The one generator that opens no file: its whole input is [`Projection::autoloaded`], a
    /// projection of the paths the walk visited.
    ///
    /// **It declares a name and nothing else**, which is all Zeitwerk does. rubydex already holds
    /// every class under the directory; what was missing is the constant they hang off.
    ///
    /// **One declaration per confirming file, which gives the name its places.** A directory is not
    /// a line and cannot be jumped to, so keying by directory would show a `module Mod` card above
    /// a jump that goes nowhere, and `hover` and `definition` must never disagree. The file that
    /// writes `class Mod::FlaggedController` *is* a line, it is what made this generator believe
    /// the directory, and Ruby agrees it declares the constant. So each confirming file writes the
    /// body into its own generated document, and rubydex merges them into one declaration with one
    /// definition per file. `definitions_of` answers with those.
    ///
    /// When a name leaves `autoloaded`, `Analysis::forget_stale` drops the documents that declared
    /// it.
    ///
    /// Every name here is spellable by construction: `Analysis::walk` declares the whole surviving
    /// chain into [`Context::namespaces`] before this runs, so there is nothing to decline.
    fn autoloaded_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        let mut declared = 0;
        for (name, confirmed) in &projection_of(declaring.context).autoloaded {
            for (uri, at) in confirmed
                .iter()
                .filter_map(|(uri, at)| Some((DocUri::from_graph_uri(uri)?, at)))
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
        defined: &BTreeMap<String, BTreeSet<String>>,
        (recast, picked): (&Recast, &mut rails::Picked),
        into: &mut Declared,
    ) -> usize {
        // Every schema before any writes a line, because the ambiguity rule below is about tables
        // *across* files. Both readers end at a `rails::Schema`, so from here on nothing can tell a
        // dump from a `schema.rb`. Which reader parsed it, and whether the suffix really named a
        // schema, are `Analysis::refresh_sources`' decisions: a document with no `schema` slot was
        // never one.
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
                    DocUri::from_graph_uri(key)?,
                    held.name.clone(),
                    held.schema.clone()?,
                ))
            })
            .collect();
        if schemas.is_empty() {
            return 0;
        }

        // A table two schemas declare is declared by neither, like a table two classes claim.
        // Otherwise the model would answer with two schemas at once, and, worse because silent,
        // `Types::harvest` would keep whichever column was read last: a type that depends on
        // document order.
        let mut once: HashMap<&str, usize> = HashMap::new();
        for (_, _, schema) in &schemas {
            for table in schema.table_names() {
                *once.entry(table).or_default() += 1;
            }
        }
        let mut tables = self.model_tables(declaring.context);
        tables.retain(|table, _| once.get(table.as_str()) == Some(&1));

        let zoned = declaring.context.documents(ZONES).is_empty();
        let mut columns = 0;
        for (uri, name, schema) in &schemas {
            let mut facts = schema.signatures(name, &tables, retyped, zoned);
            schema.attribute_methods(name, &tables, retyped, defined, zoned, &mut facts);
            // What `pick` hands back per column, for the relation classes the models write.
            schema.picked(&tables, &recast.on, &recast.anywhere, zoned, picked);
            columns += facts.len();
            super::add(into, uri, facts);
        }
        columns
    }

    /// Which adapter class a connection is: the registrations on [`ADAPTERS`], the drivers the
    /// bundle declares, the application's `config/database.yml`, and whether this is an
    /// application at all (its `config/application.rb`), since an engine is connected by its host.
    fn resolver(&self, declaring: &Declaring<'_>) -> rails::Resolver {
        let context = declaring.context;
        let registered: Vec<Arc<rails::Registered>> = context
            .documents(ADAPTERS)
            .iter()
            .filter_map(|key| self.sources.get(key)?.registered.clone())
            .collect();
        let config = projection_of(context)
            .dumps
            .iter()
            .find_map(|uri| self.sources.get(uri.as_str())?.database.clone());
        rails::Resolver::new(
            &registered.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            &|driver| context.namespaces.declares(driver),
            config.map(|config| (*config).clone()),
            !context.documents(FRAMEWORK).is_empty(),
        )
    }

    /// Every file on the model list, read and parsed once.
    ///
    /// Separate from the declaring below because two generators need it: the schema needs
    /// [`retyped_columns`] before it writes a line, and the models need every file parsed before
    /// any writes, because which element types need a relation class is a fact across files.
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
                    DocUri::from_graph_uri(key)?,
                    held.name.clone(),
                    held.model.clone()?,
                ))
            })
            .collect()
    }

    /// What the association macros and `enum` declare.
    ///
    /// **Exactly one** file writes each relation class: the first in URI order that asks for it.
    /// Arbitrary but deterministic, which is enough, because a relation class maps to no line of
    /// code and is the same class wherever it lives.
    fn model_declarations(
        &self,
        declaring: &Declaring<'_>,
        models: &[(DocUri, String, Arc<rails::Model>)],
        (columns, picked): (&BTreeSet<(String, String)>, &rails::Picked),
        resolver: &rails::Resolver,
        into: &mut Declared,
    ) -> usize {
        // A class gets a relation class for either of two reasons:
        //
        // - **a macro made it a collection**, or
        // - **it is a model**: ActiveRecord answers `where` and `first` on it, macro or not.
        //
        // Neither implies the other, so this is a union. [`models_of`] explains the classes that
        // are the first and not the second.
        //
        // Both are filtered the same way: the application must define the class itself, and the
        // relation's name must not be taken already. A project that wrote its own
        // `Comment::Relation` meant something by it, and shadowing it is the one way this pass
        // makes an answer worse instead of absent.
        //
        // The host test asks the same union one step earlier: is the class a macro is written
        // **on** a model? That must stay one set (a serializer is not a model because nothing says
        // it is), so `relations` is derived from this, not built beside it.
        let modelled: BTreeSet<String> = models
            .iter()
            .flat_map(|(_, _, model)| model.collections(&declaring.context.classes))
            .map(str::to_owned)
            .chain(projection_of(declaring.context).models.iter().cloned())
            .filter(|element| declaring.context.classes.contains(element))
            .collect();
        // The element half is deliberately **not** gated by the host test. A serializer's
        // `has_many :statuses` is still evidence that `Status` is a collection, and gating the
        // input of the set the gate reads would be circular. The host test removes the declaration,
        // never the relation.
        //
        // **A project that declares [`rails::RELATION_BASE`] itself meant something by it.** A
        // class this pass wrote into would answer with both its members and ours, and every
        // relation would inherit whatever they meant. Same rule as `Comment::Relation` and
        // `ROUTE_HELPERS`. It withdraws all the **relations**: with no relation class there is
        // nothing for `has_many` to return, so the whole half declines together instead of leaving
        // `-> Comment::Relation` naming an undeclared class.
        let interface = !declaring.context.namespaces.declares(rails::RELATION_BASE);
        let relations: BTreeSet<String> = modelled
            .iter()
            .filter(|_| interface)
            .filter(|element| {
                !declaring
                    .context
                    .classes
                    .contains(&crate::generated::collection_of(element))
            })
            // A name **rubydex invented** is not a constant path, and a generated declaration on
            // one costs the whole document (see `generated::is_constant_path`). An anonymous
            // `Class.new(Spree::Base)` in a spec is a model by every rule here, so this filter is
            // needed.
            //
            // The name, **not** `Namespaces::spellable`, which is a rule about *namespaces*. A
            // relation class joins its element's namespace, and holding it to that rule would
            // withdraw every relation whose element sits under a Zeitwerk-conjured module, leaving
            // those positions with no answer.
            .filter(|element| crate::generated::is_constant_path(element))
            .cloned()
            .collect();

        // Which document writes each relation class: **the first that asks for it**. Only for a
        // model no macro asks about does the document that defines the class write it.
        //
        // **The order of these two loops matters, and was measured.** Preferring the defining
        // document for *every* element moves relation classes that already had a home, and that
        // loses answers: `Channel::Telegram.find_by` stopped resolving to its own class side and
        // fell to `ApplicationRecord`'s. Nothing in a relation class is mapped, so its home should
        // not matter, yet it does. **Until that is understood, nothing that already has a home may
        // move.**
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

        // The query interface goes in **one** document, the first that writes any relation, for the
        // `MessageDelivery` stub's reason: it is one class however many relations inherit it, and N
        // copies would be N declarations of the same hundred-odd members. A workspace with no
        // relation writes no base class.
        let shared = assigned.iter().position(|elements| !elements.is_empty());

        // The class side's own base. `Story.where` is *inherited* (Ruby follows a class's singleton
        // chain up the class chain), so the interface goes once on each model's **base**, and a
        // project pays per base, not per model. The document that defines the base writes it, as an
        // unasked-for relation class goes where its element is defined. A base whose file is not on
        // this list falls in with the shared half.
        //
        // Built from `relations`, not `modelled`, so every class that has a class side keeps one
        // through its base. It also covers a model with no `has_many` and no `scope`, which is on
        // no list; that follows from putting the declaration where Rails does, not from a second
        // rule.
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

        let current = inheriting(&declaring.context.superclasses, |written| {
            written.trim_start_matches("::") == rails::CURRENT_ATTRIBUTES
        });
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
                    zoned: declaring.context.documents(ZONES).is_empty(),
                    picked,
                    current: &current,
                },
            );
            // Each class's own `connects_to` or `establish_connection`, on its class object.
            for (class, connection) in model.connections() {
                if let Some(adapter) = resolver.connection(connection) {
                    rails::connection_rows(
                        &mut facts,
                        (name, class),
                        &adapter,
                        connection,
                        &declaring.context.namespaces,
                    );
                }
            }
            if shared == Some(at) {
                rails::relation_base(&mut facts, &projection_of(declaring.context).framework);
            }
            members += facts.len();
            super::add(into, uri, facts);
        }
        members
    }

    /// What every concern in the project installs on the classes that include it.
    ///
    /// Two of the three spellings, read from the concern's own file: a `class_methods do` block and
    /// a hand-written `module ClassMethods`. The third is [`Self::concern_declarations`], which
    /// needs a second file.
    ///
    /// Keyed by the **concern's** file, because that is where the `def`s are: the span rule this
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

    /// The class methods an `included do … extend M` puts on every class that includes the concern.
    /// The one convention here that is finished against the graph.
    ///
    /// **Why it cannot finish where it was read.** Every other reader answers about the file it was
    /// handed. This one reads `extend ActiveModel::Naming` in `active_model/api.rb`, but what a
    /// class gains is `def model_name` in `active_model/naming.rb`: a second file, named by the
    /// first, which no document predicate could pick in advance. So the reader yields the *name*,
    /// this asks the graph which document declares it, hands that text back to the same reader, and
    /// declares the answer.
    ///
    /// **Keyed by the module's own file, not the concern's**, which gives the member a place. A
    /// span is an offset into the text the generator read, so keying by `api.rb` would point every
    /// jump at bytes with no such `def`. Keyed by `naming.rb`, `Spree::TaxCategory.model_name`
    /// lands on the line Rails wrote.
    ///
    /// **It reads a gem's file, which no other generator does.** Safe for the engine rule's reason:
    /// the member belongs to a class the user really names, and a generated document is never the
    /// user's own code (`Analysis::is_own_code`) however it is keyed, so nothing published, renamed
    /// or searched changes.
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

    /// What the framework's own singletons return, and what a controller and a template call on
    /// themselves.
    ///
    /// **Each owner's rows are hosted on the file that declares that owner**, the component's own:
    /// railties' `module Rails`, actionpack's `ActionController::Metal`. So they are written
    /// wherever that component is indexed, an application, an engine or a gem monorepo alike, and
    /// never where it is not. Nothing they declare is mapped to a file, so the host only names the
    /// generated document.
    ///
    /// `config/application.rb` is still read, for the one thing only it says: the
    /// `class < Rails::Application` that `Rails.application` is an instance of. Without one it
    /// falls back to [`rails::APPLICATION`](crate::workspace::rails) and loses only the members the
    /// project hung off its own class.
    ///
    /// **A migration's rows ride on the same graph question**
    /// ([`rails::read_migrations`](crate::workspace::rails)): the statement and column modules are
    /// asked for beside the owners, since each question walks every definition in the graph. Those
    /// rows are placed in the files they were read from, so the file declaring the module is also
    /// the one it is read out of. The second count is theirs.
    fn framework_declarations(
        &self,
        declaring: &Declaring<'_>,
        connection: Option<&str>,
        into: &mut Declared,
    ) -> (usize, usize) {
        // The umbrella, asked here because nothing else gates this generator: the host list it
        // once needed is only read for the application's class now. [`Self::wanted`] says why the
        // umbrella and no key of its own.
        if !declaring.features.rails {
            return (0, 0);
        }
        let application = declaring
            .context
            .documents(FRAMEWORK)
            .iter()
            .filter_map(|uri| DocUri::from_graph_uri(uri))
            .min_by(|left, right| left.as_str().cmp(right.as_str()))
            .and_then(|uri| (declaring.text)(&uri))
            .and_then(|source| rails::application_class(&source));
        // Every helper module the application writes, which a controller's `helpers` holds.
        let helpers: Vec<String> = projection_of(declaring.context)
            .helpers
            .iter()
            .cloned()
            .collect();
        let by_owner = rails::read_framework(
            application.as_deref(),
            &helpers,
            connection,
            &declaring.context.namespaces,
        );
        let sources = rails::migration_sources();
        let hosts = (declaring.declares)(
            &by_owner
                .keys()
                .chain(&sources)
                .map(|owner| (*owner).to_owned())
                .collect(),
        );
        let mut declared = 0;
        for (owner, facts) in by_owner {
            // Declared by the bundle, or the row would not be here, so a host is found; a name the
            // pass cannot place in a file is left unsaid rather than put somewhere else.
            if let Some(uri) = hosts.get(owner) {
                declared += facts.len();
                super::add(into, uri, facts);
            }
        }
        let mut forwarded = 0;
        for module in sources {
            let Some((uri, text)) = hosts
                .get(module)
                .and_then(|uri| Some((uri, (declaring.text)(uri)?)))
            else {
                continue;
            };
            let facts = rails::read_migrations(
                &text,
                module,
                &(declaring.caption)(uri),
                &declaring.context.namespaces,
            );
            forwarded += facts.len();
            super::add(into, uri, facts);
        }
        (declared, forwarded)
    }

    /// What a mailer's actions and a job's `perform` install on the class side.
    ///
    /// **Exactly one file writes the [`rails::MESSAGE_DELIVERY`] stub**, for the relation-class
    /// reason: it is one type however many mailers reach it. The first mailer in URI order writes
    /// it; arbitrary but deterministic, and nothing in that class is mapped.
    ///
    /// An application that declares `ActionMailer::MessageDelivery` itself gets nothing from this:
    /// it meant something by that name. Whether the *gem* declares it cannot be asked (declarations
    /// do not exist until `resolve`) and need not be: both become one declaration, and this one
    /// carries no place.
    fn entrypoint_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        let sources: Vec<(DocUri, String, Arc<rails::Entrypoints>)> = declaring
            .context
            .documents(ENTRYPOINTS)
            .iter()
            .filter_map(|key| {
                let held = self.sources.get(key)?;
                Some((
                    DocUri::from_graph_uri(key)?,
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
    /// **Two reads, and the second needs the first.** Only `draw :admin` says where
    /// `config/routes/admin.rb` sits in the scope, so a drawn file is read after its drawer, *at
    /// that prefix*. Its declarations go into its **own** generated document: a span is a byte
    /// range with no URI, and a mapping recorded against the wrong file opens the wrong line
    /// confidently.
    ///
    /// **Exactly one file declares each helper.** Two routes files naming one (an engine and the
    /// application, or `config/routes.rb` and a drawn file) would land in two generated documents,
    /// where [`Facts`]' precedence cannot see both and RBS reads two `def story_path:` lines as an
    /// overload set. First in URI order wins, as with shared relation classes.
    fn route_declarations(&self, declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        // An application that declares the constant itself meant something by it, and a module this
        // pass wrote into would answer with both its members and ours. Same rule as
        // `Comment::Relation`: losing the whole feature is the price of never shadowing a name
        // somebody chose.
        if declaring.context.namespaces.declares(rails::ROUTE_HELPERS) {
            return 0;
        }
        let mains: Vec<DocUri> = declaring
            .context
            .documents(ROUTES)
            .iter()
            .filter_map(|uri| DocUri::from_graph_uri(uri))
            .filter(|uri| {
                uri.to_file_path()
                    .is_some_and(|path| rails::is_routes(&path))
            })
            .collect();
        if mains.is_empty() {
            return 0;
        }
        let proxied = projection_of(declaring.context)
            .framework
            .contains(rails::ROUTES_PROXY);
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
                    // `Whose::Own` however the drawer was reached, on purpose: a drawn file has no
                    // wrapper (its statements *are* the body), and its drawer already cleared the
                    // gate. Asking a gem's drawn file for its own `Rails.application.routes.draw`
                    // would decline every one.
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
        // The project's own files first. The first-wins assignment below makes this matter: an
        // application that names `rails_blob_path` itself must be the one that declares it, and URI
        // order between a workspace path and a gem path is arbitrary. It also keeps the `include`s
        // (in `sources[0]`) in a file the user has.
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
            // The `include`s go in one document, the first, for the `MessageDelivery` stub's
            // reason: they belong to the *application*, not to any routes file, and N copies would
            // be N identical declarations. So do `main_app` and what a proxy answers, where the
            // bundle declares the proxy.
            if index == 0 {
                facts.extend(rails::mixins(&projection_of(declaring.context).hosts));
                if proxied {
                    facts.extend(rails::proxies());
                }
            }
            helpers += facts.len();
            super::add(into, uri, facts);
        }
        if proxied {
            helpers += Self::mounted_helpers(declaring, into);
        }
        helpers
    }

    /// The helper each of the project's engines defines where it is mounted, in the engine's own
    /// document, placed at the call that names it. The first engine to claim a name keeps it, in
    /// URI order, as a route helper does.
    fn mounted_helpers(declaring: &Declaring<'_>, into: &mut Declared) -> usize {
        let context = declaring.context;
        let mut named: BTreeSet<String> = BTreeSet::new();
        let mut helpers = 0;
        for uri in context.documents(ENGINES) {
            let Some(uri) = DocUri::from_graph_uri(uri) else {
                continue;
            };
            let Some(source) = (declaring.text)(&uri) else {
                continue;
            };
            let file = (declaring.caption)(&uri);
            let mut facts = Facts::default();
            for engine in rails::read_engines(&source) {
                // `isolate_namespace` names the module its constant resolves to, which is the one
                // the application declares.
                let name = match &engine.name {
                    rails::EngineName::Written(name) => Some(name.clone()),
                    rails::EngineName::Isolated(candidates) => candidates
                        .iter()
                        .find(|candidate| context.classes.contains(*candidate))
                        .and_then(|module| rails::engine_prefix(module))
                        .map(|prefix| prefix.trim_end_matches('_').to_owned()),
                };
                if let Some(name) = name.filter(|name| named.insert(name.clone())) {
                    facts.declare(rails::mounted_helper(&file, &name, &engine));
                }
            }
            helpers += facts.len();
            super::add(into, &uri, facts);
        }
        helpers
    }

    /// What `delegate` declares, and the two hops each name's type needs. This is all of phase two.
    ///
    /// `delegate :name, to: :user` on `Story` needs `Story#user -> User`, then
    /// `User#name -> String`. Both are facts the generators above wrote into *other files'*
    /// documents in this same pass, before anything is resolved or indexed. [`Facts::returns`] asks
    /// the question; the union below is what it asks.
    ///
    /// **The union is built once, and only on demand.** Once, because a `delegate` may derive from
    /// any file's facts. On demand, because a workspace with no `delegate` must not pay to merge
    /// every fact in the project.
    ///
    /// **What phase two writes is not in the union.** So a `delegate` through a `delegate` answers
    /// `untyped` instead of starting a fixed-point iteration over a graph a user can write a cycle
    /// into. The declarations still land in the source file's own document, where [`Facts`]'
    /// precedence sees them: a column named `title` outranks `delegate :title`, because the schema
    /// is what the database *is*.
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

    /// The document `draw :admin` reads: `config/routes/admin.rb` beside the drawer.
    ///
    /// `None` when the path cannot be built or the file is not indexed. The second is not a
    /// failure: a drawn file outside `index.include` draws nothing, like any file the pass was told
    /// not to read.
    fn drawn(drawer: &DocUri, name: &str) -> Option<DocUri> {
        let path = drawer.to_file_path()?;
        DocUri::from_path(&path.parent()?.join("routes").join(format!("{name}.rb")))
    }

    /// Which of the user's own classes read which table, where more than one may.
    ///
    /// **Direction is the safety argument.** Every table is looked up **from** an existing class,
    /// by pluralizing its name, never by singularizing a table name and hoping a class answers.
    /// Both need the same irregular rules, but only this one fails toward *nothing*: a class whose
    /// plural names no table declares nothing, and a table no class claims declares nothing.
    ///
    /// # Several classes can really read one table
    ///
    /// The rule "a table two classes claim is claimed by neither" guards against one thing: the
    /// inflector landing two *different* names on one table, where at most one can be right. It
    /// must not catch the same convention applied twice: `Account` and
    /// `Admin::CLI::Maintenance::Account` both really read `accounts`.
    ///
    /// So several claimants are kept when they all **demodulize to the same name**. A table two
    /// *different* names reach is still claimed by neither.
    ///
    /// A class that **names its own table** (`self.table_name = "settings"`) states a fact, not a
    /// guess, and never makes anything ambiguous. It **joins** the claim instead of replacing it,
    /// unless the class whose name implies the table is not a model. Replacing would let a
    /// throwaway class in a migration or a test take a real model's columns away.
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

        // The tables some class has *said* it reads: where an inflected claim meets a written one.
        // The written one should win when the writer is the real model (`TopicViewItem` says
        // `topic_views`, and the `TopicView` its name implies is a view object). It should not when
        // the writer is a throwaway, such as a legacy copy in a migration or a test double saying
        // `posts`. So the guess survives only when its class is a model.
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
            // `parent_singular_child_plural`, which needs the parent's own table, and its parent's.
            // Declined, not approximated: in practice no nested class names such a table.
            if projection_of(context).is_model(parent) {
                continue;
            }
            // The joined name a declaration would be written under must introduce no namespace. It
            // rarely declines anything; it is here because the damage it prevents is silent.
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
                // One class written in two places is **one claimant**. `claims` is filled per
                // *definition*, so a model reopened elsewhere (`class AuditLog` again in
                // `app/queries/audit_log/`) pushes its name twice. The narrowed rule below already
                // admits that, since a name demodulizes to itself. The dedup makes "one claimant"
                // mean one class, not one `class` keyword, so the list handed to the schema means
                // what it says.
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

fn view_context(
    context: &Context,
    models: &[(DocUri, String, Arc<rails::Model>)],
    template_paths: BTreeMap<String, rails::TemplatePath>,
) -> Views {
    let mut exports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut included: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut layouts: BTreeMap<String, rails::Layout> = BTreeMap::new();
    for (_, _, model) in models {
        for (owner, layout) in model.layouts() {
            layouts.insert(owner.to_owned(), layout.clone());
        }
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
        layouts,
        template_paths,
    )
}

/// What every body runs before a controller's actions, and the callback calls no body holds, from
/// every file the model reader opened. A body two files reopen keeps both files' calls.
fn controller_callbacks(
    models: &[(DocUri, String, Arc<rails::Model>)],
) -> (BTreeMap<String, rails::Callbacks>, rails::Callbacks) {
    let mut callbacks: BTreeMap<String, rails::Callbacks> = BTreeMap::new();
    let mut loose = rails::Callbacks::default();
    for (_, _, model) in models {
        for (owner, said) in model.callbacks() {
            let held = callbacks.entry(owner.to_owned()).or_default();
            held.before.extend(said.before.iter().cloned());
            held.skips.extend(said.skips.iter().cloned());
            held.named.extend(said.named.iter().cloned());
        }
        rails::absorb_callbacks(&mut loose, model.loose_callbacks());
    }
    (callbacks, loose)
}

/// Every ActiveRecord model, found by climbing what each class says it inherits.
///
/// **The whole chain, not one hop.** Engines put models two to five hops from `ActiveRecord::Base`,
/// and a one-hop test would call `Spree::Order` no model. Each hop resolves the spelling as Rails
/// does: [`candidates`] is `compute_type`'s own list, innermost nesting first and the bare name
/// last. Inside `module Spree`, `class Address < Spree::Base` and `class LineItem < Base` name the
/// same class.
///
/// `seen` is not caution about Ruby, which cannot have a superclass cycle. It is about *source*,
/// which can be written with one, and this walks the text.
///
/// **The chain stops at a name the application does not define.** `Tag < ActsAsTaggableOn::Tag` is
/// a real model whose base is in a gem, and this answers `false` for it. That is why the relation
/// set is a **union** with what the macros ask for, not a replacement: dropping those classes'
/// relation classes would make answers worse, not absent.
/// Every class whose primary key may not be its table's: each class that writes
/// `self.primary_key =` or `def self.primary_key`, every includer of a module that does, and every
/// class below any of them, since a subclass inherits the key. `ids` is typed for none of them.
fn rekeyed<'a>(
    models: impl Iterator<Item = &'a (DocUri, String, Arc<rails::Model>)>,
    includers: &BTreeMap<String, BTreeSet<String>>,
    superclasses: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let mut moved: BTreeSet<String> = BTreeSet::new();
    for (_, _, model) in models {
        for (name, module) in model.rekeyed() {
            if module {
                moved.extend(includers.get(name).into_iter().flatten().cloned());
            } else {
                moved.insert(name.to_owned());
            }
        }
    }
    if moved.is_empty() {
        return moved;
    }
    let below: Vec<String> = superclasses
        .keys()
        .filter(|name| {
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let mut current = name.as_str();
            while seen.insert(current) {
                if moved.contains(current) {
                    return true;
                }
                let Some(next) = superclasses.get(current).and_then(|written| {
                    candidates(current, written)
                        .into_iter()
                        .find_map(|candidate| superclasses.get_key_value(&candidate))
                }) else {
                    return false;
                };
                current = next.0;
            }
            false
        })
        .cloned()
        .collect();
    moved.extend(below);
    moved
}

fn models_of(superclasses: &BTreeMap<String, String>) -> BTreeSet<String> {
    inheriting(superclasses, rails::is_record_base)
}

/// Every class whose superclass chain, as [`models_of`] climbs it, reaches a superclass `stops`
/// accepts as written.
fn inheriting(
    superclasses: &BTreeMap<String, String>,
    stops: impl Fn(&str) -> bool,
) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    for name in superclasses.keys() {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut current = name.as_str();
        while seen.insert(current) {
            let Some(written) = superclasses.get(current) else {
                break;
            };
            if stops(written) {
                found.insert(name.clone());
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
    found
}

/// The class a model's class side goes on: the topmost model in its own superclass chain.
///
/// [`models_of`]'s walk with a different stop. That one climbs to `ActiveRecord::Base` and answers
/// *whether*; this one climbs while the next class up is a model the application declares, and
/// answers *which*. `Spree::Order < Spree::Base < ApplicationRecord` and
/// `Story < ApplicationRecord` both answer `ApplicationRecord`. One copy of the query interface
/// there is inherited by every model below it, as in Rails, so the cost is per application, not per
/// model.
///
/// **The base must be a class the application declares.** `Tag < ActsAsTaggableOn::Tag` stops at
/// `Tag`, because nothing may be declared on a gem's class, so such a model pays for its own copy.
/// `seen` bounds the walk as in [`models_of`], and each hop resolves its spelling through
/// [`candidates`] the same way.
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
    // One hop past the application, and only onto `ActiveRecord::Base` itself. In an application
    // with no `ApplicationRecord`, every model is its own base, so the walk above saves nothing,
    // and the class Rails really installs the interface on is in a gem. Reopening it is safe for
    // the `MessageDelivery` stub's reason: nothing declared here is mapped, so the gem keeps its
    // places. The gate is the namespace rule: the **bundle** must declare the name, or a generated
    // body would invent a constant.
    //
    // `ActiveRecord::Base` **exactly**, not [`rails::is_record_base`]'s other spelling. An
    // `ApplicationRecord` the application declares is reached by the walk above; one it does *not*
    // declare is a name this pass may not write on. Treating them as one question put the interface
    // on a bare `class ApplicationRecord` that inherits nothing.
    if let Some(written) = top.filter(|written| *written == rails::RECORD_BASE)
        && let Some(resolved) = candidates(current, written)
            .into_iter()
            .find(|candidate| namespaces.declares(candidate) && namespaces.spellable(candidate))
    {
        return resolved;
    }
    current.to_owned()
}

/// Every `(class, member)` some document already holds, as `Elsewhere::columns` wants it.
///
/// Called once, between the schemas and the models, so it holds exactly the schemas' members.
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

/// Which columns a model file re-types, keyed by the class the macro is written on.
///
/// What the model files tell the schema generator. It is an *input*, not a fact:
/// `Facts::returns` answers within one document, and these two declarations never share one.
/// `story.status` is the label `enum :status` names (a `String`), while its column is an
/// `Integer`, so the schema must say nothing about it.
///
/// A typed `attribute` travels the same way, because Rails documents that a cast type "will
/// override the type of existing attributes if needed". Only types this crate has a class for
/// are here, so `attribute :payload, :json` leaves `t.string "payload"` answering as before.
/// Every instance method each model class's own file writes with `def`, for the schema's attribute
/// methods to defer to.
fn defined_members(
    models: &[(DocUri, String, Arc<rails::Model>)],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut defined: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_, _, model) in models {
        for (class, name) in model.defined_members() {
            defined
                .entry(class.to_owned())
                .or_default()
                .insert(name.to_owned());
        }
    }
    defined
}

/// The columns `pick` must not answer for: per class, and by name on every class.
///
/// `Model::recast_columns` of every file on either list. A module's names go in the second half,
/// since the classes it re-types are its includers, and a concern's `serialize :preferences`
/// can reach any of them.
struct Recast {
    on: BTreeMap<String, BTreeSet<String>>,
    anywhere: BTreeSet<String>,
}

fn recast_columns<'a>(
    models: impl Iterator<Item = &'a (DocUri, String, Arc<rails::Model>)>,
) -> Recast {
    let mut recast = Recast {
        on: BTreeMap::new(),
        anywhere: BTreeSet::new(),
    };
    for (_, _, model) in models {
        for (host, column, module) in model.recast_columns() {
            if module {
                recast.anywhere.insert(column.to_owned());
            } else {
                recast
                    .on
                    .entry(host.to_owned())
                    .or_default()
                    .insert(column.to_owned());
            }
        }
    }
    recast
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

/// The innermost enclosing name that declares a prefix, or nothing.
///
/// `full_table_name_prefix` is
/// `(module_parents.detect { |p| p.respond_to?(:table_name_prefix) } || self).table_name_prefix`,
/// and `module_parents` is innermost first, so `Spree::Admin::Order` asks `Spree::Admin` before
/// `Spree`. The `|| self` branch is empty unless the application set
/// `ActiveRecord::Base.table_name_prefix` globally, which only running Ruby knows; the empty answer
/// here means that.
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

    /// Whether this project asked for the body of knowledge a list feeds.
    ///
    /// Nine rows, five keys: [`SCHEMAS`], [`RENAMED`] and [`ZONES`] are one family (a
    /// `schema.rb`, the macros that rename its tables and the settings that decide its time
    /// columns), and [`MODELS`] and [`CONCERNS`] are the same macros seen from either side. [`Features::resolve`](crate::workspace::Features::resolve) already folds
    /// `rails.enabled` into every Rails flag, so nothing here asks twice.
    ///
    /// **[`FRAMEWORK`] has no key of its own; the umbrella gates it.** Every other key lets a
    /// project decline knowledge that does not fit it: its own schema, macros, routes. Nothing here
    /// is project-specific. A bundle with railties cannot sensibly deny that `Rails.root` is a
    /// `Pathname`, and a bundle without it already declares nothing, because both ends of every row
    /// are checked against the graph first.
    fn wanted(&self, list: ListId, features: Features) -> bool {
        match list {
            SCHEMAS | RENAMED | ZONES => features.schema,
            MODELS | CONCERNS => features.models,
            ROUTES | ENGINES => features.routes,
            ENTRYPOINTS => features.entrypoints,
            FRAMEWORK | ADAPTERS | CONFIGURED => features.rails,
            _ => false,
        }
    }

    /// Every name this module writes onto or beside that is **not** the application's own:
    ///
    /// - the framework's singletons and what they return
    /// - the mailer's `MessageDelivery`
    /// - the module the route helpers go in
    /// - the relation class every model gets
    /// - `ActiveRecord::Base`, the class side's base when an application has no
    ///   `ApplicationRecord`; whether the bundle declares it decides whether this pass may write
    ///   there
    ///
    /// The singletons' **owners** are the subtle part: `Rails` and `Time` decide whether a body may
    /// be opened at all and *which keyword opens it*. railties writes `module Rails`, activesupport
    /// reopens Ruby's `class Time`, and the render key is `(is_module, name)`.
    fn spellable_names(&self) -> Vec<&'static str> {
        let mut names = rails::framework_classes();
        names.extend(rails::framework_constants());
        names.extend(rails::migration_constants());
        names.extend(rails::adapter_constants());
        names.push(rails::MESSAGE_DELIVERY);
        names.push(rails::ROUTE_HELPERS);
        names.push(rails::RELATION_BASE);
        names.push(rails::RECORD_BASE);
        names
    }

    fn shown(&self) -> &'static [(&'static str, &'static str)] {
        &rails::SHOWN
    }

    /// Five projections, each a fold over what the walk already saw.
    ///
    /// What keeps this cheap: `claims`, `nested`, `hosts` and `helpers` ask about the **names a
    /// document declares** and each one's superclass and `include`s, all already in [`Seen`]. Only
    /// `autoloaded` needs more, and it needs byte offsets, not names: [`Seen::confirming`].
    ///
    /// The order is [`Seen::declared`]'s: the order the document's definitions were recorded in.
    fn contribute(&self, seen: &Seen<'_>) -> Option<Box<dyn super::Contributes>> {
        let mut contribution = Contribution::default();
        // The suffix test avoids a path parse per document: `_helper.rb` leaves a handful of files,
        // and only those are asked [`rails::is_helper`], which is the real rule and also reads the
        // `app/helpers` anchor.
        let helper = seen.own
            && seen.uri.ends_with("_helper.rb")
            && DocUri::from_graph_uri(seen.uri)
                .and_then(|uri| uri.to_file_path())
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
            // A table is claimed by pluralizing a **top-level** class's name. `Admin::Setting`'s
            // table depends on `table_name_prefix`, which only running Ruby knows, so it is
            // declined here and left to `self.table_name`, which still works for it.
            //
            // `own`, not the wider width: a table is the *application's*. An engine's top-level
            // class would otherwise claim a table by pluralizing its name and compete with the
            // model that owns it. Two of these five fields mean "the application" rather than "a
            // class the reader can name": this one and `hosts`.
            if seen.own
                && !name.contains("::")
                && let Some(table) = rails::table_of(name)
            {
                contribution.claims.push((table, name.clone()));
            }
            // The table-name half of the same rule, kept as a name and not a table: `Spree::Order`
            // reads `spree_orders`, and the `spree_` comes from a file this fold may not open.
            if seen.own && name.contains("::") {
                contribution.nested.push(name.clone());
            }
            // And `own`-only even though the routes list is not. A host is a class the
            // *application's* route helpers are `include`d into. An engine's own controllers reach
            // their own helpers by their own mechanism, so each would cost an `include` of a module
            // holding nothing for it.
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
    /// **The only membership decided after the walk, and it has to be.** Every [`Wants`] predicate
    /// asks about one document, but whether a class is a model depends on the chain above it, which
    /// is complete only after every document is seen. A model with no macros is on no list, yet its
    /// own file is where its relation class belongs, so the file joins the list that opens it.
    /// `settle` sorts the list afterwards, so appending here cannot change which document writes
    /// what.
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

    /// What a directory conjures that no `class` declares.
    ///
    /// **A directory declares its namespace even where a file does too.** Zeitwerk defines the
    /// module from the directory whether or not a `module Chat` line exists, so deleting every such
    /// line leaves the constant, and the files that confirm the directory are places of it too (the
    /// user's operational test). Their generated bodies sort behind every written one
    /// (`locator::definitions_of`), so the written lines come first.
    ///
    /// **Not where a `class` declares it**: a `user.rb` writing `class User` beside the `user/`
    /// directory. A directory conjures a `module`, and rubydex holds one declaration per constant,
    /// so the two kinds would be a coin toss. So the filter runs here, once both the application's
    /// and the bundle's names are recorded.
    ///
    /// The caller then declares the survivors, which makes every prefix of a conjured name
    /// spellable: `Chat::Thread::Policy` needs `Chat::Thread`, which is either declared already or
    /// in this map too, because `rails::autoloaded_namespaces` answers the whole chain.
    fn after_the_bundle(&self, context: &mut Context) -> Vec<String> {
        let conjured: Vec<String> = projection_of(context)
            .autoloaded
            .keys()
            .filter(|name| context.namespaces.admits_a_module(name))
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

    /// Where Rails writes the query interface: the relation on the instance side, and the six
    /// class-side owners on the other.
    fn places_members_on(&self) -> [&'static [&'static str]; 2] {
        [&rails::RAILS_RELATION, &rails::RAILS_CLASS_SIDE]
    }

    fn discover(&self, reading: &Reading<'_>) -> Vec<DocUri> {
        Self::unindexed(reading)
    }

    fn discovered(&mut self, found: Vec<DocUri>) {
        self.found = found;
    }

    /// The namespaces a directory conjures, and nothing else.
    ///
    /// Runs first and shares nothing with the generators below: they key by the **file** they read,
    /// this keys by a **directory**, so no two can merge into one document.
    fn conjure(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        vec![(
            "conjured namespaces",
            self.autoloaded_declarations(declaring, into),
        )]
    }

    /// The seven generators that read a file, in the order their data forces.
    ///
    /// Three edges are real; the rest of the order is only deterministic:
    ///
    /// 1. Model files are parsed before the schema: an `enum` or a typed `attribute` re-types its
    ///    column, and the schema must defer.
    /// 2. The schema is declared before the models: an untyped `attribute` must defer to a column.
    /// 3. Concerns come after the models: they read what a model file said about
    ///    `included do … extend`.
    ///
    /// The first two travel on `retyped` and `columns`, both private to this module.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let sources = self.model_sources(declaring.context);
        let concerns = self.concern_sources(declaring.context);
        let retyped = retyped_columns(&sources);
        let defined = defined_members(&sources);
        let recast = recast_columns(sources.iter().chain(&concerns));
        let mut picked = rails::Picked::default();
        let schemas =
            self.schema_declarations(declaring, &retyped, &defined, (&recast, &mut picked), into);
        // `ids` reads the model's primary key, which a `self.primary_key =` moves.
        let moved = rekeyed(
            sources.iter().chain(&concerns),
            &declaring.context.includers,
            &declaring.context.superclasses,
        );
        picked.keys.retain(|class, _| !moved.contains(class));
        // What the schemas just said, so an untyped `attribute` of the same name can defer to it.
        // The rank already says the column wins (`Source::Column` 4, `Source::Attribute` 7), but
        // `Facts::declare` only settles collisions *inside* one document, and these two are in
        // different ones. So the loser declines, as an `enum` and a `delegate` do.
        let columns = declared_members(into);
        let resolver = self.resolver(declaring);
        let models =
            self.model_declarations(declaring, &sources, (&columns, &picked), &resolver, into);
        let entrypoints = self.entrypoint_declarations(declaring, into);
        let routes = self.route_declarations(declaring, into);
        let primary = resolver.primary();
        let (framework, migrations) =
            self.framework_declarations(declaring, primary.as_deref(), into);
        let installed = self.class_method_declarations(declaring, &concerns, into);
        let extended = self.concern_declarations(declaring, &concerns, into);
        let settings = self.setting_declarations(declaring, into);
        vec![
            ("settings a project assigns", settings),
            ("columns", schemas),
            ("members", models),
            ("entry points", entrypoints),
            ("route helpers", routes),
            ("framework returns", framework),
            ("what a migration sends to its connection", migrations),
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

    /// Every file this module's four readers open, read and parsed once, and only where the text
    /// changed since the last pass.
    ///
    /// Reads are per **document**, not per list: a model file that also writes `self.table_name=`
    /// is on two lists and parsed once. That is what [`Wanted`] is for.
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
        // The schema generator's filter, asked here so a `schema.rb` that is not a schema is never
        // read: the suffix puts a document on the list, and [`rails::is_schema`] decides.
        for uri in context.documents(SCHEMAS) {
            if DocUri::from_graph_uri(uri)
                .and_then(|uri| uri.to_file_path())
                .is_some_and(|path| rails::is_schema(&path))
            {
                wants.entry(uri.clone()).or_default().schema = Some(false);
            }
        }
        for uri in &projection_of(context).dumps {
            let wanted = wants.entry(uri.as_str().to_owned()).or_default();
            if uri
                .to_file_path()
                .is_some_and(|path| rails::is_database_config(&path))
            {
                wanted.database = true;
            } else {
                wanted.schema = Some(true);
            }
        }
        for uri in context.documents(ADAPTERS) {
            wants.entry(uri.clone()).or_default().adapters = true;
        }
        for uri in context.documents(CONFIGURED) {
            wants.entry(uri.clone()).or_default().configured = true;
        }

        // A file that left every list keeps nothing here. Not an optimisation: the memo is keyed by
        // URI, and a file deleted and written again is a different file, so an entry nothing asks
        // for is one whose freshness nothing will check.
        self.sources.retain(|uri, _| wants.contains_key(uri));

        for (key, wanted) in wants {
            let Some(uri) = DocUri::from_graph_uri(&key) else {
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

    /// The two entry-point conventions, asked once for both callers. It is the same
    /// [`rails::convention_of`] the reader asks, so which documents are opened and which classes
    /// are read cannot disagree.
    fn claims_by_ancestry(&self, superclass: Option<&str>, mixins: &[String]) -> bool {
        rails::convention_of(superclass, mixins).is_some()
    }
}

/// What one document contributes to [`Projection`].
///
/// **Every field is a fold over what the walk already saw**: the names the document declares, each
/// one's superclass and `include`s, plus the document's URI. Nothing here reads the graph or opens
/// a file, so the projection costs no second pass.
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
    /// `(namespace a directory spells, the line in *this* document that confirmed it)` for every
    /// directory between the document's autoload root and the document. Empty unless the document's
    /// own constant agrees with the deepest one.
    ///
    /// **The span lives here, not in the generator**, where every other span in this pass is read.
    /// A namespace's generated document is keyed by the confirming file, which is usually a plain
    /// controller on no generator's list, so the *sources changed* gate never sees edits to it.
    /// Only the gate that compares this struct with last time's can. Without the span here, the
    /// mapping goes stale the first time someone presses return above the `class` line, and
    /// `Synthesized::record` cannot catch that: it compares *text*, and the text is identical.
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
    /// A *map*, because two directories can spell one namespace (`app/models/user/` and
    /// `app/services/user/`), and which file a reader sees first must not depend on the graph's
    /// document order. Sorted, so there is no order to get wrong.
    pub autoloaded: BTreeMap<String, BTreeMap<String, Option<At>>>,
    /// Which of the application's classes are ActiveRecord models.
    ///
    /// **Decidable only after the walk**, so it is not a contribution: a model is a class whose
    /// superclass chain reaches `ActiveRecord::Base`, and the chain spans files. Filled once the
    /// merge is complete.
    pub models: BTreeSet<String>,
    /// The gem classes the long-tail macros install members on, which the bundle has to declare
    /// before this pass may write there.
    pub framework: BTreeSet<String>,
    /// `db/*structure.sql`: the one input that is not a graph document at all.
    ///
    /// A `read_dir` has no defined order. Which schema source is read first decides nothing, but
    /// only because the ambiguity rule runs first, and a list that varies per run is a bug waiting
    /// to happen. So [`Projects::settle`](super::Projects::settle) sorts it.
    pub dumps: Vec<DocUri>,
}

impl Projection {
    /// An empty projection, for a build this module is not registered in.
    ///
    /// `const` so a reader can fall back to a `static` instead of an `Option` at every call site:
    /// *Rails is not registered* should read as *there are no models*, not as a branch per read.
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
    /// The gate on a new claimant, and it matters: `claims` holds every top-level class the
    /// application defines, not only models. Once nested classes may claim, a nested non-model
    /// class whose name inflects onto a real table would take that table from the model that reads
    /// it. Checking the superclass chain declines those and keeps every real claimant.
    ///
    /// A lookup, not a walk, because it is asked of every class: the chain is walked once, after
    /// the merge.
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

    /// `claims` is pushed per definition in the graph's iteration order, which is a `HashMap`'s. No
    /// answer changes (`model_tables` sorts and dedups its own copy), but two walks over one
    /// workspace must produce the same projection: the wide gate compares it, and the per-document
    /// contributions rely on it.
    fn settle(&mut self) {
        for classes in self.claims.values_mut() {
            classes.sort_unstable();
        }
        self.dumps.sort_unstable();
    }

    /// A directory-conjured namespace is on no list: it comes from the **paths** the walk visited,
    /// not from a document a generator opens. So a workspace whose only Rails fact is a conjured
    /// namespace still has something to declare.
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
/// Reading its own projection back out of a [`Context`] is the normal case, and the downcast is
/// this module's: core holds `dyn Projects` and names nothing.
#[must_use]
pub fn projection_of(context: &super::Context) -> &Projection {
    static NONE: Projection = Projection::empty();
    context.projection().unwrap_or(&NONE)
}

/// Zeitwerk's implicit namespaces, where **the file agrees with the directory**.
///
/// `app/services/user/policy/not_already_silenced.rb` conjures `User::Policy` because it declares
/// `User::Policy::NotAlreadySilenced`: the directory's name plus exactly one segment, which is
/// Zeitwerk's contract. Rails would not load a file declaring anything else from there, and a
/// directory holding only such files conjures nothing reachable.
///
/// `own` code only: a gem's `app/` is loaded by the gem's own autoloader, which has other roots.
///
/// **The declared name is checked first and the path second**, for the helper test's reason:
/// parsing a path allocates per document, and this runs before every resolve. A file declaring no
/// `::` name cannot agree with any directory, and that is most files in an application.
fn autoloaded(seen: &Seen<'_>) -> Vec<(String, Option<At>)> {
    if !seen.own || !seen.declared.iter().any(|(name, _)| name.contains("::")) {
        return Vec::new();
    }
    let Some(path) = DocUri::from_graph_uri(seen.uri).and_then(|uri| uri.to_file_path()) else {
        return Vec::new();
    };
    let proposed = rails::autoloaded_namespaces(&path);
    // The chain the *file* spells. It matches the directory's when the project uses Zeitwerk's own
    // inflector, and is the right one when it does not. Every constant the document declares is
    // tried, not only the first: one file may write `class Api::V1::Foo` beside another class, and
    // the one that agrees confirms the directory.
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

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// The superclass map as a walk reads it: `class Story < ApplicationRecord` is one row.
    fn chain(rows: &[(&str, &str)]) -> BTreeMap<String, String> {
        rows.iter()
            .map(|(name, written)| ((*name).to_owned(), (*written).to_owned()))
            .collect()
    }

    const ROWS: [(&str, &str); 7] = [
        ("Story", "ApplicationRecord"),
        ("ApplicationRecord", "ActiveRecord::Base"),
        // A real model whose base is in a gem: the chain leaves the application, so this answers
        // `false`.
        ("Tag", "ActsAsTaggableOn::Tag"),
        ("Widget", "Helper"),
        ("Helper", "Object"),
        // Not valid Ruby, but *source* can be written with a cycle, and this walks the text.
        ("Loop", "Knot"),
        ("Knot", "Loop"),
    ];

    #[test]
    fn a_model_is_a_class_whose_written_chain_reaches_active_record() {
        let models = models_of(&chain(&ROWS));
        assert_eq!(
            models.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["ApplicationRecord", "Story"]
        );
        // The cycle terminates by the `seen` set rather than by the chain running out, which is
        // the one thing bounding this walk at all.
        assert!(!models.contains("Loop"), "{models:?}");
    }

    #[test]
    fn the_base_a_class_side_goes_on_is_the_topmost_model_of_its_own_chain() {
        let superclasses = chain(&ROWS);
        let models = models_of(&superclasses);
        let mut namespaces = crate::generated::Namespaces::default();
        namespaces.declare("ActiveRecord".to_owned(), true);
        namespaces.declare(rails::RECORD_BASE.to_owned(), false);
        let base = |name: &str| base_of(name, &superclasses, &models, &namespaces);

        // One hop past the application, and only ever onto `ActiveRecord::Base` itself: one
        // copy of the query interface is inherited by every model beneath it.
        assert_eq!(base("Story"), rails::RECORD_BASE);
        assert_eq!(base("ApplicationRecord"), rails::RECORD_BASE);
        // The chain leaves the application, so the model is its own base and pays for its own
        // copy — nothing may be declared on a gem's class.
        assert_eq!(base("Tag"), "Tag");
        // The next class up is not a model, so the walk stops where it is.
        assert_eq!(base("Widget"), "Widget");
        // And the cycle is bounded here too.
        assert_eq!(base("Loop"), "Loop");
    }

    #[test]
    fn a_base_nothing_declares_is_not_a_name_this_pass_may_write_on() {
        // The gate the `top` comment describes: an `ApplicationRecord` the application declares is
        // reached by the walk, and an `ActiveRecord::Base` the *bundle* does not declare would make
        // a generated body invent a constant.
        let superclasses = chain(&ROWS);
        let models = models_of(&superclasses);
        let namespaces = crate::generated::Namespaces::default();
        assert_eq!(
            base_of("Story", &superclasses, &models, &namespaces),
            "ApplicationRecord"
        );
    }
}
