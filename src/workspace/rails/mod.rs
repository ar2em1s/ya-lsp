//! Everything ya-lsp knows about Rails, and nothing else in the crate knows any of it.
//!
//! One directory rather than one file, to protect one review property: **"how much framework is
//! in here" is answered by reading one list**, and that list is this file. The convention tables
//! are `const` arrays below, `mod.rs` is the only public surface, and every submodule is private.
//!
//! # The twelve modules
//!
//! - [`conventions`] — the two rules that are about a **path**: which controller renders a
//!   template, and which files `rails db:migrate` dumps a schema into. Neither opens a file.
//! - [`inflect`] — Rails' inflector, minus everything this corpus did not need.
//! - [`syntax`] — the half-dozen Prism shapes the readers start from.
//! - [`schema`] — `db/*schema.rb`: what tables exist and what their columns return.
//! - [`structure`] — `db/*structure.sql`: the same tables, out of the dump the database's own
//!   tool wrote. The one reader here that is not Ruby, and the only one with no Prism in it.
//! - [`attributes`] — `attribute`, the cast type it may be told, and the three different features
//!   that spell their macro that way.
//! - [`models`] — the association macros, the monomorphic relation class, and the query interface
//!   the relation and the model's own singleton share.
//! - [`delegates`] — `delegate`, and the two hops between a name written here and a type written
//!   somewhere else. The only reader whose answer needs the *other* generators' output, which is
//!   why it is a second phase rather than a second macro in [`models`].
//! - [`enums`] — `enum`, both spellings, and the 3 + 4N names one call installs.
//! - [`tail`] — every remaining macro name that names a member, as one table: where each takes
//!   its names from, what it installs around them, how each is typed, and the four that are read
//!   and install nothing.
//! - [`entrypoints`] — the two conventions with no macro at all: a mailer's actions and a job's
//!   `perform`, recognised by a superclass or a mixin and read out of a `def`.
//! - [`routes`] — `config/routes.rb`: which helpers Rails names, the one module they go in, and
//!   every class that `include`s it.
//!
//! # What every one of them has in common
//!
//! **Pure text in, text out, no I/O and no graph.** A generator here is handed a `&str` and
//! answers with [`Facts`](crate::generated::Facts); which file that text came out of, which
//! classes the application defines and which of two schemas to believe are all the caller's, and
//! the caller is [`analysis::synthesize`](crate::analysis). That is what makes this directory
//! testable by reading, and why it carries a 100% coverage floor: a convention that reaches the
//! wrong class answers confidently and wrongly about the file the user is looking at, and one
//! that reaches nothing looks exactly like a project that does not follow it.

mod attributes;
mod conventions;
mod delegates;
mod entrypoints;
mod enums;
mod inflect;
mod models;
mod routes;
mod schema;
mod structure;
mod syntax;
mod tail;

pub use conventions::{controller_of, is_helper, is_routes, is_schema, is_structure, mailer_of};
pub use entrypoints::{Entrypoints, MESSAGE_DELIVERY, convention_of, is_mailer, read_entrypoints};
pub use inflect::{camelize, helper_module, table_of};
pub use models::{
    Elsewhere, Model, RECORD_BASE, RELATION_BASE, element_of, is_record_base, read_model,
    relation_base, relation_of,
};
pub use routes::{Routes, Whose, hosts_routes, mixins, read_routes};
pub use schema::{Schema, TableNames, engine_prefix, read_schema, read_table_names};
pub use structure::read_structure;
pub use syntax::candidates;

use entrypoints::Convention;
use models::Kind;
use tail::Installs;

