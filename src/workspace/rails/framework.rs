//! The framework's own singletons, and the fixed class each hands back.
//!
//! `Rails.root` is a `Pathname`, `Rails.cache` an `ActiveSupport::Cache::Store`, `Time.zone` an
//! `ActiveSupport::TimeZone`. railties and activesupport ship no `sig/`, so
//! [`Types`](crate::analysis::types::Types) has nothing to read, and every chain on one of them
//! stops at its first hop: `Rails.root.join` falls to the name rung and lists every `join` in the
//! bundle. rubydex has already found the member; only its return type is missing, and this crate
//! can write that down.
//!
//! # Why a table and not a rung
//!
//! The answer can be written *in advance*: there is one `Rails.root`, and it is a `Pathname` in
//! every Rails application. So it takes the generator route like everything else this directory
//! knows: RBS text, into `Types::harvest` with Ruby's own signatures and every gem's `sig/`, adding
//! no rung and teaching `types.rs` no Rails word.
//!
//! # Which four, and why the rest are declined
//!
//! **The bar is not "is the return knowable" but "does the class the answer names hold the members
//! the call then asks for".** A receiver typed to a class whose members this crate cannot see
//! *displaces* the name-based guess, which often held the right word (`types.md` records this). So
//! a row is here only if its class answers the members real applications call one hop later:
//!
//! | chain | return |
//! | --- | --- |
//! | `Time.zone` | `ActiveSupport::TimeZone` |
//! | `Rails.root` | `Pathname` |
//! | `Rails.cache` | `ActiveSupport::Cache::Store` |
//! | `Rails.application` | the application's own class, else `Rails::Application` |
//! | `Rails.logger` | `ActiveSupport::BroadcastLogger`, or what the application assigns ([`ASSIGNED`]) |
//!
//! One is declined: the class *really* returned answers the calls through `method_missing`, which
//! rubydex cannot see, so declaring it would trade a working guess for silence:
//!
//! | chain | return | why it fails |
//! | --- | --- | --- |
//! | `Rails.configuration` | `Rails::Application::Configuration` | `config.action_controller` and everything a project adds are `method_missing`; since the third table types `config`, its body answers instead |
//!
//! `Rails.logger` was declined here too, read as `method_missing`. It is not: `info`, `warn`,
//! `error` and the rest are `class_eval`'d from a string ([`LOGGER_METHODS`]), which [`members`]
//! writes down, and only `tagged` and a wrapped logger's own methods are `method_missing` (2 and
//! 11 of 1,334 calls in the corpora, the 11 being test doubles).
//!
//! Two more were declined for that reason until the members behind them were written down
//! ([`members`]): `Rails.env` (`development?` and its siblings are `class_eval`'d) and
//! `Time.current` (a `TimeWithZone`, whose `year` is `class_eval`'d and whose calculations forward
//! to `Time`). A controller's `helpers` was declined for the same reason until its members were
//! written down: the next call reaches the application's helper modules, which Rails
//! mixes into a class it makes at run time, so [`HELPER_PROXY`] is that class. A mailer's `with`
//! stays declined here: the call after it reaches a mailer action through `method_missing`. `with`
//! itself is `ActionMailer::Parameterized::Mailer`, which its concern's `def` says and
//! [`super::concerns`] now reads.
//!
//! `Rails.logger` is worth stating twice, because it is the most-called of these and the
//! temptation is real: `ActiveSupport::Logger` answers almost all of those calls and is **not what
//! Rails returns**. Since 7.1 `initialize_logger` wraps whatever the application configured in a
//! `BroadcastLogger`, and only a later `Rails.logger =` replaces it, which the row joins in.
//!
//! # The second table: what a controller and a template call on themselves
//!
//! `params`, `request`, `session` and the rest are the most-called methods in a controller, and
//! actionpack ships no signature for any of them, so every chain on one stopped at its first hop.
//! [`CONTEXT`] writes their returns the same way, with two differences from [`SINGLETONS`]:
//!
//! - **Four of them are made by a macro, not a `def`**: `attr_internal :request`,
//!   `delegate :session, to: "@_request"`, `delegate :flash, to: :request`, and the view context's
//!   `delegate(*CONTROLLER_DELEGATES, to: :controller)`. rubydex reads no macro in a gem, so there
//!   is no member for a return type to hang on, and the row **declares the member** too. It has no
//!   line to go to ([`Declared::at`] `None`); types, hover and completion reach it.
//! - **The class may differ under a test harness.** `session` is an `ActionDispatch::Request::Session`
//!   in every running application, API-only ones included (a disabled one), and a test session
//!   class under `ActionController::TestCase`. Decided 2026-09-24: the application's type is the
//!   answer, and the card says so.
//!
//! A controller's `cookies` is a private `def`, so its row is written `private def`
//! ([`Declared::private`]): a public signature over it could make it public, depending on the order
//! rubydex resolved the two definitions in.
//!
//! Two are declined:
//!
//! | method | why |
//! | --- | --- |
//! | a template's `params` | a mailer is a template's controller too, and a mailer's `params` is a `Hash` |
//! | `logger` | the railtie sets it to `Rails.logger`, but `config.action_controller.logger` replaces it through configuration, which no call of a writer shows |
//!
//! A template's `request` is `ActionDispatch::Request?`, since a mailer has none and the view
//! context holds `nil`. The other four raise in a mailer's template, so the type holds wherever the
//! call returns.
//!
//! # The third table: blocks that run against something else, and `config`
//!
//! [`BLOCKS`] and [`CALLBACK_HOSTS`] write the `[self: T]` a signature would say for a block
//! `Rails.application.configure`, `routes.draw`, a controller's callbacks and a mailer's `default`
//! run against something other than the code around them ([`super::blocks`] says why and which).
//! The callbacks are made with `define_method`, so, like [`CONTEXT`]'s macros, their rows declare
//! the member.
//!
//! [`CONFIG`] and [`NAMESPACES`] answer what the first table declines for `Rails.configuration`,
//! as far as it can be answered: `config` itself is a `Rails::Application::Configuration`, and a
//! framework's own namespace (`config.action_mailer`) is the `ActiveSupport::OrderedOptions` its
//! railtie assigns, written only where that framework or gem is in the bundle. What a namespace
//! holds (`config.action_mailer.default_url_options`) is whatever the application assigned, and
//! stays untyped; so does a setting an application assigns outright.
//!
//! # What is declared, and what is not
//!
//! Only the return type. railties really writes `def self.root`, and activesupport really writes
//! `def zone` in `class << self`, so the declaration rubydex holds already has a place. The
//! definition written here carries [`Declared::at`] `None` and never becomes a second one. That is
//! why it is [`Source::Interface`]: no file says what these return, whatever else they say.
//!
//! **Both ends are checked against the graph before anything is written.** A workspace whose bundle
//! is not indexed declares nothing, instead of conjuring a `module Rails` with no place and no
//! members: the rule [`super::framework_classes`] applies to the long tail's three gem classes,
//! read the same way.

use std::collections::BTreeMap;

use ruby_prism::Node;

use crate::generated::{At, Declared, Facts, Namespaces, Owner, SENT, SHARED, Source, WRITTEN};

use super::adapters::{self, CONNECTION_HANDLING, CONNECTION_POOL, DATABASE_STATEMENTS, LEASING};
use super::syntax::constant_spelling;

/// The class every Rails application's own application class inherits.
///
/// Both a table row's fallback return and the superclass [`application_class`] looks for: one
/// constant, because they are the same fact. `Rails.application` is an instance of whichever class
/// the project wrote `< Rails::Application` under, and of `Rails::Application` itself where it
/// wrote none.
pub(super) const APPLICATION: &str = "Rails::Application";

/// One framework singleton: `(owner, method, what it returns)`.
///
/// Written out whole, not grown, for [`Source::rank`](crate::generated::Source)'s reason: a table
/// whose rows arrive one at a time is a table nobody can review. The module docs say why each row
/// is here, and why four others are not.
///
/// **Which keyword opens each owner's body is not in the table**, and must not be: railties writes
/// `module Rails`, and activesupport reopens Ruby's `class Time` (`def zone` inside
/// `class << self`, in `active_support/core_ext/time/zones.rb`). The render key is
/// `(is_module, name)`, so guessing wrong declares a second constant RBS refuses to hold beside the
/// first. [`Namespaces::opens`] is what the graph says, and it is asked instead.
const SINGLETONS: [(&str, &str, &str); 4] = [
    ("Rails", "root", "Pathname"),
    ("Rails", "cache", "ActiveSupport::Cache::Store"),
    ("Rails", "application", APPLICATION),
    ("Time", "zone", "ActiveSupport::TimeZone"),
];

/// The class Rails' `initialize_logger` leaves in `Rails.logger`: whatever logger the application
/// configured, wrapped (railties 7.1 and later, `bootstrap.rb`).
const BROADCAST_LOGGER: &str = "ActiveSupport::BroadcastLogger";

/// A framework singleton Rails sets and the application may assign after:
/// `(owner, method, what Rails sets)`.
///
/// The row is that class **or what the application assigns** (`WrittenByItsWriter` in the union):
/// one application's `Rails.logger = Logger.new(STDOUT)` behind a debug switch makes it
/// `BroadcastLogger | Logger` there, and an application assigning nothing gets the class alone.
/// Before `initialize_logger` runs it is `nil`, as `Rails.root` is before the application class
/// exists; the row answers for code that runs after boot, as that one does.
const ASSIGNED: [(&str, &str, &str); 1] = [("Rails", "logger", BROADCAST_LOGGER)];

/// The logger methods `BroadcastLogger` writes with `class_eval` from a string
/// (`LOGGER_METHODS`), each dispatched to every logger it wraps. rubydex reads no string, so
/// `Rails.logger.info` had no member to reach.
const LOGGER_METHODS: [(&str, &str); 14] = [
    ("<<", "(untyped)"),
    ("log", "(*untyped) ?{ () -> untyped }"),
    ("add", "(*untyped) ?{ () -> untyped }"),
    ("debug", "(*untyped) ?{ () -> untyped }"),
    ("info", "(*untyped) ?{ () -> untyped }"),
    ("warn", "(*untyped) ?{ () -> untyped }"),
    ("error", "(*untyped) ?{ () -> untyped }"),
    ("fatal", "(*untyped) ?{ () -> untyped }"),
    ("unknown", "(*untyped) ?{ () -> untyped }"),
    ("level=", "(untyped)"),
    ("sev_threshold=", "(untyped)"),
    ("close", "()"),
    ("formatter", "()"),
    ("formatter=", "(untyped)"),
];

/// How Rails makes one of [`CONTEXT`]'s methods, which decides what its card says.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Made {
    /// A `def` rubydex already holds, so only the return is written.
    Def,
    /// The same, for a `def` Rails keeps private: written `private def`, so it stays private.
    PrivateDef,
    /// A macro rubydex does not read in a gem, so the row declares the member.
    Macro(&'static str),
}

/// What a controller calls on itself, and a template on its view context:
/// `(owner, method, what it returns, how Rails makes it)`.
///
/// Instance methods, where [`SINGLETONS`] holds class methods. Checked against actionpack and
/// actionview 7.2, 8.0 and 8.1, which write all of them the same way. The module docs say why each
/// row is here, and why three others are not.
const CONTEXT: [(&str, &str, &str, Made); 11] = [
    (
        "ActionController::StrongParameters",
        "params",
        "ActionController::Parameters",
        Made::Def,
    ),
    (
        "ActionController::Metal",
        "request",
        "ActionDispatch::Request",
        Made::Macro("attr_internal"),
    ),
    (
        "ActionController::Metal",
        "response",
        "ActionDispatch::Response",
        Made::Macro("attr_internal_reader"),
    ),
    (
        "ActionController::Metal",
        "session",
        SESSION,
        Made::Macro("delegate"),
    ),
    (
        "ActionController::Flash",
        "flash",
        "ActionDispatch::Flash::FlashHash",
        Made::Macro("delegate"),
    ),
    (
        "ActionController::Cookies",
        "cookies",
        "ActionDispatch::Cookies::CookieJar",
        Made::PrivateDef,
    ),
    (
        VIEW_CONTEXT,
        "request",
        "ActionDispatch::Request?",
        Made::Macro("attr_internal"),
    ),
    (
        VIEW_CONTEXT,
        "response",
        "ActionDispatch::Response",
        Made::Macro("delegate"),
    ),
    (VIEW_CONTEXT, "session", SESSION, Made::Macro("delegate")),
    (
        VIEW_CONTEXT,
        "flash",
        "ActionDispatch::Flash::FlashHash",
        Made::Macro("delegate"),
    ),
    (
        VIEW_CONTEXT,
        "cookies",
        "ActionDispatch::Cookies::CookieJar",
        Made::Macro("delegate"),
    ),
];

/// The module every template's view context includes, and where actionview delegates the
/// controller's methods from.
const VIEW_CONTEXT: &str = "ActionView::Helpers::ControllerHelper";

/// What `session` returns: the one row whose class a test harness replaces (see the module docs).
const SESSION: &str = "ActionDispatch::Request::Session";

/// Which side of its owner a [`BLOCKS`] or [`CONFIG`] row is written on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Instance,
    Singleton,
}

