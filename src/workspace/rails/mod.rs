//! Everything ya-lsp knows about Rails, and nothing else in the crate knows any of it.
//!
//! A directory, not a file, to protect one review property: **"how much framework is in here" is
//! answered by reading one list**, and that list is this file. The convention tables are `const`
//! arrays below, `mod.rs` is the only public surface, and every submodule is private.
//!
//! # The modules
//!
//! - [`conventions`]: rules about a **path** alone (which controller renders a template, which
//!   class a mailer's views hang off, which files are helpers Rails globs, which files
//!   `rails db:migrate` dumps a schema into). None opens a file.
//! - [`inflect`]: Rails' inflector, minus what real projects did not need.
//! - [`syntax`]: the half-dozen Prism shapes the readers start from.
//! - [`schema`]: `db/*schema.rb`: which tables exist and what their columns return.
//! - [`structure`]: `db/*structure.sql`: the same tables, from the dump the database's own tool
//!   wrote. The one reader that is not Ruby, and the only one without Prism.
//! - [`attributes`]: `attribute`, its optional cast type, and the three different features that
//!   spell their macro that way.
//! - [`models`]: one model file: which bodies it holds, which may declare, and the order the
//!   families come back together in.
//! - [`associations`]: `belongs_to`, `has_one`, `has_many`, `has_and_belongs_to_many` and `scope`:
//!   every member one line installs, and the four left out.
//! - [`relations`]: the relation class every model gets, the two classes a scope goes on, and the
//!   query interface shared by the relation and the model's singleton.
//! - [`concerns`]: a concern body's macros, and which including classes they land on.
//! - [`delegates`]: `delegate`, and the two hops from a name written here to a type written
//!   elsewhere. The only reader needing *other* generators' output, hence a second phase, not
//!   another macro in [`models`].
//! - [`enums`]: `enum`, both spellings, and the 3 + 4N names one call installs.
//! - [`tail`]: every remaining member-naming macro, as one table: where each takes its names, what
//!   it installs around them, how each is typed, and the four that are read and install nothing.
//! - [`entrypoints`]: the two conventions with no macro: a mailer's actions and a job's `perform`,
//!   recognised by superclass or mixin and read from a `def`.
//! - [`routes`]: `config/routes.rb`: which helpers Rails names, the one module they go in, and
//!   every class that `include`s it.
//! - [`framework`]: what the framework's own singletons return, and the rows left out. The one
//!   generator with almost no input: it reads a class name from `config/application.rb` and takes
//!   the rest from a table.
//! - [`migrations`]: what a migration's `method_missing` sends to its connection, and the column
//!   methods `define_column_methods` writes, both read out of activerecord's own files.
//! - [`layouts`]: which layout a controller's or a mailer's views are rendered in, from the
//!   `layout` each class wrote and the layout templates that exist.
//!
//! # What they all share
//!
//! **Pure text in, text out, no I/O and no graph.** A generator here gets a `&str` and answers with
//! [`Facts`](crate::generated::Facts); which file the text came from, which classes the application
//! defines and which of two schemas to believe are the caller's business, and the caller is
//! [`analysis::synthesize`](crate::analysis). That makes this directory testable by reading, and it
//! carries a 100% coverage floor: a convention reaching the wrong class answers confidently and
//! wrongly about the file the user is looking at, and one reaching nothing looks exactly like a
//! project that does not follow it.

mod adapters;
mod associations;
mod attributes;
mod blocks;
mod callbacks;
mod concerns;
mod conventions;
mod current;
mod delegates;
mod entrypoints;
mod enums;
mod framework;
mod inflect;
mod layouts;
mod migrations;
mod models;
mod relations;
mod renders;
mod routes;
mod schema;
mod structure;
mod syntax;
mod tail;

pub use adapters::{
    Connection, DatabaseConfig, Registered, Resolver, adapter_constants, connection_rows,
    is_database_config, read_database_config, read_registered,
};
pub use callbacks::{
    Actions as CallbackActions, Before, Callbacks, Skip as CallbackSkip,
    absorb as absorb_callbacks, runs_before,
};
pub use concerns::{
    CLASS_METHODS, ClassMethod as ConcernMethod, From as ConcernSource,
    declare as declare_concern_members, installed as concern_members,
};
pub use conventions::{
    autoloaded_constant, autoloaded_namespaces, confirmed_spelling, controller_of, is_helper,
    is_jbuilder, is_routes, is_schema, is_structure, mailer_in, mailer_of, named_constant,
    renders_its_own_variables, same_constant, template_of,
};
pub use current::{BASE as CURRENT_ATTRIBUTES, CurrentAttribute};
pub use entrypoints::{
    Entrypoints, MESSAGE_DELIVERY, TemplatePath, convention_of, is_mailer, read_entrypoints,
};
pub use framework::{
    RAILTIE_CONFIGURATION, application_class, config_facts, framework_constants,
    read_config_writes, read_framework,
};
pub use inflect::{camelize, helper_module, table_of};
pub use layouts::{Layout, Layouts, Link, Said as LayoutSaid, is_layout, layouts_of};
pub use migrations::{migration_constants, migration_sources, read_migrations};
pub use models::{Elsewhere, Model, RECORD_BASE, is_record_base, read_model};
pub use relations::{
    PROMISE, RAILS_CLASS_SIDE, RAILS_RELATION, RELATION_BASE, WHERE_CHAIN, relation_base,
};
pub use renders::{
    Default, JBUILDER, Local, Locals, Name, RENDER_CALLS, Render, StrictLocals, Value, partial_of,
    read_renders, strict_locals,
};
pub use routes::{
    Engine, EngineName, ROUTES_PROXY, Routes, Whose, hosts_routes, mixins, mounted_helper, proxies,
    read_engines, read_routes,
};
pub use schema::{Picked, Schema, TableNames, engine_prefix, read_schema, read_table_names};
pub use structure::read_structure;

pub use associations::COLLECTION_PROXY;
use associations::Kind;
use entrypoints::Convention;
use tail::Installs;