/// The ten column types lobsters' 38 tables are made of, and the Ruby class each returns.
///
/// Ten entries and not a subsystem, measured rather than assumed. Two of the ten are worth
/// their own sentence:
///
/// - **`datetime` is `Time`, not `ActiveSupport::TimeWithZone`.** Rails returns the latter when
///   `time_zone_aware_attributes` is on, which is the default and which no file states — so the
///   honest common denominator is the class the other one delegates to. Every answer derived
///   through `Time` is correct for both, and `Time` is in Ruby's own signatures, so it is in the
///   graph of a project that has no Rails in its bundle at all.
/// - **`binary` is `String`**, because that is what ActiveRecord hands back: bytes in a
///   `String`, not an IO.
///
/// Anything else — `jsonb`, `uuid`, `inet`, a type this corpus did not contain — is declared
/// `untyped` rather than skipped — the difference between a member that exists with no type and
/// no member at all, and what the schema is measured by is member lookups that would otherwise
/// fail. A mapping is a claim; `untyped` is the absence of one.
const COLUMN_TYPES: [(&str, &str); 10] = [
    ("string", "String"),
    ("text", "String"),
    ("binary", "String"),
    ("integer", "Integer"),
    ("bigint", "Integer"),
    ("boolean", "bool"),
    ("float", "Float"),
    ("decimal", "BigDecimal"),
    ("datetime", "Time"),
    ("date", "Date"),
];

/// What the schema dumper writes inside a `create_table` block that is not a column.
///
/// Everything else *is* one: the measured rule is "a call on the block parameter whose first
/// argument is a string literal, except `index`", and this list is that exception
/// plus the four constraint macros, which take a string first argument too and would otherwise
/// declare a member named after a table.
const NOT_COLUMNS: [&str; 5] = [
    "index",
    "foreign_key",
    "check_constraint",
    "exclusion_constraint",
    "unique_constraint",
];

/// The column `create_table` declares without being asked, and its type when nothing says.
const PRIMARY_KEY: (&str, &str) = ("id", "bigint");

/// Words Rails' inflector will not pluralize, and the ones it pluralizes by replacement.
///
/// Both tables are deliberately short. A word this does not know pluralizes to something no
/// table is called, the class matches nothing, and the answer is **nothing** — which is the
/// failure direction the whole item is built to have, and the reason the class→table direction
/// was chosen over singularizing table names.
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
/// The list is measured rather than chosen, and over six applications rather than one — a single
/// corpus measures a corpus and states a framework. Two shapes of evidence set it. `validates` is
/// the most common macro in a real Rails model and still declares nothing an editor can use, so it
/// is absent. `has_and_belongs_to_many` reads zero everywhere, which is *not* evidence: every
/// corpus runs `rubocop-rails`, whose `Rails/HasAndBelongsToMany` is on by default, so the
/// construct is linted away rather than declined. A zero may exclude a name only when the name
/// also costs something, and this one costs a row — Rails implements the macro by calling
/// `has_many`.
///
/// `delegate` is the only entry whose reader runs in a second phase: what it declares is read in
/// the same walk as the rest, and what it *returns* is a fact some other file's generator writes
/// in this same pass — see [`delegates`].
///
/// `attribute` is the only entry whose commonest caller is **not Rails**: in most applications the
/// majority of `attribute` calls are `active_model_serializers`' macro of the same name, which
/// defines the same member and takes an options hash where Rails takes a cast type. That is why it
/// is on this list rather than gated on a superclass — see [`attributes`].
///
/// Most of the rest are [`LONG_TAIL`]'s, one table rather than a reader each because every one
/// installs a fixed set of members named around the names the call was given. **Three of that
/// table's names are deliberately not here**: `encrypts`, `generates_token_for` and `normalizes`
/// install no method at all, so a file whose only macro is one of them declares nothing — and this
/// list is what decides which documents `synthesize` opens and parses.
///
/// `helper_method` and `helper` are here for a reason no other entry has: neither declares
/// anything in RBS and neither ever will, and the file still has to be opened, because what they
/// say is read by [`Model::exports`] and [`Model::helper_modules`] and answered by
/// `analysis::views` rather than by a generator. Listing them costs little — `Wants::calls` opens
/// a document that **calls** one of these names, and few files in an application write either.
///
/// Only `helper_method` is in [`LONG_TAIL`], and the asymmetry is the point: it really does define
/// a method — `def current_user(...)` on `_helpers` — and so belongs in a table of "every
/// remaining macro that names a member", declining there because there is nowhere to put it.
/// `helper` names no member at all; it `include`s a module into the view context, which this crate
/// models and never declares.
pub const MACROS: [&str; 35] = [
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
];

