//! Everything ya-lsp knows about Rails, and nothing else in the crate knows any of it.
//!
//! One directory rather than one file, to protect one review property: **"how much framework is
//! in here" is answered by reading one list**, and that list is this file. The convention tables
//! are `const` arrays below, `mod.rs` is the only public surface, and every submodule is private.
//!
//! # The sixteen modules
//!
//! - [`conventions`] — the rules that are about a **path** and nothing else: which controller
//!   renders a template, which class a mailer's views hang off, which files are a helper Rails
//!   globs, and which files `rails db:migrate` dumps a schema into. None opens a file.
//! - [`inflect`] — Rails' inflector, minus everything this corpus did not need.
//! - [`syntax`] — the half-dozen Prism shapes the readers start from.
//! - [`schema`] — `db/*schema.rb`: what tables exist and what their columns return.
//! - [`structure`] — `db/*structure.sql`: the same tables, out of the dump the database's own
//!   tool wrote. The one reader here that is not Ruby, and the only one with no Prism in it.
//! - [`attributes`] — `attribute`, the cast type it may be told, and the three different features
//!   that spell their macro that way.
//! - [`models`] — one model file: which bodies it holds, which of them may declare at all, and
//!   the order the families come back together in.
//! - [`associations`] — `belongs_to`, `has_one`, `has_many`, `has_and_belongs_to_many` and
//!   `scope`: every member one line installs, and the four declined.
//! - [`relations`] — the relation class every model gets, the two classes a scope goes on, and
//!   the query interface the relation and the model's own singleton share.
//! - [`concerns`] — a concern body's macros, and which including classes they land on.
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
//! - [`framework`] — what the framework's own singletons return, the four rows declined and the
//!   measurement that declined them. The one generator whose input is almost nothing: it reads
//!   a class name out of `config/application.rb` and takes the rest from a table.
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

mod associations;
mod attributes;
mod concerns;
mod conventions;
mod delegates;
mod entrypoints;
mod enums;
mod framework;
mod inflect;
mod models;
mod relations;
mod routes;
mod schema;
mod structure;
mod syntax;
mod tail;

pub use concerns::{
    CLASS_METHODS, ClassMethod as ConcernMethod, From as ConcernSource,
    declare as declare_concern_members, installed as concern_members,
};
pub use conventions::{
    autoloaded_namespaces, confirmed_spelling, controller_of, is_helper, is_routes, is_schema,
    is_structure, mailer_of,
};
pub use entrypoints::{Entrypoints, MESSAGE_DELIVERY, convention_of, is_mailer, read_entrypoints};
pub use framework::{application_class, read_framework, singleton_classes};
pub use inflect::{camelize, helper_module, table_of};
pub use models::{Elsewhere, Model, RECORD_BASE, is_record_base, read_model};
pub use relations::{
    RAILS_CLASS_SIDE, RAILS_RELATION, RELATION_BASE, element_of, relation_base, relation_of,
};
pub use routes::{Routes, Whose, hosts_routes, mixins, read_routes};
pub use schema::{Schema, TableNames, engine_prefix, read_schema, read_table_names};
pub use structure::read_structure;

use associations::Kind;
use entrypoints::Convention;
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
/// failure direction this is built to have, and the reason the class→table direction
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

/// Every receiverless name that puts a document in front of [`models::read_model`].
///
/// [`MACROS`] **plus `class_methods` and `included`**, and neither extra name is a macro — which
/// is why they are here rather than on that list. `class_methods` declares no member of the body
/// it is written in: `ActiveSupport::Concern` evaluates its block on a nested `ClassMethods`
/// module, and the `def`s inside it are the declaration. `included` declares none either, and it
/// is on this list for a narrower reason still — a bare `extend M` written in one puts `M`'s
/// instance methods on every including class's singleton, and a concern that writes nothing else
/// at all would otherwise be on no list. `activemodel/lib/active_model/api.rb` is exactly that
/// file, and it is what installs `model_name` and `human_attribute_name` on every Rails model.
/// See [`concerns`].
///
/// **Derived from [`MACROS`] rather than written out beside it**, unlike [`LONG_TAIL`]: there
/// the two lists are the same *kind* of thing and a name in one and not the other is a bug worth
/// a test, and here one list is the other plus a name that is deliberately not a macro.
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