/// The column types a schema dumper writes, and the Ruby class ActiveRecord reads each as.
///
/// Checked against ActiveRecord's type maps (abstract, PostgreSQL, MySQL, SQLite) in Rails 7.2,
/// 8.0, 8.1 and main, where each type's `cast` or `deserialize` says what a stored value becomes.
/// Four rows deserve a sentence:
///
/// - **`datetime` is [`TIME_WITH_ZONE`], not `Time`.** Rails' railtie sets
///   `time_zone_aware_attributes`, and a `datetime` column's value is `in_time_zone`'d on the way
///   out. `Time` is the class that one delegates to, and a label saying it is wrong: `+`, `ago` and
///   `beginning_of_day` hand back a `TimeWithZone` too. The default is written in no project file,
///   and the four settings that change it are ([`TIME_ZONE_SETTINGS`]); a project writing one
///   gets no class for these columns ([`ZONED`]).
/// - **`decimal` is `BigDecimal` only where it has digits after the point.** A precision with no
///   scale is `DecimalWithoutScale`, an `Integer`; see [`WHOLE_DECIMAL`].
/// - **`binary` is `String`**, because that is what ActiveRecord returns: bytes in a `String`, not
///   an IO.
/// - **PostgreSQL's own types** hold one class whatever the version: `uuid`, `citext` and the
///   other string types are `String` subclasses in ActiveRecord, `inet` and `cidr` an `IPAddr`,
///   `hstore` a `Hash` of strings, `money` a `BigDecimal` and `oid` an `Integer`.
/// - **`serial` and `bigserial` are an `integer` and a `bigint`** with a sequence behind them. They
///   are how PostgreSQL's dumper spells such a column (`id: :serial`), not a type of their own:
///   the column reads as its integer type.
///
/// - **`time`, `timestamp` and `timestamptz` are [`EITHER_TIME`]**: time-zone aware
///   only from Rails 5.1 and 7.1 on, `timestamp` on MySQL and not on PostgreSQL, so the class
///   depends on what this reader cannot see, and either is right. PostgreSQL's `timetz` has no
///   ActiveRecord type, so it reads as the `String` the driver hands back.
/// - **PostgreSQL's other types** (checked in 7.2 and 8.1's `initialize_type_map`): `interval` is
///   an `ActiveSupport::Duration` (6.1 on), an `enum` its label, each range a `Range`, `point` an
///   `ActiveRecord::Point`, and the other geometric types the `String` PostgreSQL writes. A
///   `virtual` column is read as its `type:`.
///
/// Anything else is declared `untyped`, not skipped: a member that exists with no type, versus no
/// member at all, and the schema is judged by member lookups that would otherwise fail. A mapping
/// is a claim; `untyped` is the absence of one. The one left untyped on purpose: **`json` and
/// `jsonb`**, which hold whatever JSON decodes to.
///
/// On PostgreSQL a `date` or `datetime` column can hold `'infinity'`, which ActiveRecord reads as
/// `Float::INFINITY`. A project stores that on purpose, and every column would otherwise be a
/// union, so the table names the class every other value has.
const COLUMN_TYPES: [(&str, &str); 44] = [
    ("string", "String"),
    ("text", "String"),
    ("binary", "String"),
    ("integer", "Integer"),
    ("bigint", "Integer"),
    ("boolean", "bool"),
    ("float", "Float"),
    ("decimal", "BigDecimal"),
    ("datetime", TIME_WITH_ZONE),
    ("date", "Date"),
    ("uuid", "String"),
    ("citext", "String"),
    ("ltree", "String"),
    ("tsvector", "String"),
    ("xml", "String"),
    ("macaddr", "String"),
    ("bit", "String"),
    ("bit_varying", "String"),
    ("inet", "IPAddr"),
    ("cidr", "IPAddr"),
    ("hstore", "Hash[String, String?]"),
    ("money", "BigDecimal"),
    ("oid", "Integer"),
    ("serial", "Integer"),
    ("bigserial", "Integer"),
    ("time", EITHER_TIME),
    ("timestamp", EITHER_TIME),
    ("timestamptz", EITHER_TIME),
    ("timetz", "String"),
    ("interval", "ActiveSupport::Duration"),
    ("enum", "String"),
    ("daterange", "Range[untyped]"),
    ("numrange", "Range[untyped]"),
    ("tsrange", "Range[untyped]"),
    ("tstzrange", "Range[untyped]"),
    ("int4range", "Range[untyped]"),
    ("int8range", "Range[untyped]"),
    ("point", "ActiveRecord::Point"),
    ("line", "String"),
    ("lseg", "String"),
    ("box", "String"),
    ("path", "String"),
    ("polygon", "String"),
    ("circle", "String"),
];

/// What a time-zone-aware time reads as in a Rails application.
const TIME_WITH_ZONE: &str = "ActiveSupport::TimeWithZone";

/// What a time column reads as where whether it is time-zone aware is not read: `time` and
/// `timestamptz` are from 5.1 and 7.1 on, `timestamp` on MySQL and not on PostgreSQL, and a
/// `datetime` is not where a project writes one of [`TIME_ZONE_SETTINGS`]. Either class is right.
const EITHER_TIME: &str = "ActiveSupport::TimeWithZone | Time";

/// The [`COLUMN_TYPES`] whose class rests on Rails' time-zone default.
const ZONED: [&str; 1] = ["datetime"];

/// The settings that move Rails' time-zone default, as the setters a project calls.
///
/// - `time_zone_aware_attributes = false` turns the conversion off, and
///   `skip_time_zone_conversion_for_attributes` turns it off for some columns of one model.
/// - `time_zone_aware_types` decides which column types it applies to.
/// - PostgreSQL's `datetime_type = :timestamptz` makes a `timestamp without time zone` column a
///   `:timestamp`, which is not converted, while a `structure.sql` still spells it as a `datetime`.
///
/// Written anywhere in the project's own code, one of them makes [`ZONED`] columns `untyped`: which
/// class they hold is then a value this reader does not follow.
pub const TIME_ZONE_SETTINGS: [&str; 4] = [
    "time_zone_aware_attributes=",
    "skip_time_zone_conversion_for_attributes=",
    "time_zone_aware_types=",
    "datetime_type=",
];