/// Framework methods whose block runs against something else ([`super::blocks`]), and the one
/// return their chains need: `(owner, side, method, parameters, returns, how Rails makes it)`.
///
/// - **`configure`** runs its block against the application, `instance_eval` on the instance and
///   `instance.configure` on the class, so `config` inside it is the application's, and hands back
///   what the block made.
/// - **`draw`** ends in `nil`.
/// - **`routes` and `draw`** run it against an `ActionDispatch::Routing::Mapper`, the object that
///   answers `resources`, `get` and `namespace`. `routes` also returns the route set `draw` is
///   called on, which its body only says through `config.route_set_class`.
/// - **A mailer's `default`** runs a lambda given for any key against the mailer (`instance_exec`
///   in `ActionMailer::Base#apply_defaults`).
const BLOCKS: [(&str, Side, &str, &str, &str, Made); 7] = [
    (
        "Rails::Railtie",
        Side::Instance,
        "configure",
        "[T] () { () [self: self] -> T }",
        "T",
        Made::Def,
    ),
    (
        "Rails::Railtie",
        Side::Singleton,
        "configure",
        "[T] () { () [self: instance] -> T }",
        "T",
        Made::Def,
    ),
    (
        "Rails::Engine",
        Side::Instance,
        "routes",
        "() ?{ () [self: ActionDispatch::Routing::Mapper] -> void }",
        ROUTE_SET,
        Made::Def,
    ),
    (
        ROUTE_SET,
        Side::Instance,
        "draw",
        "() { () [self: ActionDispatch::Routing::Mapper] -> void }",
        "nil",
        Made::Def,
    ),
    (
        ROUTE_SET,
        Side::Instance,
        "prepend",
        "() { () [self: ActionDispatch::Routing::Mapper] -> void }",
        "untyped",
        Made::Def,
    ),
    (
        ROUTE_SET,
        Side::Instance,
        "append",
        "() { () [self: ActionDispatch::Routing::Mapper] -> void }",
        "untyped",
        Made::Def,
    ),
    (
        "ActionMailer::Base",
        Side::Singleton,
        "default",
        "(**^() [self: instance] -> untyped)",
        "untyped",
        Made::Def,
    ),
];

/// The route set a Rails application draws on.
const ROUTE_SET: &str = "ActionDispatch::Routing::RouteSet";

/// The classes `AbstractController::Callbacks` puts its callbacks on, one `define_method` each
/// ([`super::blocks::CONTROLLER_CALLBACKS`]): every controller and every mailer.
const CALLBACK_HOSTS: [&str; 3] = [
    "ActionController::Base",
    "ActionController::API",
    "ActionMailer::Base",
];

/// What `config` is, and what a few of its settings hold: `(owner, side, method, returns, how
/// Rails makes it)`.
///
/// - **`config` itself**, on the three classes that write their own, and on the class side, where
///   railties writes `delegate :config, to: :instance`.
/// - **Only settings Rails fills with a container the application then fills in**: the host list,
///   the parameter filter, the load paths, the middleware stack. A setting an application assigns
///   outright (`time_zone`, `eager_load`) holds whatever it was given, and is left alone.
const CONFIG: [(&str, Side, &str, &str, Made); 17] = [
    (
        "Rails::Railtie",
        Side::Instance,
        "config",
        "Rails::Railtie::Configuration",
        Made::Def,
    ),
    (
        "Rails::Engine",
        Side::Instance,
        "config",
        "Rails::Engine::Configuration",
        Made::Def,
    ),
    (
        APPLICATION,
        Side::Instance,
        "config",
        APPLICATION_CONFIGURATION,
        Made::Def,
    ),
    (
        "Rails::Railtie",
        Side::Singleton,
        "config",
        "Rails::Railtie::Configuration",
        Made::Macro("delegate"),
    ),
    (
        "Rails::Engine",
        Side::Singleton,
        "config",
        "Rails::Engine::Configuration",
        Made::Macro("delegate"),
    ),
    (
        APPLICATION,
        Side::Singleton,
        "config",
        APPLICATION_CONFIGURATION,
        Made::Macro("delegate"),
    ),
    (
        APPLICATION_CONFIGURATION,
        Side::Instance,
        "hosts",
        "Array[untyped]",
        Made::Def,
    ),
    (
        APPLICATION_CONFIGURATION,
        Side::Instance,
        "public_file_server",
        "ActiveSupport::OrderedOptions",
        Made::Def,
    ),
    (
        APPLICATION_CONFIGURATION,
        Side::Instance,
        "session_options",
        "Hash[untyped, untyped]",
        Made::Def,
    ),
    (
        APPLICATION_CONFIGURATION,
        Side::Instance,
        "filter_parameters",
        "Array[untyped]",
        Made::Def,
    ),
    (
        APPLICATION_CONFIGURATION,
        Side::Instance,
        "x",
        "Rails::Application::Configuration::Custom",
        Made::Def,
    ),
    (
        ENGINE_CONFIGURATION,
        Side::Instance,
        "paths",
        "Rails::Paths::Root",
        Made::Def,
    ),
    (
        ENGINE_CONFIGURATION,
        Side::Instance,
        "root",
        "Pathname",
        Made::Def,
    ),
    (
        ENGINE_CONFIGURATION,
        Side::Instance,
        "autoload_paths",
        "Array[untyped]",
        Made::Def,
    ),
    (
        ENGINE_CONFIGURATION,
        Side::Instance,
        "autoload_once_paths",
        "Array[untyped]",
        Made::Def,
    ),
    (
        ENGINE_CONFIGURATION,
        Side::Instance,
        "eager_load_paths",
        "Array[untyped]",
        Made::Def,
    ),
    (
        ENGINE_CONFIGURATION,
        Side::Instance,
        "middleware",
        "Rails::Configuration::MiddlewareStackProxy",
        Made::Def,
    ),
];

const APPLICATION_CONFIGURATION: &str = "Rails::Application::Configuration";
const ENGINE_CONFIGURATION: &str = "Rails::Engine::Configuration";

/// The configuration a railtie adds with `config.<name> = ActiveSupport::OrderedOptions.new`:
/// `(name, the module whose presence says the railtie is in the bundle, what it holds)`.
///
/// **Made by `method_missing`** on `Rails::Railtie::Configuration`, which answers a name once some
/// railtie has assigned it, so nothing declares any of them, and `config.action_mailer` stopped
/// every chain. Each is written only where its framework or gem is in the bundle: a name nothing
/// assigned raises.
const NAMESPACES: [(&str, &str, &str); 22] = [
    ("action_cable", "ActionCable", ORDERED_OPTIONS),
    ("action_controller", "ActionController", ORDERED_OPTIONS),
    ("action_dispatch", "ActionDispatch", ORDERED_OPTIONS),
    ("action_mailbox", "ActionMailbox", ORDERED_OPTIONS),
    ("action_mailer", "ActionMailer", ORDERED_OPTIONS),
    ("action_text", "ActionText", ORDERED_OPTIONS),
    ("action_view", "ActionView", ORDERED_OPTIONS),
    ("active_job", "ActiveJob", ORDERED_OPTIONS),
    ("active_model", "ActiveModel", ORDERED_OPTIONS),
    ("active_record", "ActiveRecord", ORDERED_OPTIONS),
    ("active_storage", "ActiveStorage", ORDERED_OPTIONS),
    ("active_support", "ActiveSupport", ORDERED_OPTIONS),
    ("i18n", "I18n", ORDERED_OPTIONS),
    ("assets", "Propshaft", ORDERED_OPTIONS),
    ("factory_bot", "FactoryBot", ORDERED_OPTIONS),
    ("global_id", "GlobalID", ORDERED_OPTIONS),
    ("importmap", "Importmap", ORDERED_OPTIONS),
    ("mission_control", "MissionControl", ORDERED_OPTIONS),
    ("solid_cache", "SolidCache", ORDERED_OPTIONS),
    ("solid_queue", "SolidQueue", ORDERED_OPTIONS),
    ("turbo", "Turbo", ORDERED_OPTIONS),
    ("lograge", "Lograge", "Lograge::OrderedOptions"),
];

const ORDERED_OPTIONS: &str = "ActiveSupport::OrderedOptions";

/// Every `config.<name> = value` a file writes: the name, the whole assignment and the name in it.
///
/// `Rails::Railtie::Configuration#method_missing` keeps any name assigned to it and answers it after,
/// so `Rails.configuration.dispatcher = Dispatcher.instance` in an initializer is a setting every
/// file reads, which nothing declares. `config.x.name` is `Custom`'s, already declared, and a name
/// a railtie assigns ([`NAMESPACES`]) is left to that table.
///
/// **Only where the configuration is Rails'**, by how Rails writes it, since a gem's own
/// `ShopPromotions.config.x = y` or a spec's `let(:config)` is some other object:
///
/// - `Rails.application.config`, `Shop::Application.config` and `Rails.configuration`, anywhere;
/// - a bare `config` in a class `< Rails::Application`, `< Rails::Engine` or `< Rails::Railtie`,
///   or in the block of `Rails.application.configure` or `Shop::Application.configure`, and not in
///   a `def` there.
#[must_use]
pub fn read_config_writes(source: &str) -> Vec<(String, At)> {
    struct Writes<'s> {
        source: &'s str,
        /// Whether a bare `config` here is the application's, an engine's or a railtie's.
        rails: bool,
        found: Vec<(String, At)>,
    }
    impl Writes<'_> {
        fn spelled(&self, node: &ruby_prism::Node<'_>) -> &str {
            let location = node.location();
            self.source
                .get(location.start_offset()..location.end_offset())
                .unwrap_or_default()
                .trim_start_matches("::")
        }

        /// `Rails.application`, or a constant naming an application class (`Shop::Application`).
        fn application(&self, node: &ruby_prism::Node<'_>) -> bool {
            // Spelled so, `Application` can only be a constant.
            let spelled = self.spelled(node);
            spelled == "Rails.application" || spelled.rsplit("::").next() == Some("Application")
        }

        /// Whether `node` is a configuration Rails keeps settings in.
        fn configuration(&self, node: &ruby_prism::Node<'_>) -> bool {
            let Some(call) = node.as_call_node() else {
                return false;
            };
            match (call.name().as_slice(), call.receiver()) {
                (b"config", None) => self.rails,
                (b"config", Some(receiver)) => self.application(&receiver),
                (b"configuration", Some(receiver)) => self.spelled(&receiver) == "Rails",
                _ => false,
            }
        }

        fn within(&mut self, rails: bool, body: impl FnOnce(&mut Self)) {
            let outer = std::mem::replace(&mut self.rails, rails);
            body(self);
            self.rails = outer;
        }
    }
    impl<'pr> ruby_prism::Visit<'pr> for Writes<'_> {
        fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
            let rails = node.superclass().is_some_and(|superclass| {
                matches!(
                    self.spelled(&superclass),
                    "Rails::Application" | "Rails::Engine" | "Rails::Railtie"
                )
            });
            self.within(rails, |this| ruby_prism::visit_class_node(this, node));
        }

        fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
            self.within(false, |this| ruby_prism::visit_module_node(this, node));
        }

        fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
            self.within(false, |this| ruby_prism::visit_def_node(this, node));
        }

        fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
            let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
            if let (Some(setting), Some(receiver), Some(selection)) =
                (name.strip_suffix('='), node.receiver(), node.message_loc())
                && node.is_attribute_write()
                && self.configuration(&receiver)
                // `config[:key] = v` is an index write, `[]=`, and writes no setting.
                && setting.bytes().all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
                && !NAMESPACES.iter().any(|(held, _, _)| *held == setting)
            {
                let whole = node.location();
                self.found.push((
                    setting.to_owned(),
                    (
                        (whole.start_offset() as u32, whole.end_offset() as u32),
                        (
                            selection.start_offset() as u32,
                            selection.start_offset() as u32 + setting.len() as u32,
                        ),
                    ),
                ));
            }
            let configures = name == "configure"
                && node
                    .receiver()
                    .is_some_and(|receiver| self.application(&receiver));
            if configures {
                self.within(true, |this| ruby_prism::visit_call_node(this, node));
            } else {
                ruby_prism::visit_call_node(self, node);
            }
        }
    }
    let result = ruby_prism::parse(source.as_bytes());
    let mut writes = Writes {
        source,
        rails: false,
        found: Vec::new(),
    };
    ruby_prism::Visit::visit(&mut writes, &result.node());
    writes.found
}

/// The reader and writer of each setting a file assigns ([`read_config_writes`]), on
/// [`RAILTIE_CONFIGURATION`]: the reader holds whatever its writer is handed on any configuration,
/// the application's, an engine's or a railtie's, since they share one class variable
/// ([`SHARED`]).
#[must_use]
pub fn config_facts(writes: &[(String, At)], caption: &str) -> Facts {
    let owner = Owner::Instance(RAILTIE_CONFIGURATION.to_owned());
    let mut facts = Facts::default();
    for (name, at) in writes {
        let because = format!(
            "From `{caption}`: `config.{name} =` is kept by `Rails::Railtie::Configuration`'s \
             `method_missing`, and answered after."
        );
        facts.declare(Declared {
            owner: owner.clone(),
            name: name.clone(),
            returns: SHARED.to_owned(),
            parameters: "()".to_owned(),
            because: because.clone(),
            at: Some(*at),
            from: Source::Convention,
            overloads: Vec::new(),
            private: false,
        });
        facts.declare(Declared {
            owner: owner.clone(),
            name: format!("{name}="),
            returns: "untyped".to_owned(),
            parameters: "(untyped)".to_owned(),
            because,
            at: Some(*at),
            from: Source::Convention,
            overloads: Vec::new(),
            private: false,
        });
    }
    facts
}