/// Every remaining macro that names a member, and which family it is in.
///
/// What each family installs is [`tail::row`], one function with every family's members side by
/// side, and each of them was read out of Rails at `7ba5fa3` rather than remembered.
///
/// **Four of them declare nothing and are on the list anyway, for two different reasons.**
/// `normalizes` decorates an attribute's type, and `encrypts` and `generates_token_for` install
/// their pairs once in a framework module rather than per call: none of the three defines a
/// method per call, so there is nothing to declare. `helper_method` **does** define one —
/// `def current_user(...)` on the controller's `_helpers` module — and installs it on the
/// **view context**. [`Installs`] keeps the two apart because only the first is a dead end: the
/// view context is modelled in `analysis::views`, and the macro's names are read by [`models`]
/// into [`Model::exports`]
/// rather than declared here. It stays [`Installs::Elsewhere`] — nothing in this table has
/// anything to say about it — and it is on [`MACROS`], which is the one entry where those two
/// facts differ. A macro *absent* from this table looks exactly
/// like one nobody thought about, so each of the four is read and declined, and [`tail`]'s
/// docstring says which sentence of Rails settles it.
///
/// The twelve `mattr_`/`cattr_` spellings are one family with four rows apiece and are not
/// filtered by their corpus counts, for [`MACROS`]' own reason: `mattr_accessor` is 23 calls and
/// `thread_cattr_writer` is zero, and the zero costs a row.
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

/// Every class a macro in [`LONG_TAIL`] names that a **gem** declares, not this application.
///
/// Three, and they are the whole of what [`LONG_TAIL`] needs from a bundle. `Context` looks each of
/// them up in the graph once per pass, and a workspace whose bundle has no Active Storage
/// declares no `has_one_attached` at all rather than one typed as a class nothing can reach.
#[must_use]
pub fn framework_classes() -> Vec<&'static str> {
    LONG_TAIL
        .iter()
        .filter_map(|(_, installs)| tail::gem_class(*installs))
        .collect()
}

/// The five of them that name another class, and what each returns.
///
/// Kept beside [`MACROS`] rather than derived from it, so that a name added to one and
/// forgotten in the other is a test failure rather than a file that is read and then declares
/// nothing. Three entries of [`MACROS`] are not here. `enum` names a column's values rather than
/// a class, so [`enums`] reads it and this table would have nothing to say about it; `delegate`
/// names a *method* and the class is two hops away, so [`delegates`] reads it in a second phase
/// and its `to:` is not a class name at all; and `attribute` names a **cast type** rather than a
/// class — the ten it may name are [`COLUMN_TYPES`], which is the registry a migration writes
/// into and not a constant any application defines.
const ASSOCIATIONS: [(&str, Kind); 5] = [
    ("belongs_to", Kind::One),
    ("has_one", Kind::Maybe),
    ("has_many", Kind::Many),
    // Rails' own last line of the macro is `has_many name, scope, **hm_options, &extension`, so
    // declaring it a collection is not an approximation of the framework's behaviour, it *is*
    // the framework's behaviour: `class_name:` is forwarded, and so is the element type its own
    // name implies.
    ("has_and_belongs_to_many", Kind::Many),
    ("scope", Kind::Scope),
];