/// What a `decimal` with no digits after the point reads as.
///
/// ActiveRecord registers `DecimalWithoutScale`, an `ActiveModel::Type::BigInteger`, for a
/// `decimal(p)` or `decimal(p,0)` in every adapter (`extract_scale` answers 0 for both), and the
/// dumper then writes `precision:` with no `scale:`. A bare `decimal` has no precision and stays a
/// `BigDecimal`. MySQL always reports one, so its bare `decimal` is dumped with `precision: 10` and
/// is this.
const WHOLE_DECIMAL: &str = "integer";

/// What the schema dumper writes inside a `create_table` block that is not a column.
///
/// Everything else *is* one: the rule is "a call on the block parameter whose first argument is a
/// string literal, except `index`", and this list is that exception plus the four constraint
/// macros, which also take a string first argument and would otherwise declare a member named after
/// a table.
const NOT_COLUMNS: [&str; 5] = [
    "index",
    "foreign_key",
    "check_constraint",
    "exclusion_constraint",
    "unique_constraint",
];

/// The column `create_table` declares without being asked, and its type when nothing says.
const PRIMARY_KEY: (&str, &str) = ("id", "bigint");

/// Words Rails' inflector will not pluralize, and those it pluralizes by replacement.
///
/// Both tables are deliberately short. An unknown word pluralizes to something no table is called,
/// the class matches nothing, and the answer is **nothing**: the failure direction this is built to
/// have, and why the class→table direction was chosen over singularizing table names.
const UNCOUNTABLE: [&str; 10] = [
    "equipment",
    "information",
    "money",
    "rice",
    "series",
    "species",
    "fish",
    "sheep",
    "news",
    "police",
];

const IRREGULAR: [(&str, &str); 11] = [
    ("person", "people"),
    ("man", "men"),
    ("woman", "women"),
    ("child", "children"),
    ("mouse", "mice"),
    ("ox", "oxen"),
    ("sex", "sexes"),
    ("move", "moves"),
    ("bus", "buses"),
    ("status", "statuses"),
    ("alias", "aliases"),
];

/// The macros a model file declares that name a member.
///
/// Chosen from real applications, several of them, because one application describes itself, not
/// the framework. Two kinds of evidence shaped it:
///
/// - `validates` is the most common macro in a real Rails model and still declares nothing an
///   editor can use, so it is absent.
/// - `has_and_belongs_to_many` is rare, which is *not* evidence: `rubocop-rails` lints it away by
///   default. A rare name is excluded only when it also costs something, and this one costs a row
///   (Rails implements it by calling `has_many`).
///
/// `delegate` is the only entry whose reader runs in a second phase: its name is read in the same
/// walk as the rest, but what it *returns* is a fact another file's generator writes in this same
/// pass. See [`delegates`].
///
/// `attribute` is the only entry whose commonest caller is **not Rails**: in most applications most
/// `attribute` calls are `active_model_serializers`' same-named macro, which defines the same
/// member but takes an options hash where Rails takes a cast type. That is why it is listed here
/// instead of gated on a superclass; see [`attributes`].
///
/// Most of the rest are [`LONG_TAIL`]'s: one table instead of a reader each, because each installs
/// a fixed set of members named around the call's names. **Three of that table's names are
/// deliberately missing here**: `encrypts`, `generates_token_for` and `normalizes` install no
/// method, so a file whose only macro is one of them declares nothing, and this list decides which
/// documents `synthesize` opens and parses.
///
/// `helper_method` and `helper` are here for a unique reason: neither declares anything in RBS,
/// ever, but the file must still be opened, because [`Model::exports`] and
/// [`Model::helper_modules`] read what they say and `analysis::views` answers it. Listing them is
/// cheap: `Wants::calls` opens a document that **calls** one of these names, and few files write
/// either.
///
/// Only `helper_method` is in [`LONG_TAIL`], and the asymmetry is the point: it really defines a
/// method (`def current_user(...)` on `_helpers`), so it belongs in a table of "every remaining
/// member-naming macro", declining there because there is nowhere to put it. `helper` names no
/// member at all; it `include`s a module into the view context, which this crate models but never
/// declares.
///
/// `layout` is here for `helper`'s reason: it declares nothing, and [`Model::layouts`] reads which
/// layout the class's views are rendered in, which `analysis::views` asks to find the classes a
/// layout template's variables and exports come from.
///
/// The callback macros ([`callbacks::NAMES`]) for `layout`'s reason: none declares anything, and
/// [`Model::callbacks`] reads what each runs before an action.
pub const MACROS: [&str; 48] = [
    "belongs_to",
    "has_one",
    "has_many",
    "has_and_belongs_to_many",
    "scope",
    "enum",
    "delegate",
    "attribute",
    "class_attribute",
    "mattr_reader",
    "mattr_writer",
    "mattr_accessor",
    "cattr_reader",
    "cattr_writer",
    "cattr_accessor",
    "thread_mattr_reader",
    "thread_mattr_writer",
    "thread_mattr_accessor",
    "thread_cattr_reader",
    "thread_cattr_writer",
    "thread_cattr_accessor",
    "accepts_nested_attributes_for",
    "store_accessor",
    "store",
    "alias_attribute",
    "serialize",
    "has_secure_token",
    "has_secure_password",
    "composed_of",
    "has_one_attached",
    "has_many_attached",
    "has_rich_text",
    "delegated_type",
    "helper_method",
    "helper",
    "layout",
    "before_action",
    "prepend_before_action",
    "append_before_action",
    "skip_before_action",
    "skip_callback",
    "reset_callbacks",
    "after_action",
    "prepend_after_action",
    "append_after_action",
    "around_action",
    "prepend_around_action",
    "append_around_action",
];