/// Methods whose return Rails' own source fixes, where the framework ships no signature and
/// rubydex cannot see the method or read its body to a type:
/// `(owner, side, method, parameters, returns, how Rails makes it)`.
///
/// Checked against activesupport 7.2, 8.0, 8.1 and main. Four groups:
///
/// - **`Rails.env`'s predicates.** `EnvironmentInquirer` writes `development?`, `test?` and
///   `production?` with `class_eval` from a string, each reading an instance variable its
///   constructor set to `env == name`. `local?` is a real `def` over another.
/// - **`ActiveSupport::TimeWithZone`, what a `datetime` column reads as.** Its own `def`s read
///   `@utc` and `@time`, which nothing types; `year`, `month` and `to_date` are `class_eval`'d
///   (`time.year`); and every other `Time` method arrives through `method_missing`, which hands a
///   `Time` answer back wrapped as a `TimeWithZone` and a `Range` of them as a `Range` of wrapped
///   ends ([`ZONE_FORWARDS`], [`ZONE_RANGES`]).
/// - **`Time.zone`'s `ActiveSupport::TimeZone`**: `now`, `parse` and the rest build a
///   `TimeWithZone`; `parse` and `strptime` answer `nil` for a string with no date in it.
/// - **`Time.current` and `Date.current`**: `Time.zone ? Time.zone.now : Time.now`, so a
///   `TimeWithZone` or a `Time`.
/// - **A model's class methods Rails writes in a concern** ([`MODEL_CLASS_METHODS`]), which the
///   concern generator copies onto `ActiveRecord::Base` without a type.
/// - **What a framework class forwards with a macro** ([`FORWARDED`]).
fn members() -> Vec<(
    &'static str,
    Side,
    &'static str,
    &'static str,
    &'static str,
    Made,
)> {
    let mut rows = vec![
        (INQUIRER, Side::Instance, "local?", "()", "bool", Made::Def),
        (
            "Time",
            Side::Singleton,
            "current",
            "()",
            ZONED_OR_TIME,
            Made::Def,
        ),
        ("Date", Side::Singleton, "current", "()", "Date", Made::Def),
    ];
    for name in ["development?", "test?", "production?"] {
        rows.push((
            INQUIRER,
            Side::Instance,
            name,
            "()",
            "bool",
            Made::Macro("class_eval"),
        ));
    }
    for name in [
        "year", "mon", "month", "day", "mday", "wday", "yday", "hour", "min", "sec", "usec", "nsec",
    ] {
        rows.push((
            ZONED,
            Side::Instance,
            name,
            "()",
            "Integer",
            Made::Macro("class_eval"),
        ));
    }
    rows.push((
        ZONED,
        Side::Instance,
        "to_date",
        "()",
        "Date",
        Made::Macro("class_eval"),
    ));
    // What each hands back is the first wrapped logger's answer, which a custom logger decides:
    // the member is what the row says, not its type.
    for (name, parameters) in LOGGER_METHODS {
        rows.push((
            BROADCAST_LOGGER,
            Side::Instance,
            name,
            parameters,
            "untyped",
            Made::Macro("class_eval"),
        ));
    }
    for (name, parameters, returns) in ZONE_DEFS {
        rows.push((ZONED, Side::Instance, name, parameters, returns, Made::Def));
    }
    for name in ZONE_FORWARDS {
        rows.push((
            ZONED,
            Side::Instance,
            name,
            "(*untyped)",
            ZONED,
            Made::Macro("method_missing"),
        ));
    }
    for name in ZONE_RANGES {
        rows.push((
            ZONED,
            Side::Instance,
            name,
            "(*untyped)",
            "Range[ActiveSupport::TimeWithZone]",
            Made::Macro("method_missing"),
        ));
    }
    for (name, parameters, returns) in ZONE_BUILDERS {
        rows.push((
            TIME_ZONE,
            Side::Instance,
            name,
            parameters,
            returns,
            Made::Def,
        ));
    }
    for (name, parameters, returns) in MODEL_CLASS_METHODS {
        rows.push((
            "ActiveRecord::Base",
            Side::Singleton,
            name,
            parameters,
            returns,
            Made::Def,
        ));
    }
    for (owner, name, returns, made) in FORWARDED {
        rows.push((
            owner,
            Side::Instance,
            name,
            "()",
            returns,
            Made::Macro(made),
        ));
    }
    // `extend ActiveModel::Naming` is how any ActiveModel class gets it, not a model alone.
    rows.push((
        "ActiveModel::Naming",
        Side::Instance,
        "model_name",
        "()",
        "ActiveModel::Name",
        Made::Def,
    ));
    for (owner, name, parameters, returns) in RETURNS {
        rows.push((owner, Side::Instance, name, parameters, returns, Made::Def));
    }
    for owner in SAVES {
        rows.push((
            owner,
            Side::Instance,
            "save",
            "(**untyped) ?{ () -> untyped }",
            "bool?",
            Made::Def,
        ));
        rows.push((
            owner,
            Side::Instance,
            "save!",
            "(**untyped) ?{ () -> untyped }",
            "true?",
            Made::Def,
        ));
    }
    rows
}

/// The four modules that write a record's `save` and `save!`, one `super` into the next:
/// `Suppressor`, `Transactions`, `Validations`, `Persistence`. Which one a model's lookup reaches
/// first depends on the order `ActiveRecord::Base` includes them, so every one carries the row, and
/// each row is the whole chain's answer.
///
/// - **`save` is `bool?`**: `false` for an invalid record, a halted callback or a raised
///   `RecordInvalid`, and `nil` where a callback raises `ActiveRecord::Rollback`, which
///   `with_transaction_returning_status` swallows before `status` was set.
/// - **`save!` is `true?`**: every failure raises, except that same `Rollback`.
const SAVES: [&str; 4] = [
    "ActiveRecord::Suppressor",
    "ActiveRecord::Transactions",
    "ActiveRecord::Validations",
    "ActiveRecord::Persistence",
];

/// Instance methods whose `def` Rails writes and ya-lsp cannot read to a type, because the value
/// comes out of an instance variable, a gem's own object or a callback chain:
/// `(owner, method, parameters, returns)`. Checked against 7.2, 8.0 and 8.1, which write each the
/// same way.
///
/// - **A record's persistence** (`ActiveRecord::Persistence`): `update` and `update!` are `save`'s
///   and `save!`'s answers inside the same transaction wrapper; `update_columns` is
///   `affected_rows == 1`; the predicates read instance variables Rails sets to `true` or `false`.
/// - **`attributes`** is `AttributeSet#to_hash`: every attribute's name, a `String`, to its value.
/// - **`ActiveModel::Errors`**: `full_messages` maps each error to its `full_message`, and `to_a`
///   is its alias.
/// - **`redirect_to`** ends `self.status = proposed_status`, which `_extract_redirect_to_status`
///   answers with a status code or `302`; the instrumented copy hands back what it wrapped.
/// - **`credentials`** is `encrypted(…)`'s `ActiveSupport::EncryptedConfiguration`. What it holds
///   stays untyped: encrypted, and different per environment.
/// - **A mailer's `mail`** hands back the `Mail::Message` the mailer built in `initialize`.
/// - **`MessageDelivery`**: `deliver_now` is `handle_exceptions { run_callbacks { message.deliver } }`:
///   the message, the exception a `rescue_from` handled, `false` where a callback halted, or `nil`
///   where the action never called `mail` (`NullMail` answers `nil` to everything).
///   `deliver_later` is `perform_later`'s job, or `false` where it was not enqueued.
/// - **`ActiveSupport::Cache::Store`**: `write` is `true`, `false` where it was refused, or `nil`
///   where the backend failed, as every store Rails ships writes it and the method documents it.
///   `delete` and `exist?` are `true` or `false` in every store. `fetch_multi` is the `Hash` of
///   what each name held or its block made.
/// - **The request**: `env` is rack's `Hash` of `String` keys; `host` strips the port from a
///   `String`; `format` is the first acceptable `Mime::Type`, or `Mime::NullType`'s instance.
/// - **`flash.now`** is `@now ||= FlashNow.new(self)`.
/// - **`strip_tags`** is `full_sanitizer.sanitize(html)&.html_safe`: `nil` for `nil`.
/// - **`Duration#to_i`** is `@value.to_i`, and `in_seconds` its alias.
/// - **`exec_query`** is the adapter's `internal_exec_query`, an `ActiveRecord::Result` in every
///   adapter Rails ships.
/// - **`destroy!`** is `destroy || _raise_record_not_destroyed`, and `destroy` hands back the frozen
///   record.
const RETURNS: [(&str, &str, &str, &str); 35] = [
    (PERSISTENCE, "update", "(untyped)", "bool?"),
    (PERSISTENCE, "update!", "(untyped)", "true?"),
    (
        PERSISTENCE,
        "update_column",
        "(untyped, untyped, **untyped)",
        "bool",
    ),
    (PERSISTENCE, "update_columns", "(untyped)", "bool"),
    (PERSISTENCE, "new_record?", "()", "bool"),
    (PERSISTENCE, "previously_new_record?", "()", "bool"),
    (PERSISTENCE, "previously_persisted?", "()", "bool"),
    (PERSISTENCE, "destroyed?", "()", "bool"),
    (PERSISTENCE, "persisted?", "()", "bool"),
    (
        "ActiveRecord::AttributeMethods",
        "attributes",
        "()",
        "Hash[String, untyped]",
    ),
    (
        "ActiveModel::Attributes",
        "attributes",
        "()",
        "Hash[String, untyped]",
    ),
    (ERRORS, "full_messages", "()", "Array[String]"),
    (ERRORS, "to_a", "()", "Array[String]"),
    (ERRORS, "full_messages_for", "(untyped)", "Array[String]"),
    (
        "ActionController::Redirecting",
        "redirect_to",
        "(?untyped, ?untyped)",
        "Integer",
    ),
    (
        "ActionController::Instrumentation",
        "redirect_to",
        "(*untyped)",
        "Integer",
    ),
    (
        APPLICATION,
        "credentials",
        "()",
        "ActiveSupport::EncryptedConfiguration",
    ),
    (
        "ActionMailer::Base",
        "mail",
        "(?untyped) ?{ (untyped) -> untyped }",
        "Mail::Message",
    ),
    (
        "ActionMailer::MessageDelivery",
        "deliver_now",
        "()",
        DELIVERED,
    ),
    (
        "ActionMailer::MessageDelivery",
        "deliver_now!",
        "()",
        DELIVERED,
    ),
    (
        "ActionMailer::MessageDelivery",
        "deliver_later",
        "(?untyped)",
        ENQUEUED,
    ),
    (
        "ActionMailer::MessageDelivery",
        "deliver_later!",
        "(?untyped)",
        ENQUEUED,
    ),
    (
        CACHE_STORE,
        "write",
        "(untyped, untyped, ?untyped)",
        "bool?",
    ),
    (CACHE_STORE, "delete", "(untyped, ?untyped)", "bool"),
    (CACHE_STORE, "exist?", "(untyped, ?untyped)", "bool"),
    (
        CACHE_STORE,
        "fetch_multi",
        "(*untyped) { (untyped) -> untyped }",
        "Hash[untyped, untyped]",
    ),
    ("Rack::Request::Env", "env", "()", "Hash[String, untyped]"),
    ("ActionDispatch::Http::URL", "host", "()", "String"),
    (
        "ActionDispatch::Http::MimeNegotiation",
        "format",
        "(?untyped)",
        "Mime::Type | Mime::NullType",
    ),
    (
        "ActionDispatch::Flash::FlashHash",
        "now",
        "()",
        "ActionDispatch::Flash::FlashNow",
    ),
    (
        "ActionView::Helpers::SanitizeHelper",
        "strip_tags",
        "(untyped)",
        "ActiveSupport::SafeBuffer?",
    ),
    (DURATION, "to_i", "()", "Integer"),
    (DURATION, "in_seconds", "()", "Integer"),
    (
        DATABASE_STATEMENTS,
        "exec_query",
        "(untyped, ?untyped, ?untyped, ?prepare: untyped)",
        "ActiveRecord::Result",
    ),
    (PERSISTENCE, "destroy!", "()", "self"),
];

const PERSISTENCE: &str = "ActiveRecord::Persistence";
const ERRORS: &str = "ActiveModel::Errors";
const CACHE_STORE: &str = "ActiveSupport::Cache::Store";
const DURATION: &str = "ActiveSupport::Duration";

/// What `deliver_now` hands back (see [`RETURNS`]).
const DELIVERED: &str = "Mail::Message | Exception | false | nil";

/// What `deliver_later` hands back (see [`RETURNS`]).
const ENQUEUED: &str = "ActiveJob::Base | false";