/// The framework's own half of a view context: the modules `ActionView::Base` includes.
///
/// Rails builds the class a template renders in out of three includes — `include Helpers,
/// ::ERB::Util, Context` (`action_view/base.rb`) — and then layers the application's own
/// `app/helpers` modules and the controller's `helper_method` proxies over them. The first two
/// are here; the third is declined below. Nothing is generated: actionview is in the bundle and
/// therefore already in the graph, so all this table supplies is the **name of the root** for
/// `analysis::views` to walk ancestors from, and rubydex's own linearization does the rest —
/// `ActionView::Helpers` `include`s its 24 helper modules at module-body level, so one name
/// reaches every one of them and an alias like `t` comes along with its `translate`.
///
/// **Order matters and is Ruby's.** These are the *outermost* rungs of the chain: an
/// application's `def tag` in `ApplicationHelper` is included later and therefore wins, which is
/// why [`analysis::views`](crate::analysis) reads this table last of its three halves.
///
/// **What is declined, and the measurement that declined it.** Over 8,732 bare-word call sites
/// in the six corpora's templates and `app/helpers` files, 5,104 answered with a candidate list
/// or with nothing. Asked of the graph, one module at a time, against a control class that
/// includes nothing:
///
/// | module | words it alone answers | positions |
/// |---|---|---|
/// | `ActionView::Helpers` | 27 | 4,639 |
/// | `ERB::Util` | 2 (`h`, `json_escape`) | 38 |
/// | `ActionView::Context` | **0** | 0 |
///
/// `Context` is the renderer's own plumbing — `output_buffer`, `view_flow` — and no template in
/// six corpora calls any of it bare, so it is a row that would cost a walk and answer nothing.
/// **`ActionView::Base` itself is declined too**, and not for lack of reach: it answers exactly
/// the same 29 words, because it is these two modules plus `Context`. What it would add is
/// `Object` and `Kernel`, whose members would then arrive through the view rung rather than the
/// name rung at every template in the project — a far wider displacement bought for nothing.
pub const VIEW_CONTEXT: [&str; 2] = ["ActionView::Helpers", "ERB::Util"];

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

// ---------------------------------------------------------------------------------------
// The one model more than one reader's tests ask the same questions of
//
// Here rather than beside either of them because a descendant sees its ancestors' private
// items: `models::tests` and `relations::tests` both read this source, and two copies of "the
// same" model are how the two files quietly stop asserting the same thing. A fixture with a
// single reader stays with that reader.
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