/// Every receiverless name that puts a document in front of [`models::read_model`].
///
/// [`MACROS`] **plus `class_methods` and `included`**, neither of which is a macro, hence not on
/// that list. `class_methods` declares no member of its body: `ActiveSupport::Concern` evaluates
/// its block on a nested `ClassMethods` module, and the `def`s inside are the declaration.
/// `included` declares none either, and is here for a narrower reason: a bare `extend M` inside one
/// puts `M`'s instance methods on every including class's singleton, and a concern writing nothing
/// else would be on no list. `activemodel/lib/active_model/api.rb` is exactly that file, and it
/// installs `model_name` and `human_attribute_name` on every Rails model. See [`concerns`].
///
/// **Derived from [`MACROS`], not written beside it**, unlike [`LONG_TAIL`]: there the two lists
/// hold the same *kind* of thing, and a name in one but not the other is a bug worth a test; here
/// one list is the other plus names that are deliberately not macros.
pub const MODEL_CALLS: [&str; MACROS.len() + 2] = {
    let mut names = [""; MACROS.len() + 2];
    let mut at = 0;
    while at < MACROS.len() {
        names[at] = MACROS[at];
        at += 1;
    }
    names[MACROS.len()] = "class_methods";
    names[MACROS.len() + 1] = "included";
    names
};

/// Every remaining macro that names a member, and which family it is in.
///
/// What each family installs is [`tail::row`], one function with every family's members side by
/// side, each read from Rails' source, not remembered.
///
/// **Four declare nothing and are listed anyway, for two different reasons.** `normalizes`
/// decorates an attribute's type, and `encrypts` and `generates_token_for` install their pairs once
/// in a framework module, not per call: none defines a method per call, so there is nothing to
/// declare. `helper_method` **does** define one (`def current_user(...)` on the controller's
/// `_helpers` module), installed on the **view context**. [`Installs`] keeps the two apart because
/// only the first is a dead end: `analysis::views` models the view context, and [`models`] reads
/// the macro's names into [`Model::exports`] instead of declaring them here. So it stays
/// [`Installs::Elsewhere`] (this table has nothing to say about it) while being on [`MACROS`]: the
/// one entry where those two facts differ. A macro *missing* from this table looks exactly like one
/// nobody thought about, so each of the four is read and declined, and [`tail`]'s docs cite the
/// line of Rails that settles it.
///
/// The twelve `mattr_`/`cattr_` spellings are one family with four rows each, and are not trimmed
/// by usage, for [`MACROS`]' reason: a rare spelling like `thread_cattr_writer` costs only a row.
const LONG_TAIL: [(&str, Installs); 29] = [
    ("class_attribute", Installs::ClassAttribute),
    ("mattr_reader", Installs::ModuleReader),
    ("mattr_writer", Installs::ModuleWriter),
    ("mattr_accessor", Installs::ModuleAccessor),
    ("cattr_reader", Installs::ModuleReader),
    ("cattr_writer", Installs::ModuleWriter),
    ("cattr_accessor", Installs::ModuleAccessor),
    ("thread_mattr_reader", Installs::ModuleReader),
    ("thread_mattr_writer", Installs::ModuleWriter),
    ("thread_mattr_accessor", Installs::ModuleAccessor),
    ("thread_cattr_reader", Installs::ModuleReader),
    ("thread_cattr_writer", Installs::ModuleWriter),
    ("thread_cattr_accessor", Installs::ModuleAccessor),
    ("accepts_nested_attributes_for", Installs::NestedAttributes),
    ("store_accessor", Installs::StoreAccessor),
    ("store", Installs::Store),
    ("alias_attribute", Installs::AliasAttribute),
    ("serialize", Installs::Serialize),
    ("has_secure_token", Installs::SecureToken),
    ("has_secure_password", Installs::SecurePassword),
    ("composed_of", Installs::ComposedOf),
    ("has_one_attached", Installs::OneAttached),
    ("has_many_attached", Installs::ManyAttached),
    ("has_rich_text", Installs::RichText),
    ("delegated_type", Installs::DelegatedType),
    ("encrypts", Installs::Nothing),
    ("generates_token_for", Installs::Nothing),
    ("helper_method", Installs::Elsewhere),
    ("normalizes", Installs::Nothing),
];

/// Every class a generated row names that a **gem** declares, not this application.
///
/// The three a [`LONG_TAIL`] macro needs from a bundle, [`WHERE_CHAIN`], which a bare `where`
/// returns, and [`ROUTES_PROXY`], which a mounted engine's helper does. `Context` looks each up in the graph once per pass, and a workspace whose bundle lacks
/// Active Storage declares no `has_one_attached` at all, instead of one typed as an unreachable
/// class.
#[must_use]
pub fn framework_classes() -> Vec<&'static str> {
    LONG_TAIL
        .iter()
        .filter_map(|(_, installs)| tail::gem_class(*installs))
        .chain([WHERE_CHAIN, PROMISE, COLLECTION_PROXY, ROUTES_PROXY])
        .collect()
}

/// The five that name another class, and what each returns.
///
/// Kept beside [`MACROS`], not derived from it, so a name added to one and forgotten in the other
/// fails a test instead of producing a file that is read and declares nothing. Three [`MACROS`]
/// entries are not here:
///
/// - `enum` names a column's values, not a class, so [`enums`] reads it and this table has nothing
///   to say.
/// - `delegate` names a *method* whose class is two hops away, so [`delegates`] reads it in a
///   second phase, and its `to:` is not a class name.
/// - `attribute` names a **cast type**, not a class: one of Rails' type registries
///   ([`attributes`]), not constants an application defines.
const ASSOCIATIONS: [(&str, Kind); 5] = [
    ("belongs_to", Kind::One),
    ("has_one", Kind::One),
    ("has_many", Kind::Many),
    // Rails' own last line of the macro is `has_many name, scope, **hm_options, &extension`, so
    // declaring it a collection is not an approximation of the framework but exactly what it does:
    // `class_name:` is forwarded, and so is the element type its name implies.
    ("has_and_belongs_to_many", Kind::Many),
    ("scope", Kind::Scope),
];