/// A superclass whose name *ends* with one of these makes the class a mailer, a job or a
/// worker, and the `def`s in its body are read.
///
/// The suffix and not the class's own name, which the corpus settles rather than taste: three
/// of chatwoot's migrations are named `EnqueueValidateOpenaiHooksJob` and inherit
/// `ActiveRecord::Migration[7.1]`, so a rule keyed on the class's own spelling would have
/// declared `perform_later` on a migration. The superclass is also what carries the framework
/// in: `ApplicationJob`, `ApplicationMailer`, `Spree::BaseMailer`, `Devise::Mailer` and
/// mastodon's `ActivityPub::DeliveryWorker` are all somebody's own base class, and each of them
/// ends in the word for what it is.
///
/// `Worker` is here because Sidekiq is: 27 classes in the corpus subclass a worker without
/// including `Sidekiq::Worker` themselves, all 27 in the two applications that use Sidekiq at
/// all, and none in the four that do not. The gate is still `def perform` — a class that
/// inherits one of these and defines nothing declares nothing.
const INHERITS: [(&str, Convention); 3] = [
    ("Mailer", Convention::Mailer),
    ("Job", Convention::Job),
    ("Worker", Convention::Worker),
];

/// The two framework base classes, which end in neither of [`INHERITS`]' words.
///
/// Not the same rule spelled twice: `ApplicationMailer < ActionMailer::Base` is the base every
/// Rails application generates, and a suffix rule alone misses every class that subclasses the
/// framework directly — **19 classes**, of which discourse's 13 mailers are the whole of that
/// application's mailer half. An exact match rather than a suffix,
/// because `Base` is a word half of Rails ends in.
const BASES: [(&str, Convention); 2] = [
    ("ActionMailer::Base", Convention::Mailer),
    ("ActiveJob::Base", Convention::Job),
];

/// The module ya-lsp writes the route helpers into.
///
/// Rails' own is **anonymous** — `RouteSet#url_helpers` builds a `Module.new` and never names
/// it — so unlike `ActionMailer::MessageDelivery` there is no name to borrow, and this one is
/// invented. An application that has defined the constant itself is one that meant something by
/// it, and this pass then says nothing at all, which is the rule `Comment::Relation` follows
/// applied to a module. It is declared nowhere in the six corpora.
///
/// **The name is top-level, and the original reason for that has been retired.** On rubydex
/// 0.2.5 [`Facts`](crate::generated::Facts) could spell
/// `module A::B::C`, rubydex would index the module under exactly that name, and an
/// `include A::B::C` written in RBS then did not reach it — the mixin was recorded as a
/// `<partial>` ancestor and `find_member_in_ancestors` walked past the host as though it were
/// not there. `ActionDispatch::Routing::RouteHelpers` was the first spelling of this constant
/// and it silently bought nothing. Upstream linearizes it: the same fixture now reads
/// `[StoriesController, App::Web::Helpers, Object, Kernel, BasicObject]`.
///
/// The name stays top-level anyway, and the argument is no longer rubydex's. Every route helper's
/// hover card prints its owner, so moving the constant renames what a user reads at 9,479 call
/// sites; the collision it would guard against is measured at **0 in six corpora**; and there is
/// no honest namespace to move it to — Rails' own is anonymous, so a qualified spelling would
/// have to be either a lie about `ActionDispatch` or a namespace invented for one constant.
///
/// **Unlike `MessageDelivery`, everything in it is mapped.** The whole point is that
/// `story_path` jumps to `resources :stories`, so the safety argument cannot be "no mapping
/// means no place" and has to be the reader's exactness instead — which is why
/// [`routes`] is checked against the real router rather than against a reading of it.
pub const ROUTE_HELPERS: &str = "RouteHelpers";

/// The two framework controllers a Rails application's own base inherits from.
///
/// Rails installs the route helpers with an `inherited` hook on both — `on_load(:action_controller)`
/// fires for `ActionController::Base` and for `ActionController::API` — so the class that gets
/// them is every *subclass*, which is what makes writing one `include` per controller the
/// framework's own behaviour rather than an approximation of it.
const CONTROLLERS: [&str; 2] = ["ActionController::Base", "ActionController::API"];