/// Methods whose answer depends on the call, written as an RBS overload set so
/// [`Types`](crate::analysis::types::Types) picks the arm at each call:
/// `(owner, side, method, the constant only the Rails versions writing the method declare, arms)`,
/// each arm `(parameters, returns)`.
///
/// - **`Duration#since` and `ago`**, and their aliases, are `sum(sign, time)`: no argument is
///   `Time.current`'s `TimeWithZone | Time`; a time moves by `since` and `advance` into its own
///   class; a `Date` advances as a `Date` by days and becomes a time by seconds.
/// - **`Rails.cache.fetch` with a block** is what the block makes on a miss and what was stored on
///   a hit, which is the same value where the project keeps its keys consistent (ruled
///   2026-09-28). `raw:` is declined: a raw hit is the stored `String`. Without a block it is
///   whatever was stored.
/// - **`select_all`** is an `ActiveRecord::Result`, or a `FutureResult` with `async:`.
/// - **`Arel.sql`** is a `SqlLiteral`, or with binds a `BoundSqlLiteral` (8.1 hands a `SqlLiteral`
///   back unchanged).
/// - **`params.expect`** (8.0 and later) is `permit` then `require` of each key: one key's value, a
///   `Parameters` for a hash filter and an `Array` for a list filter, or an `Array` of the values of
///   several keys. So a filter written as keywords is one of the two; a bare key's value is
///   request data.
const ARMS: [Overloaded; 13] = [
    (DURATION, Side::Instance, "since", None, MOVED),
    (DURATION, Side::Instance, "from_now", None, MOVED),
    (DURATION, Side::Instance, "after", None, MOVED),
    (DURATION, Side::Instance, "ago", None, MOVED),
    (DURATION, Side::Instance, "until", None, MOVED),
    (DURATION, Side::Instance, "before", None, MOVED),
    (CACHE_STORE, Side::Instance, "fetch", None, FETCHED),
    (
        DATABASE_STATEMENTS,
        Side::Instance,
        "select_all",
        None,
        SELECTED,
    ),
    (
        "ActiveRecord::ConnectionAdapters::QueryCache",
        Side::Instance,
        "select_all",
        None,
        SELECTED,
    ),
    (
        "ActiveRecord::ConnectionAdapters::Mysql2::DatabaseStatements",
        Side::Instance,
        "select_all",
        None,
        SELECTED,
    ),
    ("Arel", Side::Singleton, "sql", None, SQL),
    (
        PARAMETERS,
        Side::Instance,
        "expect",
        Some("ActionController::ExpectedParameterMissing"),
        EXPECTED,
    ),
    (
        PARAMETERS,
        Side::Instance,
        "expect!",
        Some("ActionController::ExpectedParameterMissing"),
        EXPECTED,
    ),
];

/// One [`ARMS`] row: `(owner, side, method, gate, arms)`.
type Overloaded = (
    &'static str,
    Side,
    &'static str,
    Option<&'static str>,
    &'static [(&'static str, &'static str)],
);

const PARAMETERS: &str = "ActionController::Parameters";

/// A `Duration` moved from a time (see [`ARMS`]).
const MOVED: &[(&str, &str)] = &[
    ("()", "ActiveSupport::TimeWithZone | Time"),
    (
        "(ActiveSupport::TimeWithZone)",
        "ActiveSupport::TimeWithZone",
    ),
    ("(DateTime)", "DateTime"),
    ("(Time)", "Time"),
    ("(Date)", "Date | ActiveSupport::TimeWithZone | Time"),
];

/// `Rails.cache.fetch` (see [`ARMS`]).
const FETCHED: &[(&str, &str)] = &[
    ("[T] (untyped, **untyped) { (untyped, untyped) -> T }", "T"),
    (
        "(untyped, raw: untyped, **untyped) { (untyped, untyped) -> untyped }",
        "untyped",
    ),
    ("(untyped, ?untyped)", "untyped"),
];

/// A connection's `select_all` (see [`ARMS`]).
const SELECTED: &[(&str, &str)] = &[
    (
        "(untyped, ?untyped, ?untyped, ?preparable: untyped, ?allow_retry: untyped)",
        "ActiveRecord::Result",
    ),
    (
        "(untyped, ?untyped, ?untyped, async: untyped, **untyped)",
        "untyped",
    ),
];

/// `Arel.sql` (see [`ARMS`]).
const SQL: &[(&str, &str)] = &[
    ("(untyped, ?retryable: untyped)", "Arel::Nodes::SqlLiteral"),
    (
        "(untyped, untyped, *untyped, **untyped)",
        "Arel::Nodes::SqlLiteral | Arel::Nodes::BoundSqlLiteral",
    ),
];

/// `params.expect` (see [`ARMS`]).
const EXPECTED: &[(&str, &str)] = &[
    (
        "(**untyped)",
        "ActionController::Parameters | Array[untyped]",
    ),
    ("(Symbol)", "untyped"),
    ("(Symbol, Symbol, *untyped)", "untyped"),
];

/// What a framework class hands on to another object with a macro rubydex does not read in a gem:
/// `(owner, method, returns, the macro)`.
///
/// - **`ActiveModel::Errors`** writes `delegate :each, :clear, :empty?, :size, :uniq!, to: :@errors`,
///   an `Array`.
/// - **`has_one_attached`'s `ActiveStorage::Attached::One`** writes
///   `delegate_missing_to :attachment, allow_nil: true`, so without an attachment each is `nil`;
///   the attachment hands `filename` on to its blob the same way.
/// - **`ActionMailbox::Base`** writes `attr_reader :inbound_email` and
///   `delegate :mail, :delivered!, :bounced!, to: :inbound_email`, the same in 7.2, 8.0 and 8.1. A
///   mailbox is only ever built by `ActionMailbox::Base.receive(inbound_email)`, with an
///   `ActionMailbox::InboundEmail`, whose `mail` is `Mail.from_source(source)`.
const FORWARDED: [(&str, &str, &str, &str); 7] = [
    ("ActiveModel::Errors", "empty?", "bool", "delegate"),
    ("ActiveModel::Errors", "size", "Integer", "delegate"),
    (
        "ActiveStorage::Attached::One",
        "blob",
        "ActiveStorage::Blob?",
        "delegate_missing_to",
    ),
    (
        "ActiveStorage::Attached::One",
        "filename",
        "ActiveStorage::Filename?",
        "delegate_missing_to",
    ),
    (
        "ActiveStorage::Attached::One",
        "id",
        "Integer?",
        "delegate_missing_to",
    ),
    (
        "ActionMailbox::Base",
        "inbound_email",
        "ActionMailbox::InboundEmail",
        "attr_reader",
    ),
    ("ActionMailbox::Base", "mail", "Mail::Message", "delegate"),
];

/// A model's class methods, each written in one of ActiveRecord's concerns:
/// `(method, parameters, returns)`.
///
/// - `transaction` hands back its block's value, or `nil` where the block raised
///   `ActiveRecord::Rollback`, which the transaction swallows.
/// - `table_name` is `nil` on `ActiveRecord::Base` and on an abstract class with no table
///   (`reset_table_name`), and a frozen `String` everywhere else. `sequence_name` is `nil` where
///   the adapter names none. `quoted_table_name` quotes whatever `table_name` is.
/// - `column_names` is the schema's names, `table_exists?` the schema cache's answer.
const MODEL_CLASS_METHODS: [(&str, &str, &str); 10] = [
    ("arel_table", "()", "Arel::Table"),
    ("model_name", "()", "ActiveModel::Name"),
    ("sanitize_sql_array", "(Array[untyped])", "String"),
    ("sanitize_sql_like", "(String, ?String)", "String"),
    ("transaction", "[T] (**untyped) { () -> T }", "T?"),
    ("table_name", "()", "String?"),
    ("quoted_table_name", "()", "String"),
    ("sequence_name", "()", "String?"),
    ("column_names", "()", "Array[String]"),
    ("table_exists?", "()", "bool"),
];

/// What `Rails.env` is.
const INQUIRER: &str = "ActiveSupport::EnvironmentInquirer";

/// What a `datetime` column, `Time.current` and `Time.zone.now` are.
const ZONED: &str = "ActiveSupport::TimeWithZone";

/// `Time.current`, and anything else that reads `Time.zone` and falls back to `Time.now`.
const ZONED_OR_TIME: &str = "ActiveSupport::TimeWithZone | Time";

/// What `Time.zone` is.
const TIME_ZONE: &str = "ActiveSupport::TimeZone";

/// `TimeWithZone`'s own `def`s and aliases: `(method, parameters, returns)`.
///
/// `-` is either: another time gives the seconds between (`Float`), anything else a moved
/// `TimeWithZone`. `in_time_zone` hands back a plain `Time` for a `nil` or `false` zone.
const ZONE_DEFS: [(&str, &str, &str); 52] = [
    ("time", "()", "Time"),
    ("utc", "()", "Time"),
    ("getutc", "()", "Time"),
    ("getgm", "()", "Time"),
    ("gmtime", "()", "Time"),
    ("comparable_time", "()", "Time"),
    ("localtime", "(?untyped)", "Time"),
    ("getlocal", "(?untyped)", "Time"),
    ("to_time", "()", "Time"),
    ("to_datetime", "()", "DateTime"),
    ("to_i", "()", "Integer"),
    ("tv_sec", "()", "Integer"),
    ("to_f", "()", "Float"),
    ("to_r", "()", "Rational"),
    ("to_a", "()", "Array[untyped]"),
    ("hash", "()", "Integer"),
    ("utc_offset", "()", "Integer"),
    ("gmt_offset", "()", "Integer"),
    ("gmtoff", "()", "Integer"),
    ("zone", "()", "String"),
    ("formatted_offset", "(?untyped, ?untyped)", "String"),
    ("inspect", "()", "String"),
    ("to_s", "()", "String"),
    ("to_fs", "(?Symbol)", "String"),
    ("to_formatted_s", "(?Symbol)", "String"),
    ("strftime", "(String)", "String"),
    ("xmlschema", "(?Integer)", "String"),
    ("iso8601", "(?Integer)", "String"),
    ("rfc3339", "(?Integer)", "String"),
    ("httpdate", "()", "String"),
    ("rfc2822", "()", "String"),
    ("rfc822", "()", "String"),
    ("dst?", "()", "bool"),
    ("isdst", "()", "bool"),
    ("utc?", "()", "bool"),
    ("gmt?", "()", "bool"),
    ("past?", "()", "bool"),
    ("future?", "()", "bool"),
    ("today?", "()", "bool"),
    ("tomorrow?", "()", "bool"),
    ("yesterday?", "()", "bool"),
    ("between?", "(untyped, untyped)", "bool"),
    ("before?", "(untyped)", "bool"),
    ("after?", "(untyped)", "bool"),
    ("+", "(untyped)", ZONED),
    ("since", "(untyped)", ZONED),
    ("ago", "(untyped)", ZONED),
    ("-", "(untyped)", "ActiveSupport::TimeWithZone | Float"),
    ("advance", "(Hash[Symbol, untyped])", ZONED),
    ("change", "(Hash[Symbol, untyped])", ZONED),
    ("time_zone", "()", TIME_ZONE),
    ("in_time_zone", "(?untyped)", ZONED_OR_TIME),
];

/// `Time`'s calculations, which `TimeWithZone` answers through `method_missing` and wraps.
const ZONE_FORWARDS: [&str; 45] = [
    "beginning_of_day",
    "end_of_day",
    "midnight",
    "at_midnight",
    "at_beginning_of_day",
    "middle_of_day",
    "midday",
    "noon",
    "beginning_of_hour",
    "end_of_hour",
    "beginning_of_minute",
    "end_of_minute",
    "beginning_of_week",
    "end_of_week",
    "at_beginning_of_week",
    "at_end_of_week",
    "beginning_of_month",
    "end_of_month",
    "beginning_of_quarter",
    "end_of_quarter",
    "beginning_of_year",
    "end_of_year",
    "monday",
    "sunday",
    "tomorrow",
    "yesterday",
    "next_day",
    "prev_day",
    "next_week",
    "prev_week",
    "last_week",
    "next_month",
    "prev_month",
    "last_month",
    "next_year",
    "prev_year",
    "last_year",
    "days_ago",
    "days_since",
    "weeks_ago",
    "months_ago",
    "years_ago",
    "round",
    "floor",
    "ceil",
];

/// The same calculations that answer a `Range` of times, whose ends come back wrapped.
const ZONE_RANGES: [&str; 5] = [
    "all_day",
    "all_week",
    "all_month",
    "all_quarter",
    "all_year",
];

/// What `Time.zone` builds: `(method, parameters, returns)`.
const ZONE_BUILDERS: [(&str, &str, &str); 9] = [
    ("now", "()", ZONED),
    ("at", "(*untyped)", ZONED),
    ("local", "(*untyped)", ZONED),
    ("iso8601", "(String)", ZONED),
    (
        "parse",
        "(String, ?untyped)",
        "ActiveSupport::TimeWithZone?",
    ),
    (
        "strptime",
        "(String, String, ?untyped)",
        "ActiveSupport::TimeWithZone?",
    ),
    ("today", "()", "Date"),
    ("tomorrow", "()", "Date"),
    ("yesterday", "()", "Date"),
];

/// The constants a row's return spells, for asking the graph: each class of a union, without its
/// `?` or type arguments, and none of the words that are not constants, a method's own type
/// variable (`[T]` before its parameters) included.
fn names_in(parameters: &'static str, returns: &'static str) -> impl Iterator<Item = &'static str> {
    let variables: Vec<&str> = parameters
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .map(|(variables, _)| variables.split(',').map(str::trim).collect())
        .unwrap_or_default();
    returns
        .split('|')
        .map(|member| generic_head(member.trim().trim_end_matches('?')))
        .filter(move |name| {
            !matches!(
                *name,
                "bool" | "untyped" | "void" | "self" | "true" | "false" | "nil"
            ) && !variables.contains(name)
        })
}

/// Where [`NAMESPACES`] are answered.
pub const RAILTIE_CONFIGURATION: &str = "Rails::Railtie::Configuration";