/// A superclass whose name *ends* with one of these makes the class a mailer, a job or a worker,
/// and the `def`s in its body are read.
///
/// The suffix, not the class's own name: a migration named `EnqueueValidateOpenaiHooksJob`
/// inheriting `ActiveRecord::Migration[7.1]` would otherwise get `perform_later` declared on it.
/// The superclass is also what carries the framework in: `ApplicationJob`, `ApplicationMailer`,
/// `Spree::BaseMailer`, `Devise::Mailer` and `ActivityPub::DeliveryWorker` are all someone's own
/// base class, and each ends in the word for what it is.
///
/// `Worker` is here because of Sidekiq: applications using it subclass workers without including
/// `Sidekiq::Worker` themselves. The gate is still `def perform`: a class inheriting one of these
/// and defining nothing declares nothing.
const INHERITS: [(&str, Convention); 3] = [
    ("Mailer", Convention::Mailer),
    ("Job", Convention::Job),
    ("Worker", Convention::Worker),
];

/// The two framework base classes, which end in neither of [`INHERITS`]' words.
///
/// Not the same rule twice: `ApplicationMailer < ActionMailer::Base` is the base every Rails
/// application generates, and a suffix rule alone misses every class subclassing the framework
/// directly (one application's mailers all do). An exact match, not a suffix, because half of Rails
/// ends in `Base`.
const BASES: [(&str, Convention); 2] = [
    ("ActionMailer::Base", Convention::Mailer),
    ("ActiveJob::Base", Convention::Job),
];

/// The module ya-lsp writes the route helpers into.
///
/// Rails' own is **anonymous** (`RouteSet#url_helpers` builds a `Module.new` and never names it),
/// so unlike `ActionMailer::MessageDelivery` there is no name to borrow, and this one is invented.
/// An application defining the constant itself meant something by it, and this pass then says
/// nothing: the `Comment::Relation` rule, applied to a module.
///
/// **The name is top-level, on purpose.** The collision a namespace would prevent does not happen
/// in practice, and there is no honest namespace to move it to (Rails' own is anonymous, so a
/// qualified name would either misattribute it to `ActionDispatch` or invent a namespace for one
/// constant). No reader sees it: [`SHOWN`] spells a route helper as the bare name it is called by.
///
/// **Unlike `MessageDelivery`, every route helper in it is mapped** (`main_app`, the one proxy
/// no file names, is not). The whole point is that `story_path`
/// jumps to `resources :stories`, so the safety argument cannot be "no mapping means no place"; it
/// has to be the reader's exactness, which is why [`routes`] is checked against the real router,
/// not a reading of it.
pub const ROUTE_HELPERS: &str = "RouteHelpers";

/// What a reader sees for each namespace this directory invents (`Knowledge::shown`).
///
/// - [`RELATION_BASE`] holds the query interface a relation answers, which Rails writes onto
///   `ActiveRecord::Relation` (`relation.rb`'s `include`s), so that is the class a card names.
/// - [`ROUTE_HELPERS`] stands for a module Rails never names, so a helper is shown alone, as it is
///   called: `story_path`, not a module a reader cannot find.
/// - [`HELPER_PROXY`](framework::HELPER_PROXY) is an instance of an unnamed
///   `Class.new(ActionView::Base)`, so the class it is shown as is the one it subclasses.
pub const SHOWN: [(&str, &str); 3] = [
    (RELATION_BASE, RAILS_RELATION[0]),
    (ROUTE_HELPERS, ""),
    (framework::HELPER_PROXY, framework::VIEW),
];

/// The framework's own half of a view context: the modules `ActionView::Base` includes.
///
/// Rails builds the class a template renders in from three includes,
/// `include Helpers, ::ERB::Util, Context` (`action_view/base.rb`), then layers the application's
/// `app/helpers` modules and the controller's `helper_method` proxies on top. The first two are
/// here; the third is left out (below). Nothing is generated: actionview is in the bundle and so
/// already in the graph, so this table only supplies the **root name** for `analysis::views` to
/// walk ancestors from, and rubydex's linearization does the rest. `ActionView::Helpers` `include`s
/// its helper modules at module-body level, so one name reaches them all, and an alias like `t`
/// comes along with its `translate`.
///
/// **Order matters, and it is Ruby's.** These are the *outermost* rungs: an application's `def tag`
/// in `ApplicationHelper` is included later and wins, which is why
/// [`analysis::views`](crate::analysis) reads this table last of its three halves.
///
/// **What is left out, and why.** Asked of the graph one module at a time, against a control class
/// that includes nothing:
///
/// | module | what it alone answers |
/// |---|---|
/// | `ActionView::Helpers` | nearly all bare template calls that were missing an answer |
/// | `ERB::Util` | `h` and `json_escape` |
/// | `ActionView::Context` | nothing |
///
/// `Context` is the renderer's plumbing (`output_buffer`, `view_flow`), and no template calls it
/// bare, so its row would cost a walk and answer nothing. **`ActionView::Base` itself is left out
/// too**, not for lack of reach: it answers exactly the same words, being these two modules plus
/// `Context`. What it would add is `Object` and `Kernel`, whose members would then arrive through
/// the view rung instead of the name rung in every template: far wider displacement for nothing.
pub const VIEW_CONTEXT: [&str; 2] = ["ActionView::Helpers", "ERB::Util"];

/// The two framework controllers a Rails application's own base inherits from.
///
/// Rails installs the route helpers with an `inherited` hook on both (`on_load(:action_controller)`
/// fires for `ActionController::Base` and `ActionController::API`), so every *subclass* gets them,
/// which makes one `include` per controller the framework's own behaviour, not an approximation.
const CONTROLLERS: [&str; 2] = ["ActionController::Base", "ActionController::API"];