/// Every class [`MODEL`]'s application defines, which is what a macro's candidate list is read
/// against.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn known() -> std::collections::BTreeSet<String> {
    ["Story", "User", "Comment", "Tag", "Tagging"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Every class [`MODEL`] gives a generated relation class to — its four collection elements and
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

    #[test]
    fn a_nested_class_claims_a_table_only_when_it_is_a_model() {
        // "A nested class claims nothing" is too coarse: it claims what Rails
        // says it claims", and the superclass is the whole of the new gate. A `class Story`
        // inside `module Legacy` that is not an ActiveRecord model must still claim nothing, or
        // the schema's columns land on a service object that never reads one — which is
        // measured: the six corpora hold 43 such classes whose name inflects onto a real table.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let nested = harness.write(
            "app/models/admin/story.rb",
            // The second is two hops from the base rather than one, which is the shape
            // solidus writes 101 of and the reason `is_model` climbs rather than asks once.
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
        // models really do read `stories`; a one-claimant rule lets only the top-level one
        // answer.
        assert!(harness.has("Admin::Story#title()"));
        assert!(harness.has("Legacy::Story#title()"));
        // The one that inherits nothing is not a model, and claims nothing.
        assert!(!harness.has("Service::Story#title()"));
    }

    #[test]
    fn a_namespace_that_declares_a_prefix_moves_every_table_under_it() {
        // `full_table_name_prefix` is `module_parents.detect { |p| p.respond_to?(...) }`, so a
        // `def self.table_name_prefix` on `Admin` says every model under it reads a table that
        // begins `admin_`. It is read out of a file, which is why the inflection happens where
        // the documents are and not where the definitions are.
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
        // And the bare plural is not also its: `stories` is a table this schema does not have,
        // but the claim would still have been wrong.
        assert!(!harness.has("Admin::Story#title()"));
    }

    #[test]
    fn a_namespace_that_declares_a_suffix_moves_every_table_under_it_too() {
        // `full_table_name_suffix` is the same `module_parents.detect`, and it measures **0**
        // in six applications. It is read anyway because it is the same syntax in the same
        // walk, and ignoring it is the only way this reader can name a table that exists and is
        // not the one the class reads.
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
        // The commoner of the two spellings by a factor of nearly four — 36 of the 46
        // declarations in six corpora — and it is a call rather than a `def` because the engine says it about a
        // module somebody else wrote. `Rails::Engine#isolate_namespace` installs
        // `table_name_prefix` as `generate_railtie_name(mod.name)` and an underscore.
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
        // `compute_table_name`'s other branch is `parent_singular_child_plural`, and it needs
        // the parent's own table and then the parent's parent's. Declining it is measured
        // rather than assumed: 22 classes in six applications are nested inside a model and not
        // one of them names a table any of those applications has.
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
        // The namespace rule, asked by the second generator to reach the shape. A generated
        // `class Reports::Metric` where nothing declares `Reports` costs that namespace its own
        // members silently, so the claim is declined rather than spelled. It declines nothing
        // in six corpora and is here because the damage it prevents cannot be seen.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        // `class Reports::Story` and no `module Reports` anywhere — **and no `reports/`
        // directory either**, which is the half that makes the namespace unreachable rather
        // than merely unwritten. A file sitting in one is the other test below.
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
    /// `app/models/reports/story.rb` is a file Zeitwerk loads by defining `Reports` first — a
    /// directory under an autoload root with no `reports.rb` beside it **is** the declaration —
    /// so the namespace is spellable and the claim is not declined. The two tests are the same
    /// class and the same superclass, and the only difference between them is which directory
    /// the file sits in, which is the whole of the rule.
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
        // The other end of the same walk: a chain that runs out is not a model, and a chain
        // that runs in a circle is not an infinite loop. Neither shape can claim a table.
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
        // The one place an inflected claim and a written one meet, and both readings are in
        // the corpus. mastodon's throwaway `MoveUserSettings::LegacySetting` says
        // `self.table_name = "settings"` and, if a written name simply replaces an inflected
        // one, *takes* those columns off
        // the `Setting` model that reads them; discourse's three test doubles do the same to
        // `Post`. But discourse's `TopicViewItem` says `topic_views` and the `TopicView` whose
        // name implies it is a plain view object that reads no table at all.
        //
        // So the guess survives the meeting only when the class it is about is a model.
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
        // And the class that is not a model does not keep a table somebody else named.
        assert!(harness.has("Backfill::WidgetRow#name()"));
        assert!(!harness.has("Widget#name()"));
    }

    #[test]
    fn an_anonymous_class_is_not_a_model_however_it_is_written() {
        // A defect only measurement finds, and it is reachable from two different
        // generators. rubydex names an anonymous `Class.new(ApplicationRecord)`
        // `<hash>:<offset><anonymous>`; that class is an ActiveRecord model by every rule this
        // crate has, so it asked for a relation — and `class ` + that name is not RBS, so
        // `Synthesized::record`'s parse gate threw away **the whole document**, taking every
        // real declaration in the file with it. Solidus writes 38 such specs.
        //
        // The assertion is on a *neighbour*: what proves the document survived is that the
        // class written beside the anonymous one still declares its own members.
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
        // `claims` is filled per *definition*, so a model reopened in a second file pushed its
        // own name twice and "a table two classes claim is claimed by neither" then declined it
        // — losing every column on a model nothing was ambiguous about. The shape is the
        // ordinary Ruby idiom for namespacing a helper under a model: forem writes
        // `class AuditLog` again in `app/queries/audit_log/unpublish_alls_query.rb`, and
        // discourse writes `class Reviewable < ActiveRecord::Base` in **six** `lib/reviewable/`
        // files. **Three models in six corpora** were in that state, discourse's `Post` among
        // them, and none of the three is a collision.
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
        // The narrowed ambiguity rule, in the case it was built for. Several claimants are kept
        // when they demodulize alike — the same convention applied twice — and two *different*
        // names landing on one table is the inflector having got one of them wrong.
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
        // An ambiguous answer is not an answer. Both classes are top level, both are the user's
        // own, and both name `stories` — so the columns go to neither of them.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let rival = harness.write("app/models/storie.rb", "class Storie\nend\n");
        harness.watch(&[&rival]);

        assert!(!harness.has("Story#title()"), "an ambiguous table answered");
        assert!(!harness.has("Storie#title()"));
    }

    #[test]
    fn a_model_that_names_its_own_table_reads_that_one_and_not_the_other() {
        // The documented escape, and the half of it that is easy to get wrong: a class that
        // says `self.table_name` must *stop* claiming the table its name implies, or one model
        // answers with two schemas at once.
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