/// Where ActiveSupport writes `try` and `try!` ([`TRIES`]), which `Object` and `Delegator`
/// include. `NilClass` writes its own pair, answering `nil`.
const TRYABLE: &str = "ActiveSupport::Tryable";

/// ActiveSupport's `try` and `try!`: `public_send` of their first argument, `try`'s only where the
/// receiver responds to it, so both return [`SENT`]. The same in 7.2, 8.0 and 8.1.
const TRIES: [&str; 2] = ["try", "try!"];

/// The class ya-lsp writes for what a controller's `helpers` hands back: [`VIEW`] with every helper
/// module the application writes included.
///
/// Rails makes that object at run time. The instance side is the controller's `view_context`, an
/// instance of `Class.new(ActionView::Base)` including the controller's `_helpers`; the class side
/// is `ActionView::Base.empty` extended with them. Neither class has a name, and `_helpers` holds
/// every application helper, since a direct subclass of `ActionController::Base` runs
/// `helper :all` (`ActionController::Railties::Helpers#inherited`). So the name is ya-lsp's, top
/// level like [`RELATION_BASE`](super::RELATION_BASE), and a project that declares it keeps its own
/// ([`helpers_hand_back`]).
pub const HELPER_PROXY: &str = "HelperProxy";

/// The view class the helper proxy is made from.
pub(super) const VIEW: &str = "ActionView::Base";

/// Where `helpers` is: `(owner, side)`. `ActionController::Helpers#helpers` is the controller's own
/// (`@_helper_proxy ||= view_context`), and the class object's comes from
/// `Helpers::ClassMethods`, which `ActionController::Base` extends.
const HELPERS: [(&str, Side); 2] = [
    ("ActionController::Helpers", Side::Instance),
    ("ActionController::Base", Side::Singleton),
];

/// The class `helpers` hands back, and [`HELPER_PROXY`]'s body where that is the class.
///
/// - **Where the application writes a helper module**, the proxy: `ApplicationController.helpers`
///   and a controller's `helpers` reach `cloud_cover_url` as well as `strip_tags`. It includes
///   every one, so `ActionController::Base.helpers`, whose `_helpers` Rails fills with none of
///   them, reaches them too: a call that raises in Ruby, so the answer holds wherever it returns.
///   So does `include_all_helpers = false`.
/// - **Where it writes none, or declares the name itself**, [`VIEW`], which holds the framework's.
/// - A helper `helper_method` or a gem's `helper` adds is in neither, and answers nothing.
fn helpers_hand_back(helpers: &[String], namespaces: &Namespaces) -> (&'static str, Facts) {
    let mut facts = Facts::default();
    if helpers.is_empty() || namespaces.declares(HELPER_PROXY) {
        return (VIEW, facts);
    }
    let owner = Owner::Instance(HELPER_PROXY.to_owned());
    facts.note(
        owner.clone(),
        "What a controller's `helpers` hands back: a view object holding every helper module the \
         application writes. Rails makes its class at run time; ya-lsp writes this one for it."
            .to_owned(),
    );
    facts.inherits(owner.clone(), VIEW.to_owned());
    for helper in helpers {
        facts.mixin(owner.clone(), helper.clone());
    }
    (HELPER_PROXY, facts)
}