/// What a Sidekiq worker includes, both spellings.
///
/// `Sidekiq::Job` is what 7.0 renamed `Sidekiq::Worker` to, and neither is going away, so
/// supporting only one would be a coin flip per repository: the same argument and answer as
/// `enum`'s two spellings. Sidekiq is not Rails but lives here anyway: this directory holds
/// *framework convention*, and `mod.rs` being the one list keeps that reviewable.
const WORKERS: [&str; 2] = ["Sidekiq::Worker", "Sidekiq::Job"];

// ---------------------------------------------------------------------------------------
// The one model several readers' tests ask the same questions of
//
// Here, not beside either reader, because a descendant module sees its ancestors' private items:
// `models::tests` and `relations::tests` both read this source, and two copies of "the same" model
// are how two files quietly stop asserting the same thing. A fixture with a single reader stays
// with that reader.
// ---------------------------------------------------------------------------------------

/// One model writing every association shape this directory reads, and a `scope`.
#[cfg(test)]
const MODEL: &str = "\
class Story < ApplicationRecord
  belongs_to :user
  belongs_to :parent_story, class_name: \"Story\", optional: true
  belongs_to :owner, polymorphic: true
  has_one :draft, class_name: \"Comment\"
  has_many :comments
  has_many :taggings
  has_many :tags, through: :taggings
  has_many :voters, through: :votes, source: :user
  scope :recent, -> { order(created_at: :desc) }
end
";