/// What a Sidekiq worker includes, both spellings.
///
/// `Sidekiq::Job` is what 7.0 renamed `Sidekiq::Worker` to and neither is going away, so
/// supporting one is a coin flip per repository — the same argument `enum`'s two spellings get
/// and the same answer. Sidekiq is not Rails and is in this directory anyway:
/// what the directory holds is *framework convention*, and `mod.rs` being the one list is what
/// keeps that reviewable.
const WORKERS: [&str; 2] = ["Sidekiq::Worker", "Sidekiq::Job"];

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// The tables above are one list written twice, and this is the seam between them.
    ///
    /// The three names [`MACROS`] carries that neither [`ASSOCIATIONS`] nor [`LONG_TAIL`] does
    /// are spelled out here rather than exempted, so that a reader added without a table entry
    /// fails instead of quietly widening the exemption.
    ///
    /// The subtraction is the other half and it is the one that costs something: a
    /// [`LONG_TAIL`] family that declares nothing is **not** on [`MACROS`], because that list
    /// decides which documents the pass opens. A name that slipped onto both would be a file
    /// read, parsed and thrown away once per settle.
    #[test]
    fn every_macro_is_read_by_exactly_one_reader() {
        let mut all: Vec<&str> = ASSOCIATIONS.iter().map(|(name, _)| *name).collect();
        // Everything but the three that install **no method at all**. `Installs::Elsewhere` is
        // not among them: `helper_method` has a reader, it is just not one in this file — see
        // [`MACROS`], and the test below for the distinction that is doing the work here.
        all.extend(
            LONG_TAIL
                .iter()
                .filter(|(_, installs)| *installs != Installs::Nothing)
                .map(|(name, _)| *name),
        );
        all.push("enum");
        all.push("delegate");
        all.push("attribute");
        // The one name on [`MACROS`] that is in neither table above: `helper`
        // installs no member, so [`LONG_TAIL`] would have nothing to say about it, and the file
        // still has to be opened because `analysis::views` reads what it says.
        all.push("helper");
        all.sort_unstable();
        let mut macros = MACROS.to_vec();
        macros.sort_unstable();
        assert_eq!(macros, all, "a macro is on one list and not the other");
    }

    /// The four that declare nothing here — and which of the two reasons each has.
    ///
    /// `helper_method` is [`Installs::Elsewhere`] and the other three are [`Installs::Nothing`],
    /// and the distinction is load-bearing rather than descriptive: three of them define no
    /// method at all, and the fourth defines one on the **view context**. Only the first three
    /// are a dead end, and that is exactly what decides which of them may open a file: a
    /// document whose only macro is a `normalizes` is a document nothing would learn anything
    /// from, and one whose only macro is a `helper_method` tells `analysis::views` which of a
    /// controller's methods a template may call.
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

    /// No macro is in two families, which a list of twenty-nine written by hand can be.
    #[test]
    fn no_macro_is_in_the_long_tail_twice() {
        let mut names: Vec<&str> = LONG_TAIL.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let held = names.len();
        names.dedup();
        assert_eq!(names.len(), held, "a macro is in `LONG_TAIL` twice");
    }

    /// The classes [`LONG_TAIL`] asks the graph for are the ones its own table names, and no
    /// others.
    ///
    /// [`framework_classes`] is read by `Context` and drives a lookup per pass; a class listed
    /// there and not in [`LONG_TAIL`] would be a lookup for a gate nothing consults, and one in
    /// [`LONG_TAIL`] and not there would be a macro that silently never declares.
    #[test]
    fn every_gem_class_the_table_names_is_asked_for() {
        assert_eq!(
            framework_classes(),
            [
                "ActiveStorage::Attached::One",
                "ActiveStorage::Attached::Many",
                "ActionText::RichText",
            ]
        );
    }
}