/// Every constant a row of either table names, on either side of the arrow, and every namespace
/// above one.
///
/// Asked of the bundle once per pass, exactly as [`super::framework_classes`] is: a name missing
/// from the answer is a row that declares nothing. Both sides, not just the return, because
/// declaring `def self.root` on a `Rails` nothing else declares would *invent* the module: a
/// constant with one member and no place, where there was an honest miss. The namespaces above an
/// owner are asked for the same reason: a joined name introduces every segment above it
/// ([`Namespaces::spellable`]).
#[must_use]
pub fn framework_constants() -> Vec<&'static str> {
    let singletons = SINGLETONS
        .iter()
        .chain(&ASSIGNED)
        .flat_map(|(owner, _, returns)| [*owner, *returns]);
    let context = CONTEXT
        .iter()
        .flat_map(|(owner, _, returns, _)| [*owner, returns.trim_end_matches('?')]);
    let blocks = BLOCKS
        .iter()
        .flat_map(|(owner, _, _, parameters, returns, _)| {
            [*owner]
                .into_iter()
                .chain(names_in(parameters, returns))
                .chain(named_in(parameters))
        })
        .chain(CALLBACK_HOSTS);
    let config = CONFIG
        .iter()
        .flat_map(|(owner, _, _, returns, _)| [*owner, generic_head(returns)]);
    let namespaces = NAMESPACES
        .iter()
        .flat_map(|(_, gate, holds)| [*gate, *holds])
        .chain([RAILTIE_CONFIGURATION])
        .chain(HELPERS.iter().map(|(owner, _)| *owner))
        .chain([VIEW, HELPER_PROXY, TRYABLE]);
    let members = members()
        .into_iter()
        .flat_map(|(owner, _, _, parameters, returns, _)| {
            [owner].into_iter().chain(names_in(parameters, returns))
        });
    let arms = ARMS.iter().flat_map(|(owner, _, _, gate, arms)| {
        [*owner].into_iter().chain(*gate).chain(
            arms.iter()
                .flat_map(|(parameters, returns)| names_in(parameters, returns)),
        )
    });
    let mut names: Vec<&'static str> = singletons
        .chain(context)
        .chain(blocks)
        .chain(config)
        .chain(namespaces)
        .chain(members)
        .chain(arms)
        .flat_map(|name| {
            name.match_indices("::")
                .map(move |(at, _)| &name[..at])
                .chain([name])
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The classes a parameter list names in a `[self: …]`, the only place these rows name one.
fn named_in(parameters: &'static str) -> impl Iterator<Item = &'static str> {
    parameters.split("[self: ").skip(1).filter_map(|rest| {
        let name = rest.split(']').next()?;
        (!matches!(name, "self" | "instance")).then_some(name)
    })
}

/// A return's class without its type arguments: `Array[untyped]` is `Array`.
fn generic_head(returns: &str) -> &str {
    returns.split('[').next().unwrap_or(returns)
}

/// What every table's rows declare, for the rows this bundle can back, by the owner each is on.
///
/// - `namespaces` is what anything indexed declares: the application's own walk plus the bundle
///   answering [`framework_constants`].
/// - `application` is the class the project's own `config/application.rb` declares, when it
///   declares one.
/// - `helpers` is every helper module the application writes, which a controller's `helpers`
///   holds ([`helpers_hand_back`]).
///
/// A row whose owner or return is declared nowhere writes nothing. **Keyed by owner** because each
/// owner's rows are hosted on the file that declares it: the component's own file, so what a
/// component's rows say is written wherever that component is indexed, an application or not.
/// [`HELPER_PROXY`] goes with [`VIEW`], the class it is made from.
#[must_use]
pub fn read_framework(
    application: Option<&str>,
    helpers: &[String],
    connection: Option<&str>,
    namespaces: &Namespaces,
) -> BTreeMap<&'static str, Facts> {
    let mut by_owner: BTreeMap<&'static str, Facts> = BTreeMap::new();
    for (owner, name, returns) in SINGLETONS {
        // The project's own `Shop::Application` in place of the framework's base, and only for
        // the row the base belongs to. The substitution matters: an application's own class is
        // where `config.domain` and everything else a project hangs off `Rails.application` is
        // written. It needs no second opinion from the graph: it was read off a `class` line in the
        // application's own file.
        let (returns, declared) = match (returns, application) {
            (APPLICATION, Some(own)) => (own, true),
            _ => (returns, namespaces.declares(returns)),
        };
        if !declared || !namespaces.declares(owner) {
            continue;
        }
        by_owner.entry(owner).or_default().declare(Declared {
            owner: if namespaces.opens(owner) {
                Owner::ModuleSingleton(owner.to_owned())
            } else {
                Owner::Singleton(owner.to_owned())
            },
            name: name.to_owned(),
            returns: returns.to_owned(),
            parameters: "()".to_owned(),
            because: format!(
                "`{owner}.{name}` is a `{returns}`. The framework ships no signature for it, \
                 so ya-lsp writes the return type; the method itself is declared in the bundle."
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
        });
    }
    for (owner, name, returns, made) in CONTEXT {
        if !namespaces.declares(owner)
            || !namespaces.spellable(owner)
            || !namespaces.declares(returns.trim_end_matches('?'))
        {
            continue;
        }
        let said = match made {
            Made::Def | Made::PrivateDef => {
                "the method itself is declared in the bundle".to_owned()
            }
            Made::Macro(macro_name) => format!(
                "Rails makes the method with `{macro_name}`, which is not read in a gem, so it has \
                 no line to go to"
            ),
        };
        let harness = if returns == SESSION {
            " A controller test's harness stores a test session instead."
        } else {
            ""
        };
        by_owner.entry(owner).or_default().declare(Declared {
            owner: if namespaces.opens(owner) {
                Owner::Module(owner.to_owned())
            } else {
                Owner::Instance(owner.to_owned())
            },
            name: name.to_owned(),
            returns: returns.to_owned(),
            parameters: "()".to_owned(),
            because: format!(
                "`{name}` is a `{returns}`. The framework ships no signature for it, so ya-lsp \
                 writes the return type; {said}.{harness}"
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: made == Made::PrivateDef,
        });
    }
    let owned = |owner: &str, side: Side| match (side, namespaces.opens(owner)) {
        (Side::Instance, false) => Owner::Instance(owner.to_owned()),
        (Side::Instance, true) => Owner::Module(owner.to_owned()),
        (Side::Singleton, false) => Owner::Singleton(owner.to_owned()),
        (Side::Singleton, true) => Owner::ModuleSingleton(owner.to_owned()),
    };
    let said = |made: Made| match made {
        Made::Def | Made::PrivateDef => "the method itself is declared in the bundle".to_owned(),
        Made::Macro(macro_name) => format!(
            "Rails makes the method with `{macro_name}`, which is not read in a gem, so it has no \
             line to go to"
        ),
    };
    let backed = |owner: &str, names: &[&str]| {
        namespaces.declares(owner)
            && namespaces.spellable(owner)
            && names
                .iter()
                .all(|name| namespaces.declares(generic_head(name)))
    };
    for (owner, name, returns) in ASSIGNED {
        if !backed(owner, &[returns]) {
            continue;
        }
        by_owner.entry(owner).or_default().declare(Declared {
            owner: owned(owner, Side::Singleton),
            name: name.to_owned(),
            returns: format!("{returns} | {WRITTEN}"),
            parameters: "()".to_owned(),
            because: format!(
                "`{owner}.{name}` is the `{returns}` Rails sets at boot, or what the application \
                 assigns it after. The framework ships no signature for it, so ya-lsp writes the \
                 return type; the method itself is declared in the bundle."
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
        });
    }
    for (owner, side, name, parameters, returns, made) in BLOCKS {
        let named: Vec<&str> = named_in(parameters)
            .chain(names_in(parameters, returns))
            .collect();
        if !backed(owner, &named) {
            continue;
        }
        by_owner.entry(owner).or_default().declare(Declared {
            owner: owned(owner, side),
            name: name.to_owned(),
            returns: returns.to_owned(),
            parameters: parameters.to_owned(),
            because: format!(
                "`{name}` runs its block against something other than the code around it, which \
                 the framework's own signature would say and it ships none, so ya-lsp writes it; \
                 {}.",
                said(made)
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
        });
    }
    for host in CALLBACK_HOSTS {
        if !backed(host, &[]) {
            continue;
        }
        for name in super::blocks::CONTROLLER_CALLBACKS {
            by_owner.entry(host).or_default().declare(Declared {
                owner: owned(host, Side::Singleton),
                name: name.to_owned(),
                returns: "void".to_owned(),
                parameters: super::blocks::controller_callback(),
                because: format!(
                    "`{name}` runs its block and its `if:` and `unless:` lambdas against the \
                     controller; {}.",
                    said(Made::Macro("define_method"))
                ),
                at: None,
                from: Source::Interface,
                overloads: Vec::new(),
                private: false,
            });
        }
    }
    for (owner, side, name, returns, made) in CONFIG {
        if !backed(owner, &[returns]) {
            continue;
        }
        by_owner.entry(owner).or_default().declare(Declared {
            owner: owned(owner, side),
            name: name.to_owned(),
            returns: returns.to_owned(),
            parameters: "()".to_owned(),
            because: format!(
                "`{name}` is a `{returns}`. The framework ships no signature for it, so ya-lsp \
                 writes the return type; {}.",
                said(made)
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
        });
    }
    for (owner, side, name, parameters, returns, made) in members() {
        let named: Vec<&str> = names_in(parameters, returns).collect();
        if !backed(owner, &named) {
            continue;
        }
        by_owner.entry(owner).or_default().declare(Declared {
            owner: owned(owner, side),
            name: name.to_owned(),
            returns: returns.to_owned(),
            parameters: parameters.to_owned(),
            because: format!(
                "`{name}` is a `{returns}`. The framework ships no signature for it, so ya-lsp \
                 writes the return type; {}.",
                said(made)
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
        });
    }
    for (owner, side, name, gate, arms) in ARMS {
        let named: Vec<&str> = arms
            .iter()
            .flat_map(|(parameters, returns)| names_in(parameters, returns))
            .collect();
        // Every row writes at least one arm.
        let ((parameters, returns), rest) = (arms[0], &arms[1..]);
        if gate.is_some_and(|gate| !namespaces.declares(gate)) || !backed(owner, &named) {
            continue;
        }
        by_owner.entry(owner).or_default().declare(Declared {
            owner: owned(owner, side),
            name: name.to_owned(),
            returns: (*returns).to_owned(),
            parameters: (*parameters).to_owned(),
            because: format!(
                "What `{name}` hands back depends on the call, so ya-lsp writes one arm per shape \
                 of call; {}.",
                said(Made::Def)
            ),
            at: None,
            from: Source::Interface,
            overloads: rest
                .iter()
                .map(|(parameters, returns)| ((*parameters).to_owned(), (*returns).to_owned()))
                .collect(),
            private: false,
        });
    }
    if backed(TRYABLE, &[]) {
        for name in TRIES {
            by_owner.entry(TRYABLE).or_default().declare(Declared {
                owner: owned(TRYABLE, Side::Instance),
                name: name.to_owned(),
                returns: SENT.to_owned(),
                parameters: "(*untyped) ?{ (?) -> untyped }".to_owned(),
                because: format!(
                    "`{name}(:name, …)` calls `name` publicly on the receiver, so it answers what \
                     that call does; {}.",
                    said(Made::Def)
                ),
                at: None,
                from: Source::Interface,
                overloads: Vec::new(),
                private: false,
            });
        }
    }
    for (name, gate, holds) in NAMESPACES {
        if !namespaces.declares(gate) || !backed(RAILTIE_CONFIGURATION, &[holds]) {
            continue;
        }
        by_owner
            .entry(RAILTIE_CONFIGURATION)
            .or_default()
            .declare(Declared {
                owner: owned(RAILTIE_CONFIGURATION, Side::Instance),
                name: name.to_owned(),
                returns: holds.to_owned(),
                parameters: "()".to_owned(),
                because: format!(
                    "`config.{name}` is the `{holds}` `{gate}`'s railtie assigns; {}.",
                    said(Made::Macro("method_missing"))
                ),
                at: None,
                from: Source::Interface,
                overloads: Vec::new(),
                private: false,
            });
    }
    // Only from 7.2 (`LEASING`): 7.0 has no `dirty_current_transaction`.
    if namespaces.declares(LEASING) && backed(DATABASE_STATEMENTS, &[]) {
        for (name, parameters, returns) in adapters::transaction_rows() {
            by_owner
                .entry(DATABASE_STATEMENTS)
                .or_default()
                .declare(Declared {
                    owner: owned(DATABASE_STATEMENTS, Side::Instance),
                    name: name.to_owned(),
                    returns,
                    parameters,
                    because: format!(
                        "`{name}` is what the adapter's transaction manager answers; {}.",
                        said(Made::Macro("delegate"))
                    ),
                    at: None,
                    from: Source::Interface,
                    overloads: Vec::new(),
                    private: false,
                });
        }
    }
    if let Some(adapter) = connection
        && backed(CONNECTION_HANDLING, &[])
    {
        let (leasing, pool) = (
            namespaces.declares(LEASING),
            namespaces.declares(CONNECTION_POOL),
        );
        for (name, parameters, returns) in adapters::rows(adapter, leasing, pool) {
            by_owner
                .entry(CONNECTION_HANDLING)
                .or_default()
                .declare(Declared {
                    owner: owned(CONNECTION_HANDLING, Side::Instance),
                    name: name.to_owned(),
                    returns,
                    parameters,
                    because: format!(
                        "`{name}` hands back the application's connection, a `{adapter}` by its \
                         `config/database.yml` and the adapters its bundle loads; {}.",
                        said(Made::Def)
                    ),
                    at: None,
                    from: Source::Interface,
                    overloads: Vec::new(),
                    private: false,
                });
        }
        if pool {
            let facts = by_owner.entry(CONNECTION_POOL).or_default();
            let owner = Owner::Instance(CONNECTION_POOL.to_owned());
            facts.generic(owner.clone(), "[A]".to_owned());
            facts.note(
                owner.clone(),
                "`A` is the adapter a pool's connections are, which ya-lsp writes; Rails' class \
                 has no type parameter."
                    .to_owned(),
            );
            for (name, parameters, returns) in adapters::pool_rows(leasing) {
                facts.declare(Declared {
                    owner: owner.clone(),
                    name: name.to_owned(),
                    returns,
                    parameters,
                    because: String::new(),
                    at: None,
                    from: Source::Interface,
                    overloads: Vec::new(),
                    private: false,
                });
            }
        }
    }
    if backed(VIEW, &[]) {
        let (returns, proxy) = helpers_hand_back(helpers, namespaces);
        if !proxy.is_empty() {
            by_owner.entry(VIEW).or_default().extend(proxy);
        }
        for (owner, side) in HELPERS {
            if !backed(owner, &[]) {
                continue;
            }
            by_owner.entry(owner).or_default().declare(Declared {
                owner: owned(owner, side),
                name: "helpers".to_owned(),
                returns: returns.to_owned(),
                parameters: "()".to_owned(),
                because: format!(
                    "`helpers` is a view object holding the application's helper modules, made at \
                     run time, so ya-lsp writes it as a `{returns}`; {}.",
                    said(Made::Def)
                ),
                at: None,
                from: Source::Interface,
                overloads: Vec::new(),
                private: false,
            });
        }
    }
    by_owner
}

/// The class a `config/application.rb` writes `< Rails::Application` under, with its nesting.
///
/// `module Shop; class Application < Rails::Application` answers `Shop::Application`.
/// `None` for a file that declares none (every file but that one), and for an engine, which has no
/// such file; the caller then keeps [`APPLICATION`].
#[must_use]
pub fn application_class(source: &str) -> Option<String> {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut walker = Walker {
        source,
        nesting: Vec::new(),
        found: None,
    };
    walker.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
    );
    walker.found
}

/// The `class`/`module` walk [`application_class`] is, and nothing else.
struct Walker<'src> {
    source: &'src str,
    nesting: Vec<String>,
    found: Option<String>,
}

impl Walker<'_> {
    /// One body, and then the class and module bodies written as statements of it.
    ///
    /// **Statements, not a walk of the whole tree**: [`super::entrypoints`]' rule, for its reason.
    /// A generic visit descends into every method body in the file and overflows a 2 MiB stack on a
    /// large one. A `class` inside an `if` is not a statement of the body, and a
    /// `config/application.rb` does not write one.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        for statement in statements.body().iter() {
            let (path, inner) = if let Some(class) = statement.as_class_node() {
                if class
                    .superclass()
                    .map(|superclass| constant_spelling(self.source, &superclass))
                    .as_deref()
                    == Some(APPLICATION)
                {
                    let mut nesting = self.nesting.clone();
                    nesting.push(constant_spelling(self.source, &class.constant_path()));
                    self.found = Some(nesting.join("::"));
                    return;
                }
                (class.constant_path(), class.body())
            } else if let Some(module) = statement.as_module_node() {
                (module.constant_path(), module.body())
            } else {
                continue;
            };
            self.nesting.push(constant_spelling(self.source, &path));
            self.walk(inner);
            self.nesting.pop();
            // After the recursion, not before it: the first application class in the file is the
            // answer, and a second `module` beside the one that held it must not be walked into on
            // the way out.
            if self.found.is_some() {
                return;
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{
        APPLICATION, CONNECTION_POOL, DATABASE_STATEMENTS, LEASING, application_class,
        framework_constants, read_framework,
    };
    use crate::generated::{Facts, Namespaces, Owner, declaring, declaring_kinds};

    /// Every owner's rows in one document, in owner order, as a whole-table fixture reads best.
    fn all(application: Option<&str>, namespaces: &Namespaces) -> Facts {
        let mut all = Facts::default();
        for facts in read_framework(application, &[], None, namespaces).into_values() {
            all.extend(facts);
        }
        all
    }

    /// Everything a real bundle declares, spelled as the graph spells it: railties writes
    /// `module Rails`, and `Time`, `Pathname` and the two activesupport classes are classes.
    fn bundle() -> Namespaces {
        declaring_kinds(
            &[
                "Time",
                "Pathname",
                "ActiveSupport::Cache::Store",
                "ActiveSupport::TimeZone",
                APPLICATION,
            ],
            &["Rails"],
        )
    }

    fn rbs(application: Option<&str>, namespaces: &Namespaces) -> String {
        all(application, namespaces)
            .render(&declaring(&["Rails"]))
            .rbs
    }

    /// The whole of what the table declares, pinned as a document.
    ///
    /// Both bodies, both keywords, and the sentence each member carries: the shape every other test
    /// here reads one line out of.
    #[test]
    fn the_rbs_the_framework_table_declares() {
        assert_eq!(
            rbs(None, &bundle()),
            "\
module Rails
  # `Rails.root` is a `Pathname`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.root: () -> Pathname
  # `Rails.cache` is a `ActiveSupport::Cache::Store`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.cache: () -> ActiveSupport::Cache::Store
  # `Rails.application` is a `Rails::Application`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.application: () -> Rails::Application
end
class Time
  # `Time.zone` is a `ActiveSupport::TimeZone`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.zone: () -> ActiveSupport::TimeZone
end
"
        );
    }

    /// The application's own class displaces the framework's base, and only for that row.
    #[test]
    fn the_projects_own_application_class_is_what_rails_application_returns() {
        let rbs = rbs(Some("Shop::Application"), &bundle());
        assert!(
            rbs.contains("def self.application: () -> Shop::Application"),
            "{rbs}"
        );
        assert!(rbs.contains("def self.root: () -> Pathname"), "{rbs}");
    }

    /// An application class nothing else declares is still written, because it was read off a
    /// `class` line in the application's own file, not looked up.
    #[test]
    fn the_projects_own_class_needs_no_second_opinion() {
        let rbs = rbs(Some("Shop::Application"), &declaring(&["Rails"]));
        assert_eq!(
            rbs,
            "\
module Rails
  # `Rails.application` is a `Shop::Application`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.application: () -> Shop::Application
end
"
        );
    }

    /// A bundle that is not indexed declares nothing, instead of inventing a `Rails` with one
    /// member and no place.
    #[test]
    fn nothing_is_declared_on_an_owner_nothing_else_declares() {
        assert_eq!(rbs(None, &Namespaces::default()), "");
    }

    /// The owner is asked about even when the return is there: the case the test above cannot
    /// reach, because a bundle with neither declines on the return first.
    #[test]
    fn an_owner_nothing_declares_is_declined_though_its_return_is_known() {
        assert_eq!(rbs(None, &declaring_kinds(&["Pathname"], &[])), "");
    }

    /// A row whose return class the bundle does not hold is the only one dropped.
    #[test]
    fn a_row_whose_return_is_missing_declines_on_its_own() {
        let rbs = rbs(None, &declaring_kinds(&["Pathname"], &["Rails"]));
        assert_eq!(
            rbs,
            "\
module Rails
  # `Rails.root` is a `Pathname`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.root: () -> Pathname
end
"
        );
    }

    /// Which keyword opens a body is the graph's answer, not the table's: a `Rails` every file
    /// spells `class` is opened with `class`.
    #[test]
    fn the_keyword_that_opens_a_body_is_read_rather_than_assumed() {
        let rbs = rbs(
            None,
            &declaring_kinds(
                &["Rails", "Pathname", "Time", "ActiveSupport::TimeZone"],
                &[],
            ),
        );
        assert!(rbs.starts_with("class Rails\n"), "{rbs}");
        assert!(rbs.contains("class Time\n"), "{rbs}");
    }

    /// `Rails.env`'s predicates and `TimeWithZone`'s members, each on the class Rails writes it
    /// on, with the macro that makes it named where rubydex cannot see the method. A row whose
    /// return the bundle lacks is the only one dropped: without `DateTime`, `to_datetime` goes and
    /// the rest stay.
    #[test]
    fn what_rails_env_and_a_time_with_zone_answer() {
        let every = [
            super::INQUIRER,
            super::ZONED,
            super::TIME_ZONE,
            "Time",
            "Date",
            "Integer",
            "Float",
            "Rational",
            "String",
            "Range",
            "Array",
            "ActiveRecord::Base",
            "Arel::Table",
            "ActiveModel::Name",
        ];
        let rbs = rbs(
            None,
            &declaring_kinds(
                &every,
                &[
                    "Rails",
                    "ActiveSupport",
                    "ActiveRecord",
                    "Arel",
                    "ActiveModel",
                ],
            ),
        );
        for line in [
            "class ActiveSupport::EnvironmentInquirer\n",
            "  def test?: () -> bool\n",
            "  def local?: () -> bool\n",
            "class ActiveSupport::TimeWithZone\n",
            "  def year: () -> Integer\n",
            "  def to_date: () -> Date\n",
            "  def strftime: (String) -> String\n",
            "  def beginning_of_day: (*untyped) -> ActiveSupport::TimeWithZone\n",
            "  def all_day: (*untyped) -> Range[ActiveSupport::TimeWithZone]\n",
            "class ActiveSupport::TimeZone\n",
            "  def parse: (String, ?untyped) -> ActiveSupport::TimeWithZone?\n",
            "  def self.current: () -> (ActiveSupport::TimeWithZone | Time)\n",
            "  def self.current: () -> Date\n",
            // A method's own type variable is no constant to ask the bundle about.
            "  def self.transaction: [T] (**untyped) { () -> T } -> T?\n",
            "  def self.arel_table: () -> Arel::Table\n",
            "  def self.table_name: () -> String?\n",
            "  def self.table_exists?: () -> bool\n",
        ] {
            assert!(rbs.contains(line), "{line}{rbs}");
        }
        // `class_eval` and `method_missing` make a method no line holds, and the card says so.
        assert!(
            rbs.contains("Rails makes the method with `class_eval`, which is not read in a gem"),
            "{rbs}"
        );
        // A union return is bracketed, or RBS would read its `|` as a second overload and refuse
        // the whole document.
        assert!(
            rbs.contains("  def -: (untyped) -> (ActiveSupport::TimeWithZone | Float)\n"),
            "{rbs}"
        );
        assert!(
            crate::analysis::types::Types::new().harvest("file:///framework.rbs", &rbs),
            "{rbs}"
        );
        assert!(!rbs.contains("def to_datetime"), "{rbs}");
        let with = rbs_of(&every, "DateTime");
        assert!(
            with.contains("  def to_datetime: () -> DateTime\n"),
            "{with}"
        );
    }

    fn rbs_of(every: &[&str], also: &str) -> String {
        let mut names = every.to_vec();
        names.push(also);
        rbs(
            None,
            &declaring_kinds(
                &names,
                &[
                    "Rails",
                    "ActiveSupport",
                    "ActiveRecord",
                    "Arel",
                    "ActiveModel",
                ],
            ),
        )
    }

    /// Both ends of every row and every namespace above one, deduplicated, which is what the bundle
    /// is asked about. A `?` is the return's, not the constant's.
    #[test]
    fn every_constant_a_row_names_is_asked_of_the_bundle() {
        assert_eq!(
            framework_constants(),
            vec![
                "ActionCable",
                "ActionController",
                "ActionController::API",
                "ActionController::Base",
                "ActionController::Cookies",
                "ActionController::ExpectedParameterMissing",
                "ActionController::Flash",
                "ActionController::Helpers",
                "ActionController::Instrumentation",
                "ActionController::Metal",
                "ActionController::Parameters",
                "ActionController::Redirecting",
                "ActionController::StrongParameters",
                "ActionDispatch",
                "ActionDispatch::Cookies",
                "ActionDispatch::Cookies::CookieJar",
                "ActionDispatch::Flash",
                "ActionDispatch::Flash::FlashHash",
                "ActionDispatch::Flash::FlashNow",
                "ActionDispatch::Http",
                "ActionDispatch::Http::MimeNegotiation",
                "ActionDispatch::Http::URL",
                "ActionDispatch::Request",
                "ActionDispatch::Request::Session",
                "ActionDispatch::Response",
                "ActionDispatch::Routing",
                "ActionDispatch::Routing::Mapper",
                "ActionDispatch::Routing::RouteSet",
                "ActionMailbox",
                "ActionMailbox::Base",
                "ActionMailbox::InboundEmail",
                "ActionMailer",
                "ActionMailer::Base",
                "ActionMailer::MessageDelivery",
                "ActionText",
                "ActionView",
                "ActionView::Base",
                "ActionView::Helpers",
                "ActionView::Helpers::ControllerHelper",
                "ActionView::Helpers::SanitizeHelper",
                "ActiveJob",
                "ActiveJob::Base",
                "ActiveModel",
                "ActiveModel::Attributes",
                "ActiveModel::Errors",
                "ActiveModel::Name",
                "ActiveModel::Naming",
                "ActiveRecord",
                "ActiveRecord::AttributeMethods",
                "ActiveRecord::Base",
                "ActiveRecord::ConnectionAdapters",
                "ActiveRecord::ConnectionAdapters::DatabaseStatements",
                "ActiveRecord::ConnectionAdapters::Mysql2",
                "ActiveRecord::ConnectionAdapters::Mysql2::DatabaseStatements",
                "ActiveRecord::ConnectionAdapters::QueryCache",
                "ActiveRecord::Persistence",
                "ActiveRecord::Result",
                "ActiveRecord::Suppressor",
                "ActiveRecord::Transactions",
                "ActiveRecord::Validations",
                "ActiveStorage",
                "ActiveStorage::Attached",
                "ActiveStorage::Attached::One",
                "ActiveStorage::Blob",
                "ActiveStorage::Filename",
                "ActiveSupport",
                "ActiveSupport::BroadcastLogger",
                "ActiveSupport::Cache",
                "ActiveSupport::Cache::Store",
                "ActiveSupport::Duration",
                "ActiveSupport::EncryptedConfiguration",
                "ActiveSupport::EnvironmentInquirer",
                "ActiveSupport::OrderedOptions",
                "ActiveSupport::SafeBuffer",
                "ActiveSupport::TimeWithZone",
                "ActiveSupport::TimeZone",
                "ActiveSupport::Tryable",
                "Arel",
                "Arel::Nodes",
                "Arel::Nodes::BoundSqlLiteral",
                "Arel::Nodes::SqlLiteral",
                "Arel::Table",
                "Array",
                "Date",
                "DateTime",
                "Exception",
                "FactoryBot",
                "Float",
                "GlobalID",
                "Hash",
                "HelperProxy",
                "I18n",
                "Importmap",
                "Integer",
                "Lograge",
                "Lograge::OrderedOptions",
                "Mail",
                "Mail::Message",
                "Mime",
                "Mime::NullType",
                "Mime::Type",
                "MissionControl",
                "Pathname",
                "Propshaft",
                "Rack",
                "Rack::Request",
                "Rack::Request::Env",
                "Rails",
                "Rails::Application",
                "Rails::Application::Configuration",
                "Rails::Application::Configuration::Custom",
                "Rails::Configuration",
                "Rails::Configuration::MiddlewareStackProxy",
                "Rails::Engine",
                "Rails::Engine::Configuration",
                "Rails::Paths",
                "Rails::Paths::Root",
                "Rails::Railtie",
                "Rails::Railtie::Configuration",
                "Range",
                "Rational",
                "SolidCache",
                "SolidQueue",
                "String",
                "Time",
                "Turbo",
            ]
        );
    }

    /// actionpack and actionview as the graph spells them, less `missing`: the namespaces and the
    /// four mixins are modules, `Metal` and every return a class.
    fn actionpack_without(missing: &[&str]) -> Namespaces {
        let keep = |names: &[&'static str]| -> Vec<&'static str> {
            names
                .iter()
                .copied()
                .filter(|name| !missing.contains(name))
                .collect()
        };
        declaring_kinds(
            &keep(&[
                "ActionController::Metal",
                "ActionController::Parameters",
                "ActionDispatch::Request",
                "ActionDispatch::Request::Session",
                "ActionDispatch::Response",
                "ActionDispatch::Flash::FlashHash",
                "ActionDispatch::Cookies::CookieJar",
            ]),
            &keep(&[
                "ActionController",
                "ActionController::StrongParameters",
                "ActionController::Flash",
                "ActionController::Cookies",
                "ActionDispatch",
                "ActionDispatch::Flash",
                "ActionDispatch::Cookies",
                "ActionView",
                "ActionView::Helpers",
                "ActionView::Helpers::ControllerHelper",
            ]),
        )
    }

    fn actionpack() -> Namespaces {
        actionpack_without(&[])
    }

    /// What `helpers` is on either side, over a bundle holding `also` beside the three it needs.
    fn helpers_rbs(helpers: &[&str], also: &[&'static str]) -> String {
        let helpers: Vec<String> = helpers.iter().map(|name| (*name).to_owned()).collect();
        let classes: Vec<&str> = ["ActionController::Base", "ActionView::Base"]
            .into_iter()
            .chain(also.iter().copied())
            .collect();
        let bundle = declaring_kinds(
            &classes,
            &[
                "ActionController",
                "ActionController::Helpers",
                "ActionView",
            ],
        );
        let mut all = Facts::default();
        for facts in read_framework(None, &helpers, None, &bundle).into_values() {
            all.extend(facts);
        }
        all.render(&declaring(&["ActionController", "ActionView"]))
            .rbs
    }

    /// A controller's `helpers`, and its class object's, are a view object holding every helper
    /// module the application writes: [`HELPER_PROXY`], made from `ActionView::Base`.
    ///
    /// With no helper module, or where the project declares the name itself, both are the
    /// framework's `ActionView::Base`; without `ActionView::Base` in the bundle, neither is said.
    #[test]
    fn a_controllers_helpers_holds_the_applications_helper_modules() {
        let rbs = helpers_rbs(&["Admin::UsersHelper", "ApplicationHelper"], &[]);
        for written in [
            "  # `helpers` is a view object holding the application's helper modules, made at run \
             time, so ya-lsp writes it as a `HelperProxy`; the method itself is declared in the \
             bundle.\n  def self.helpers: () -> HelperProxy\nend\n",
            "module Helpers\n  # `helpers` is a view object holding the application's helper \
             modules, made at run time, so ya-lsp writes it as a `HelperProxy`; the method itself \
             is declared in the bundle.\n  def helpers: () -> HelperProxy\nend\n",
            "class HelperProxy < ActionView::Base\n  # What a controller's `helpers` hands back: a \
             view object holding every helper module the application writes. Rails makes its \
             class at run time; ya-lsp writes this one for it.\n  include Admin::UsersHelper\n  \
             include ApplicationHelper\nend\n",
        ] {
            assert!(rbs.contains(written), "{written}\n---\n{rbs}");
        }
        assert!(
            crate::analysis::types::Types::new().harvest("file:///helpers.rbs", &rbs),
            "{rbs}"
        );
        for (helpers, also) in [
            (&[][..], &[][..]),
            (&["ApplicationHelper"][..], &["HelperProxy"][..]),
        ] {
            let rbs = helpers_rbs(helpers, also);
            assert!(
                rbs.contains("  def helpers: () -> ActionView::Base\n"),
                "{rbs}"
            );
            assert!(
                rbs.contains("  def self.helpers: () -> ActionView::Base\n"),
                "{rbs}"
            );
            assert!(!rbs.contains("class HelperProxy"), "{rbs}");
        }
        // An owner the bundle lacks loses its own row and no other.
        let bundle = declaring_kinds(
            &["ActionController::Base", "ActionView::Base"],
            &["ActionController", "ActionView"],
        );
        let rows = read_framework(None, &["ApplicationHelper".to_owned()], None, &bundle);
        assert!(!rows.contains_key("ActionController::Helpers"));
        assert_eq!(
            rows["ActionController::Base"].returns(
                &Owner::Singleton("ActionController::Base".to_owned()),
                "helpers"
            ),
            Some("HelperProxy")
        );
        let bundle = declaring_kinds(
            &["ActionController::Base"],
            &["ActionController", "ActionController::Helpers"],
        );
        let viewless = read_framework(None, &["ApplicationHelper".to_owned()], None, &bundle);
        assert!(
            viewless.values().all(|facts| facts
                .returns(
                    &Owner::Module("ActionController::Helpers".to_owned()),
                    "helpers"
                )
                .is_none()),
            "no view class, no answer"
        );
    }

    /// The whole of the second table, pinned as a document: which body each row opens, with which
    /// keyword, and the sentence each member carries. Owners come in name order, one body each.
    #[test]
    fn the_rbs_the_context_table_declares() {
        assert_eq!(
            all(None, &actionpack()).render(&declaring(&[])).rbs,
            "\
module ActionController::Cookies
  # `cookies` is a `ActionDispatch::Cookies::CookieJar`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  private def cookies: () -> ActionDispatch::Cookies::CookieJar
end
module ActionController::Flash
  # `flash` is a `ActionDispatch::Flash::FlashHash`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `delegate`, which is not read in a gem, so it has no line to go to.
  def flash: () -> ActionDispatch::Flash::FlashHash
end
class ActionController::Metal
  # `request` is a `ActionDispatch::Request`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `attr_internal`, which is not read in a gem, so it has no line to go to.
  def request: () -> ActionDispatch::Request
  # `response` is a `ActionDispatch::Response`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `attr_internal_reader`, which is not read in a gem, so it has no line to go to.
  def response: () -> ActionDispatch::Response
  # `session` is a `ActionDispatch::Request::Session`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `delegate`, which is not read in a gem, so it has no line to go to. A controller test's harness stores a test session instead.
  def session: () -> ActionDispatch::Request::Session
end
module ActionController::StrongParameters
  # `params` is a `ActionController::Parameters`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def params: () -> ActionController::Parameters
end
module ActionView::Helpers::ControllerHelper
  # `request` is a `ActionDispatch::Request?`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `attr_internal`, which is not read in a gem, so it has no line to go to.
  def request: () -> ActionDispatch::Request?
  # `response` is a `ActionDispatch::Response`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `delegate`, which is not read in a gem, so it has no line to go to.
  def response: () -> ActionDispatch::Response
  # `session` is a `ActionDispatch::Request::Session`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `delegate`, which is not read in a gem, so it has no line to go to. A controller test's harness stores a test session instead.
  def session: () -> ActionDispatch::Request::Session
  # `flash` is a `ActionDispatch::Flash::FlashHash`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `delegate`, which is not read in a gem, so it has no line to go to.
  def flash: () -> ActionDispatch::Flash::FlashHash
  # `cookies` is a `ActionDispatch::Cookies::CookieJar`. The framework ships no signature for it, so ya-lsp writes the return type; Rails makes the method with `delegate`, which is not read in a gem, so it has no line to go to.
  def cookies: () -> ActionDispatch::Cookies::CookieJar
end
"
        );
    }

    /// Each owner's rows are their own facts, to be hosted on the file that declares that owner.
    #[test]
    fn the_rows_come_apart_by_the_owner_they_are_on() {
        let owners: Vec<&str> = read_framework(None, &[], None, &actionpack())
            .into_keys()
            .collect();
        assert_eq!(
            owners,
            [
                "ActionController::Cookies",
                "ActionController::Flash",
                "ActionController::Metal",
                "ActionController::StrongParameters",
                "ActionView::Helpers::ControllerHelper",
            ]
        );
    }

    /// Each row stands on its own three checks: its owner declared, spellable without inventing a
    /// namespace, and its return declared.
    #[test]
    fn a_context_row_declines_on_any_of_its_three_ends() {
        let rows = |namespaces: &Namespaces| {
            all(None, namespaces)
                .render(&declaring(&[]))
                .rbs
                .lines()
                .filter(|line| !line.trim_start().starts_with('#') && line.contains("def "))
                .map(|line| line.trim().to_owned())
                .collect::<Vec<_>>()
        };
        // No `ActionController::Parameters`: only the row returning it goes.
        let without_return = rows(&actionpack_without(&["ActionController::Parameters"]));
        assert!(
            !without_return
                .iter()
                .any(|row| row.starts_with("def params"))
        );
        assert!(
            without_return
                .iter()
                .any(|row| row.starts_with("def request"))
        );
        // No `ActionController::Flash`: only the row on it goes.
        let without_owner = rows(&actionpack_without(&["ActionController::Flash"]));
        assert_eq!(without_owner.len(), 10, "{without_owner:?}");
        // No `ActionView::Helpers` above the view context: writing
        // `ActionView::Helpers::ControllerHelper` would invent it, so every row on it goes.
        let unspellable = rows(&actionpack_without(&["ActionView::Helpers"]));
        assert_eq!(unspellable.len(), 6, "{unspellable:?}");
    }

    /// The application class, with the nesting it is written in.
    #[test]
    fn the_application_class_is_spelled_with_its_nesting() {
        assert_eq!(
            application_class(
                "module Shop\n  class Application < Rails::Application\n  end\nend\n"
            ),
            Some("Shop::Application".to_owned())
        );
    }

    /// A class written at top level, and one nested two deep.
    #[test]
    fn the_application_class_is_found_at_any_depth() {
        assert_eq!(
            application_class("class Application < Rails::Application\nend\n"),
            Some("Application".to_owned())
        );
        assert_eq!(
            application_class(
                "module A\n  module B\n    class App < Rails::Application\n    end\n  end\nend\n"
            ),
            Some("A::B::App".to_owned())
        );
    }

    /// A class inside another class: not a shape Rails generates, and still walked. The walk
    /// descends into every class and module body, not only the ones a convention expects.
    #[test]
    fn a_class_nested_in_a_class_is_reached() {
        assert_eq!(
            application_class("class Outer\n  class App < Rails::Application\n  end\nend\n"),
            Some("Outer::App".to_owned())
        );
    }

    /// Everything that is not the shape: no superclass, a different one, a body with no class, an
    /// empty body, and an empty file.
    #[test]
    fn nothing_but_a_rails_application_subclass_answers() {
        assert_eq!(application_class("class Application\nend\n"), None);
        assert_eq!(application_class("class App < Sinatra::Base\nend\n"), None);
        assert_eq!(application_class("require \"rails\"\n"), None);
        assert_eq!(application_class("module Shop\nend\n"), None);
        assert_eq!(application_class(""), None);
    }

    /// The first one wins, and the module beside the one that held it is not walked into.
    #[test]
    fn the_first_application_class_is_the_answer() {
        assert_eq!(
            application_class(
                "module A\n  class App < Rails::Application\n  end\nend\n\
                 module B\n  class App < Rails::Application\n  end\nend\n"
            ),
            Some("A::App".to_owned())
        );
    }
    /// A bundle holding everything the block and `config` rows name, with three of the 22
    /// namespaces' gates: `ActiveRecord`, `Turbo` and lograge's `Lograge`.
    /// A row whose answer depends on the call is one overload set, a record's `save` is written on
    /// each module a model's lookup may reach first, and `params.expect` only where the bundle's
    /// Rails writes it.
    #[test]
    fn a_row_whose_answer_depends_on_the_call_is_an_overload_set() {
        let classes = [
            "ActiveSupport::Duration",
            "ActiveSupport::TimeWithZone",
            "Time",
            "Date",
            "DateTime",
            "ActionController::Parameters",
            "Array",
            "Integer",
        ];
        let modules = [
            "ActiveSupport",
            "ActionController",
            "ActiveRecord",
            "ActiveRecord::Persistence",
            "ActiveRecord::Suppressor",
        ];
        let rendered = |namespaces: &Namespaces| {
            all(None, namespaces)
                .render(&declaring_kinds(&[], &modules))
                .rbs
        };
        let rbs = rendered(&declaring_kinds(&classes, &modules));
        for line in [
            "def since: () -> (ActiveSupport::TimeWithZone | Time) | (ActiveSupport::TimeWithZone) \
             -> ActiveSupport::TimeWithZone | (DateTime) -> DateTime | (Time) -> Time | (Date) -> \
             (Date | ActiveSupport::TimeWithZone | Time)",
            "def save: (**untyped) ?{ () -> untyped } -> bool?",
            "def save!: (**untyped) ?{ () -> untyped } -> true?",
            "def update: (untyped) -> bool?",
            "def to_i: () -> Integer",
        ] {
            assert!(rbs.contains(line), "{line}\n{rbs}");
        }
        assert!(!rbs.contains("def expect"), "{rbs}");
        let mut gated = classes.to_vec();
        gated.push("ActionController::ExpectedParameterMissing");
        let rbs = rendered(&declaring_kinds(&gated, &modules));
        assert!(
            rbs.contains(
                "def expect: (**untyped) -> (ActionController::Parameters | Array[untyped]) | \
                 (Symbol) -> untyped | (Symbol, Symbol, *untyped) -> untyped"
            ),
            "{rbs}"
        );
    }

    fn railties() -> Namespaces {
        declaring_kinds(
            &[
                "Rails::Railtie",
                "Rails::Engine",
                APPLICATION,
                "ActionDispatch::Routing::RouteSet",
                "ActionDispatch::Routing::Mapper",
                "ActionMailer::Base",
                "ActionController::Base",
                "ActionController::API",
                "Rails::Railtie::Configuration",
                "Rails::Engine::Configuration",
                "Rails::Application::Configuration",
                "Rails::Application::Configuration::Custom",
                "Rails::Paths::Root",
                "Rails::Configuration::MiddlewareStackProxy",
                "Pathname",
                "Array",
                "Hash",
                "ActiveSupport::OrderedOptions",
                "Lograge::OrderedOptions",
            ],
            &[
                "Rails",
                "Rails::Paths",
                "Rails::Configuration",
                "ActionDispatch",
                "ActionDispatch::Routing",
                "ActionMailer",
                "ActionController",
                "ActiveSupport",
                "ActiveRecord",
                "Turbo",
                "Lograge",
            ],
        )
    }

    /// The block rows write the `self` each block runs against, and the controller callbacks are
    /// declared on every class `AbstractController::Callbacks` reaches.
    #[test]
    fn a_block_row_writes_the_self_its_block_runs_against() {
        let rbs = rbs(None, &railties());
        for line in [
            "def configure: [T] () { () [self: self] -> T } -> T",
            "def self.configure: [T] () { () [self: instance] -> T } -> T",
            "def routes: () ?{ () [self: ActionDispatch::Routing::Mapper] -> void } -> \
             ActionDispatch::Routing::RouteSet",
            "def draw: () { () [self: ActionDispatch::Routing::Mapper] -> void } -> nil",
            "def append: () { () [self: ActionDispatch::Routing::Mapper] -> void } -> untyped",
            "def self.default: (**^() [self: instance] -> untyped) -> untyped",
            "def self.before_action: (*untyped, ?if: ^() [self: instance] -> untyped, ?unless: \
             ^() [self: instance] -> untyped, **untyped) ?{ (untyped) [self: instance] -> void } \
             -> void",
        ] {
            assert!(rbs.contains(line), "missing {line}:\n{rbs}");
        }
        assert_eq!(
            rbs.matches("def self.prepend_around_action:").count(),
            3,
            "{rbs}"
        );
    }

    /// `config` is typed on both sides, and a namespace only where its framework or gem is: the
    /// bundle holds `ActiveRecord`, `Turbo` and `Lograge`, and nothing for `ActionCable`.
    #[test]
    fn config_is_typed_and_a_namespace_only_where_its_framework_is() {
        let rbs = rbs(None, &railties());
        for line in [
            "def config: () -> Rails::Application::Configuration",
            "def self.config: () -> Rails::Railtie::Configuration",
            "def hosts: () -> Array[untyped]",
            "def paths: () -> Rails::Paths::Root",
            "def active_record: () -> ActiveSupport::OrderedOptions",
            "def turbo: () -> ActiveSupport::OrderedOptions",
            "def lograge: () -> Lograge::OrderedOptions",
        ] {
            assert!(rbs.contains(line), "missing {line}:\n{rbs}");
        }
        assert!(!rbs.contains("def action_cable:"), "{rbs}");
    }

    /// A setting is a write on Rails' configuration, as Rails spells it: not a namespace, not an
    /// index, not another object's `config`, not a bare `config` outside Rails' own bodies.
    #[test]
    fn a_setting_is_a_write_on_config() {
        let source = "\
module Shop
  class Application < ::Rails::Application
    config.dispatcher = Dispatcher.new
    config.active_record = nil
    config[:key] = 1
    config == other
    config.x.custom = 1

    def wire
      config.wired = 1
    end

    module Inner
      config.inner = 1
    end
  end
end

class Blog < Rails::Engine
  config.blog = 1
end

class Plain < Base
  config.plain = 1
end

Rails.application.configure do
  config.configured = 1
end
Shop::Application.configure do
  config.older = 1
end
Other.configure do
  config.other = 1
end
config.bare = 1
Rails.configuration.relay = 1
Rails.configuration.relay ||= 1
Rails.application.config.initialized = 1
Shop::Application.config.constant = 1
ShopPromotions.config.promotions = 1
Rails.config.wrong = 1
Mailer.configuration.host = 'x'
settings.config.deep = 1
@config.ivar = 1
config = 1
";
        let found = super::read_config_writes(source);
        let named: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            named,
            [
                "dispatcher",
                "blog",
                "configured",
                "older",
                "relay",
                "initialized",
                "constant"
            ]
        );
        let ((start, end), (name, name_end)) = found[0].1;
        assert_eq!(
            &source[start as usize..end as usize],
            "config.dispatcher = Dispatcher.new"
        );
        assert_eq!(&source[name as usize..name_end as usize], "dispatcher");
    }

    /// A row whose `self` names a class the bundle lacks declines on its own, as a return does.
    #[test]
    fn a_block_row_declines_where_the_class_its_block_runs_against_is_missing() {
        let partial = rbs(
            None,
            &declaring_kinds(
                &[
                    "Rails::Railtie",
                    "Rails::Engine",
                    "ActionDispatch::Routing::RouteSet",
                ],
                &["Rails", "ActionDispatch", "ActionDispatch::Routing"],
            ),
        );
        assert!(!partial.contains("def draw:"), "{partial}");
        assert!(!partial.contains("def routes:"), "{partial}");
        assert!(partial.contains("def configure:"), "{partial}");
        // An owner the bundle declares under a namespace it does not is not spelled at all: the
        // joined name would introduce `ActionDispatch::Routing`.
        let unspelled = rbs(
            None,
            &declaring_kinds(
                &[
                    "ActionDispatch::Routing::RouteSet",
                    "ActionDispatch::Routing::Mapper",
                ],
                &["ActionDispatch"],
            ),
        );
        assert!(!unspelled.contains("def draw:"), "{unspelled}");
    }

    /// An owner the graph opens as a module is opened as one, on both sides.
    #[test]
    fn a_block_rows_owner_is_opened_with_the_graphs_keyword() {
        let rbs = rbs(
            None,
            &declaring_kinds(
                &[],
                &[
                    "Rails",
                    "Rails::Railtie",
                    "ActionMailer",
                    "ActionMailer::Base",
                ],
            ),
        );
        assert!(rbs.contains("module Railtie"), "{rbs}");
        assert!(!rbs.contains("class Railtie"), "{rbs}");
        assert!(rbs.contains("def self.configure:"), "{rbs}");
        assert!(rbs.contains("def self.default:"), "{rbs}");
    }

    /// From 7.2 the members an adapter delegates to its transaction manager are declared on the
    /// module that writes the `delegate`, and only where the bundle declares that module.
    #[test]
    fn the_transaction_members_need_the_module_that_delegates_them() {
        let classes = [CONNECTION_POOL, LEASING];
        let above = ["ActiveRecord", "ActiveRecord::ConnectionAdapters"];
        let with = declaring_kinds(&classes, &[&above[..], &[DATABASE_STATEMENTS]].concat());
        let rbs = read_framework(None, &[], None, &with)[DATABASE_STATEMENTS]
            .render(&with)
            .rbs;
        assert!(
            rbs.contains(
                "def open_transactions: (*untyped) ?{ (*untyped) -> untyped } -> \
                          ForwardedToItsTarget[\"transaction_manager\", \"open_transactions\"]"
            ),
            "{rbs}"
        );
        let without = declaring_kinds(&classes, &above);
        assert!(!read_framework(None, &[], None, &without).contains_key(DATABASE_STATEMENTS));
    }
}