/// Every class [`MODEL`]'s application defines: what a macro's candidate list is read against.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn known() -> std::collections::BTreeSet<String> {
    ["Story", "User", "Comment", "Tag", "Tagging"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Every class [`MODEL`] gives a generated relation class to: its four collection elements, and
/// `Story` itself, which has one because it writes a `scope`.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn relation_classes() -> std::collections::BTreeSet<String> {
    ["Comment", "Tag", "Tagging", "Story"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    /// The tables above are one list written twice, and this is the seam between them.
    ///
    /// The five names [`MACROS`] has that neither [`ASSOCIATIONS`] nor [`LONG_TAIL`] has are
    /// spelled out here, not exempted, so a reader added without a table entry fails instead of
    /// quietly widening the exemption.
    ///
    /// The subtraction is the half that costs something: a [`LONG_TAIL`] family that declares
    /// nothing is **not** on [`MACROS`], because that list decides which documents the pass opens.
    /// A name slipping onto both would be a file read, parsed and discarded on every settle.
    #[test]
    fn every_macro_is_read_by_exactly_one_reader() {
        let mut all: Vec<&str> = ASSOCIATIONS.iter().map(|(name, _)| *name).collect();
        // Everything but the three that install **no method**. `Installs::Elsewhere` is not among
        // them: `helper_method` has a reader, just not in this file; see [`MACROS`], and the test
        // below for the distinction at work.
        all.extend(
            LONG_TAIL
                .iter()
                .filter(|(_, installs)| *installs != Installs::Nothing)
                .map(|(name, _)| *name),
        );
        all.push("enum");
        all.push("delegate");
        all.push("attribute");
        // The one [`MACROS`] name in neither table above: `helper` installs no member, so
        // [`LONG_TAIL`] has nothing to say about it, but the file must still be opened because
        // `analysis::views` reads what it says.
        all.push("helper");
        // And `layout` for the same reason: it names no member, and `analysis::views` reads it.
        all.push("layout");
        // And the callbacks, which `analysis::views` reads too.
        all.extend(callbacks::NAMES);
        all.sort_unstable();
        let mut macros = MACROS.to_vec();
        macros.sort_unstable();
        assert_eq!(macros, all, "a macro is on one list and not the other");
    }

    /// The four that declare nothing here, and which of the two reasons each has.
    ///
    /// `helper_method` is [`Installs::Elsewhere`] and the other three are [`Installs::Nothing`],
    /// and the distinction does real work: three define no method at all, and the fourth defines
    /// one on the **view context**. Only the first three are dead ends, which decides whether they
    /// may open a file: a document whose only macro is `normalizes` teaches nothing, while one
    /// whose only macro is `helper_method` tells `analysis::views` which controller methods a
    /// template may call.
    #[test]
    fn a_macro_that_declares_nothing_is_not_worth_opening_a_file_for() {
        let declines: Vec<&str> = LONG_TAIL
            .iter()
            .filter(|(_, installs)| installs.declines())
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            declines,
            [
                "encrypts",
                "generates_token_for",
                "helper_method",
                "normalizes"
            ]
        );
        let elsewhere: Vec<&str> = LONG_TAIL
            .iter()
            .filter(|(_, installs)| *installs == Installs::Elsewhere)
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            elsewhere,
            ["helper_method"],
            "the one that installs a method somewhere this table cannot declare on"
        );
        for name in declines {
            assert_eq!(
                MACROS.contains(&name),
                elsewhere.contains(&name),
                "{name} is on the document filter and has no reader, or the reverse"
            );
        }
    }

    /// No macro is in two families, which a hand-written list of twenty-nine could get wrong.
    #[test]
    fn no_macro_is_in_the_long_tail_twice() {
        let mut names: Vec<&str> = LONG_TAIL.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let held = names.len();
        names.dedup();
        assert_eq!(names.len(), held, "a macro is in `LONG_TAIL` twice");
    }

    /// The classes the generators ask the graph for are exactly the ones their rows name.
    ///
    /// [`framework_classes`] is read by `Context` and drives a lookup per pass; a class listed
    /// there that no row names would be a lookup for a gate nothing consults, and one a row names
    /// but not there would be a row that silently never declares. The first three are
    /// [`LONG_TAIL`]'s, and the last is what a bare `where` returns.
    #[test]
    fn every_gem_class_the_table_names_is_asked_for() {
        assert_eq!(
            framework_classes(),
            [
                "ActiveStorage::Attached::One",
                "ActiveStorage::Attached::Many",
                "ActionText::RichText",
                WHERE_CHAIN,
                PROMISE,
                COLLECTION_PROXY,
                ROUTES_PROXY,
            ]
        );
    }

    #[test]
    fn a_nested_class_claims_a_table_only_when_it_is_a_model() {
        // "A nested class claims nothing" is too coarse; the rule is "it claims what Rails says it
        // claims", and the superclass is the whole gate. A `class Story` inside `module Legacy`
        // that is not an ActiveRecord model must still claim nothing, or the schema's columns land
        // on a service object that never reads them. Real projects have such classes whose names
        // inflect onto real tables.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let nested = harness.write(
            "app/models/admin/story.rb",
            // The second is two hops from the base, not one: the engine shape, and why `is_model`
            // climbs instead of asking once.
            "module Admin\n  class Story < ApplicationRecord\n  end\nend\n\n\
             class Base < ApplicationRecord\nend\n\n\
             module Legacy\n  class Story < Base\n  end\nend\n\n\
             module Service\n  class Story\n  end\nend\n",
        );
        // And the spelling of "top level" that says so outright.
        let widget = harness.write("app/models/widget.rb", "class ::Widget\nend\n");
        harness.watch(&[&nested, &widget]);

        assert!(harness.has("Story#title()"));
        assert!(harness.has("Widget#name()"));
        // `Admin` declares no prefix, so `compute_table_name` is the bare plural and all three
        // models really read `stories`; a one-claimant rule would let only the top-level one
        // answer.
        assert!(harness.has("Admin::Story#title()"));
        assert!(harness.has("Legacy::Story#title()"));
        // The one that inherits nothing is not a model, and claims nothing.
        assert!(!harness.has("Service::Story#title()"));
    }

    #[test]
    fn a_namespace_that_declares_a_prefix_moves_every_table_under_it() {
        // `full_table_name_prefix` is `module_parents.detect { |p| p.respond_to?(...) }`, so a
        // `def self.table_name_prefix` on `Admin` means every model under it reads a table starting
        // `admin_`. It is read from a file, which is why the inflection happens where the documents
        // are, not where the definitions are.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"admin_stories\", force: :cascade do |t|\n    \
             t.string \"headline\", null: false\n  end\nend\n",
        );
        let admin = harness.write(
            "app/models/admin.rb",
            "module Admin\n  def self.table_name_prefix\n    \"admin_\"\n  end\nend\n",
        );
        let nested = harness.write(
            "app/models/admin/story.rb",
            "module Admin\n  class Story < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&schema, &admin, &nested]);

        assert!(harness.has("Admin::Story#headline()"));
        // And the bare plural is not also its: `stories` is not in this schema, but claiming it
        // would still be wrong.
        assert!(!harness.has("Admin::Story#title()"));
    }

    #[test]
    fn a_namespace_that_declares_a_suffix_moves_every_table_under_it_too() {
        // `full_table_name_suffix` is the same `module_parents.detect`, and rarely used. It is read
        // anyway because it is the same syntax in the same walk, and ignoring it is the only way
        // this reader could name a table that exists but is not the one the class reads.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"stories_v2\", force: :cascade do |t|\n    \
             t.string \"headline\", null: false\n  end\nend\n",
        );
        let legacy = harness.write(
            "app/models/legacy.rb",
            "module Legacy\n  def self.table_name_suffix\n    \"_v2\"\n  end\nend\n",
        );
        let nested = harness.write(
            "app/models/legacy/story.rb",
            "module Legacy\n  class Story < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&schema, &legacy, &nested]);

        assert!(harness.has("Legacy::Story#headline()"));
    }

    #[test]
    fn an_engine_that_isolates_a_namespace_declares_the_same_prefix() {
        // The commoner of the two spellings, and a call rather than a `def`, because the engine
        // says it about a module someone else wrote. `Rails::Engine#isolate_namespace` installs
        // `table_name_prefix` as `generate_railtie_name(mod.name)` plus an underscore.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"spree_orders\", force: :cascade do |t|\n    \
             t.string \"number\", null: false\n  end\nend\n",
        );
        let engine = harness.write(
            "lib/spree/core/engine.rb",
            "module Spree\n  module Core\n    class Engine < ::Rails::Engine\n      \
             isolate_namespace Spree\n    end\n  end\nend\n",
        );
        let order = harness.write(
            "app/models/spree/order.rb",
            "module Spree\n  class Order < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&schema, &engine, &order]);

        assert!(harness.has("Spree::Order#number()"));
    }

    #[test]
    fn a_class_nested_inside_a_model_claims_nothing() {
        // `compute_table_name`'s other branch is `parent_singular_child_plural`, which needs the
        // parent's own table and then its parent's. Declined, not approximated: in practice,
        // classes nested inside a model do not name such tables.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let nested = harness.write(
            "app/models/widget.rb",
            "class Widget < ApplicationRecord\n  class Story < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&nested]);

        assert!(harness.has("Widget#name()"));
        assert!(!harness.has("Widget::Story#title()"));
    }

    #[test]
    fn a_nested_model_whose_namespace_nothing_declares_claims_nothing() {
        // The namespace rule, reached by a second generator. A generated `class Reports::Metric`
        // where nothing declares `Reports` silently costs that namespace its own members, so the
        // claim is declined instead of spelled. It rarely fires, and is here because the damage it
        // prevents is invisible.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        // `class Reports::Story` with no `module Reports` anywhere, **and no `reports/`
        // directory**, which makes the namespace unreachable, not just unwritten. A file inside one
        // is the other test below.
        let unreachable = harness.write(
            "app/models/story_reports.rb",
            "class Reports::Story < ApplicationRecord\nend\n",
        );
        harness.watch(&[&unreachable]);

        assert!(harness.has("Story#title()"));
        assert!(!harness.has("Reports::Story#title()"));
    }

    /// The same class one directory over, where Rails' own autoloader declares the namespace.
    ///
    /// Zeitwerk loads `app/models/reports/story.rb` by defining `Reports` first (a directory under
    /// an autoload root with no `reports.rb` beside it **is** the declaration), so the namespace is
    /// spellable and the claim stands. The two tests share class and superclass and differ only in
    /// the file's directory, which is the whole rule.
    #[test]
    fn a_nested_model_the_directory_declares_a_namespace_for_claims_its_table() {
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let conjured = harness.write(
            "app/models/reports/story.rb",
            "class Reports::Story < ApplicationRecord\nend\n",
        );
        harness.watch(&[&conjured]);

        assert!(harness.has("Reports::Story#title()"));
    }

    #[test]
    fn a_class_whose_superclass_is_nobody_the_workspace_defines_is_not_a_model() {
        // The other end of the same walk: a chain that runs out is not a model, and a chain that
        // loops is not an infinite loop. Neither can claim a table.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let odd = harness.write(
            "app/models/odd.rb",
            "module Legacy\n  class Story < Sinatra::Base\n  end\nend\n\n\
             module Circular\n  class Story < Other\n  end\n\n  class Other < Story\n  \
             end\nend\n",
        );
        harness.watch(&[&odd]);

        assert!(harness.has("Story#title()"));
        assert!(!harness.has("Legacy::Story#title()"));
        assert!(!harness.has("Circular::Story#title()"));
    }

    #[test]
    fn a_written_table_name_meets_the_class_whose_name_implies_it() {
        // The one place an inflected claim meets a written one, and real code has both readings. A
        // throwaway `MoveUserSettings::LegacySetting` says `self.table_name = "settings"`, and if a
        // written name simply replaced an inflected one, it would *take* those columns from the
        // `Setting` model that reads them; test doubles do the same to `Post`. But `TopicViewItem`
        // says `topic_views`, and the `TopicView` its name implies is a plain view object that
        // reads no table.
        //
        // So the guess survives only when its class is a model.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let model = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // The other half of the fixture: a top-level class named after a table it does not read.
        let widget = harness.write("app/models/widget.rb", "class Widget\nend\n");
        let migration = harness.write(
            "db/migrate/20240101000000_backfill.rb",
            "class Backfill < ActiveRecord::Migration[7.1]\n  \
             class LegacyStory < ApplicationRecord\n    self.table_name = \"stories\"\n  \
             end\n\n  class WidgetRow < ApplicationRecord\n    \
             self.table_name = \"widgets\"\n  end\nend\n",
        );
        harness.watch(&[&model, &widget, &migration]);

        assert!(harness.has("Story#title()"), "the model lost its own table");
        assert!(harness.has("Backfill::LegacyStory#title()"));
        // And the class that is not a model does not keep a table someone else named.
        assert!(harness.has("Backfill::WidgetRow#name()"));
        assert!(!harness.has("Widget#name()"));
    }

    #[test]
    fn an_anonymous_class_is_not_a_model_however_it_is_written() {
        // A defect only real projects reveal, reachable from two generators. rubydex names an
        // anonymous `Class.new(ApplicationRecord)` `<hash>:<offset><anonymous>`; that class is an
        // ActiveRecord model by every rule here, so it asks for a relation, and `class ` + that
        // name is not RBS, so `Synthesized::record`'s parse gate would throw away **the whole
        // document**, with every real declaration in the file. Engine specs write this shape.
        //
        // The assertion is on a *neighbour*: the document survived if the class written beside the
        // anonymous one still declares its members.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "spec/models/thing_spec.rb",
            "class Thing < ApplicationRecord\n  has_many :parts\nend\n\n\
             thrown = -> { Class.new(ApplicationRecord) { def spin; end } }\n",
        );
        harness.write(
            "app/models/part.rb",
            "class Part < ApplicationRecord\nend\n",
        );
        harness.index();

        assert!(
            harness.has("Thing#parts()"),
            "the macro beside the anonymous class still declares"
        );
        assert!(
            harness.has("Thing::Relation"),
            "and so does the relation the same document writes"
        );
    }

    #[test]
    fn a_model_reopened_to_nest_something_under_it_keeps_its_table() {
        // `claims` is filled per *definition*, so a model reopened in a second file pushes its own
        // name twice, and "a table two classes claim is claimed by neither" would then decline it,
        // losing every column of a model nothing is ambiguous about. The shape is the ordinary Ruby
        // idiom for namespacing a helper under a model: `class AuditLog` again in
        // `app/queries/audit_log/unpublish_alls_query.rb`, or
        // `class Reviewable < ActiveRecord::Base` in several `lib/reviewable/` files. None of these
        // is a collision.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let query = harness.write(
            "app/queries/story/recent_query.rb",
            "class Story\n  class RecentQuery\n  end\nend\n",
        );
        harness.watch(&[&query]);

        assert!(harness.has("Story#title()"));
    }

    #[test]
    fn two_nested_names_that_reach_one_table_claim_neither() {
        // The narrowed ambiguity rule in the case it was built for: several claimants are kept when
        // they demodulize alike (one convention applied twice), while two *different* names on one
        // table means the inflector got one wrong.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let rivals = harness.write(
            "app/models/rivals.rb",
            "module Legacy\n  class Storie < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&rivals]);

        assert!(!harness.has("Story#title()"), "an ambiguous table answered");
        assert!(!harness.has("Legacy::Storie#title()"));
    }

    #[test]
    fn two_classes_that_pluralize_to_one_table_claim_neither() {
        // An ambiguous answer is not an answer. Both classes are top level, both the user's own,
        // both name `stories`, so neither gets the columns.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let rival = harness.write("app/models/storie.rb", "class Storie\nend\n");
        harness.watch(&[&rival]);

        assert!(!harness.has("Story#title()"), "an ambiguous table answered");
        assert!(!harness.has("Storie#title()"));
    }

    #[test]
    fn a_model_that_names_its_own_table_reads_that_one_and_not_the_other() {
        // The documented escape, and the half that is easy to get wrong: a class that says
        // `self.table_name` must *stop* claiming the table its name implies, or one model answers
        // with two schemas at once.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let model = harness.write(
            "app/models/story.rb",
            "class Story\n  self.table_name = \"widgets\"\nend\n",
        );
        harness.watch(&[&model]);

        assert!(harness.has("Story#name()"), "the named table was not read");
        assert!(
            !harness.has("Story#title()"),
            "and the table its name implies was read as well"
        );
    }
}
