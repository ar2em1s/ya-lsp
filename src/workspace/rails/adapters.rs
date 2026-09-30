//! Which connection adapter a model's `connection` is.
//!
//! Three things decide it, and each is read here, text in and text out:
//!
//! - **Rails' registry** of adapter names (`ActiveRecord::ConnectionAdapters.register`, 7.2 and
//!   later): Rails' own four ([`BUILT_IN`]), and whatever an adapter gem registers beside them
//!   ([`read_registered`]).
//! - **The application's `config/database.yml`**: the adapter each environment's databases name
//!   ([`read_database_config`]).
//! - **A model's own `connects_to` or `establish_connection`**: which of those databases it uses
//!   ([`read_connection`]).
//!
//! What is not read, because only running Ruby knows it: `DATABASE_URL` and
//! `<NAME>_DATABASE_URL`, which override the file's adapter at run time, an
//! `ActiveRecord::Base.establish_connection` a script or an initializer makes, and a gem that
//! swaps `ActiveRecord::Base.connection` for a proxy at load time (ar-octopus, replica_pools).
//! [`Resolver`] says how far the answer can be trusted around each.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ruby_prism::{CallNode, Node};

use super::syntax::{constant_spelling, keyword, symbol_or_string};
use crate::generated::{Declared, FORWARDED, Facts, Namespaces, Owner, Source, candidates};

/// Every adapter's ancestor: the class the answer widens to where the adapters disagree.
pub const ABSTRACT_ADAPTER: &str = "ActiveRecord::ConnectionAdapters::AbstractAdapter";

/// What `mysql2` and `trilogy` share below [`ABSTRACT_ADAPTER`].
const ABSTRACT_MYSQL: &str = "ActiveRecord::ConnectionAdapters::AbstractMysqlAdapter";

/// The class every model's pool is, opened with its adapter as a type parameter (`[A]`).
pub const CONNECTION_POOL: &str = "ActiveRecord::ConnectionAdapters::ConnectionPool";

/// The module `ActiveRecord::Base` extends with `connection`, `connection_pool`,
/// `lease_connection` and `with_connection`.
pub const CONNECTION_HANDLING: &str = "ActiveRecord::ConnectionHandling";

/// A class activerecord 7.2 added beside `lease_connection` and `with_connection`, which 7.1 does
/// not have: the version gate for the two, since a row would otherwise declare them where they
/// are missing.
pub const LEASING: &str = "ActiveRecord::ConnectionAdapters::ConnectionPool::LeaseRegistry";

/// Rails' own registrations, as `connection_adapters.rb` writes them in activerecord 7.2, 8.0 and
/// 8.1: `(name, class, the driver gem's top-level constant)`. The adapter loads only where its
/// driver does, so a bundle without `pg` cannot be connected to PostgreSQL.
const BUILT_IN: [(&str, &str, &str); 4] = [
    (
        "sqlite3",
        "ActiveRecord::ConnectionAdapters::SQLite3Adapter",
        "SQLite3",
    ),
    (
        "mysql2",
        "ActiveRecord::ConnectionAdapters::Mysql2Adapter",
        "Mysql2",
    ),
    (
        "trilogy",
        "ActiveRecord::ConnectionAdapters::TrilogyAdapter",
        "Trilogy",
    ),
    (
        "postgresql",
        "ActiveRecord::ConnectionAdapters::PostgreSQLAdapter",
        "PG",
    ),
];

/// The superclass of each adapter class activerecord ships, as its `class X < Y` lines say.
const BUILT_IN_SUPERS: [(&str, &str); 5] = [
    (
        "ActiveRecord::ConnectionAdapters::SQLite3Adapter",
        ABSTRACT_ADAPTER,
    ),
    (
        "ActiveRecord::ConnectionAdapters::Mysql2Adapter",
        ABSTRACT_MYSQL,
    ),
    (
        "ActiveRecord::ConnectionAdapters::TrilogyAdapter",
        ABSTRACT_MYSQL,
    ),
    (
        "ActiveRecord::ConnectionAdapters::PostgreSQLAdapter",
        ABSTRACT_ADAPTER,
    ),
    (ABSTRACT_MYSQL, ABSTRACT_ADAPTER),
];

/// A URL's scheme to its adapter, `ActiveRecord.protocol_adapters`' defaults. Any other scheme is
/// the adapter's own name with `-` read as `_` (`ConnectionUrlResolver`).
const PROTOCOLS: [(&str, &str); 3] = [
    ("sqlite", "sqlite3"),
    ("mysql", "mysql2"),
    ("postgres", "postgresql"),
];

/// Every constant this module names, for the pass to ask the bundle about: the drivers, the
/// adapter classes, and the namespaces above them.
#[must_use]
pub fn adapter_constants() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = BUILT_IN
        .iter()
        .flat_map(|(_, class, driver)| [*class, *driver])
        .collect();
    names.extend([
        "ActiveRecord",
        "ActiveRecord::ConnectionAdapters",
        ABSTRACT_ADAPTER,
        ABSTRACT_MYSQL,
        CONNECTION_POOL,
        CONNECTION_HANDLING,
        LEASING,
        DATABASE_STATEMENTS,
    ]);
    names
}

/// Whether `path` is an application's `config/database.yml`.
#[must_use]
pub fn is_database_config(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "database.yml")
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|directory| directory == "config")
}

/// What a database's configuration says its adapter is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adapter {
    /// A name written out: `postgresql`, or the adapter a literal URL's scheme names.
    Named(String),
    /// ERB, a URL read from the environment, or nothing at all: whatever the bundle can load.
    Unknown,
}

/// One database of one environment in `config/database.yml`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    environment: String,
    /// `primary` for an environment with one database, which is what Rails calls it.
    name: String,
    adapter: Adapter,
}

/// `config/database.yml`, read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    entries: Vec<Entry>,
}

impl DatabaseConfig {
    /// What each environment's primary database is: the one named `primary`, else its first, as
    /// `ActiveRecord::Base` connects.
    fn primaries(&self) -> Vec<&Adapter> {
        let mut environments: Vec<&str> = self
            .entries
            .iter()
            .map(|entry| entry.environment.as_str())
            .collect();
        environments.dedup();
        environments
            .into_iter()
            .filter_map(|environment| {
                let mut of = self
                    .entries
                    .iter()
                    .filter(|entry| entry.environment == environment);
                let first = of.clone().next()?;
                Some(
                    &of.find(|entry| entry.name == "primary")
                        .unwrap_or(first)
                        .adapter,
                )
            })
            .collect()
    }

    /// Every environment's database of this name.
    fn named(&self, name: &str) -> Vec<&Adapter> {
        self.entries
            .iter()
            .filter(|entry| entry.name == name)
            .map(|entry| &entry.adapter)
            .collect()
    }
}

/// One mapping of the YAML, as far as a database's adapter needs it.
#[derive(Debug, Default)]
struct Mapping {
    key: String,
    value: String,
    anchor: Option<String>,
    /// `<<: *default` and a whole value written `*default`: the anchors merged in, first wins.
    merges: Vec<String>,
    children: Vec<Mapping>,
}

/// One line of the file that holds a key.
struct Line<'a> {
    indent: usize,
    key: &'a str,
    value: &'a str,
}

/// Read `config/database.yml` for the adapter of every database of every environment.
///
/// **A scanner, not a YAML parser**, like the `structure.sql` reader: keys by indentation,
/// anchors (`&default`), merges (`<<: *default`) and whole-value aliases (`*default`). A value
/// holding ERB is [`Adapter::Unknown`]; a line of ERB alone (`<% … %>`), a comment and a list
/// item are skipped. A `url:` wins over an `adapter:`, as Rails merges the URL's hash over the
/// rest.
///
/// An environment whose every value is a mapping holds several databases, each by its name, as
/// Rails reads one; any other holds one, named `primary`.
#[must_use]
pub fn read_database_config(source: &str) -> DatabaseConfig {
    let lines: Vec<Line<'_>> = source.lines().filter_map(line_of).collect();
    let mut at = 0;
    let top = mappings(&lines, &mut at, 0);
    let mut anchors: BTreeMap<&str, &Mapping> = BTreeMap::new();
    collect_anchors(&top, &mut anchors);
    let adapter = |mapping: &Mapping| adapter_of(mapping, &anchors, 0).unwrap_or(Adapter::Unknown);
    let mut entries = Vec::new();
    for environment in &top {
        let several = !environment.children.is_empty()
            && environment.merges.is_empty()
            && environment
                .children
                .iter()
                .all(|child| !child.children.is_empty() || !child.merges.is_empty());
        if several {
            for database in &environment.children {
                entries.push(Entry {
                    environment: environment.key.clone(),
                    name: database.key.clone(),
                    adapter: adapter(database),
                });
            }
        } else {
            entries.push(Entry {
                environment: environment.key.clone(),
                name: "primary".to_owned(),
                adapter: adapter(environment),
            });
        }
    }
    DatabaseConfig { entries }
}

/// One line as a key and its value, or `None` for a line that holds no key.
fn line_of(raw: &str) -> Option<Line<'_>> {
    let text = raw.trim_start();
    if text.is_empty() || text.starts_with('#') || text.starts_with("<%") || text.starts_with('-') {
        return None;
    }
    let indent = raw.len() - text.len();
    let split = text
        .char_indices()
        .find(|(at, character)| {
            *character == ':'
                && text[at + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(at, _)| at)?;
    let key = text[..split]
        .trim()
        .trim_matches(|quote| quote == '"' || quote == '\'');
    let value = text[split + 1..].trim();
    // A comment needs the space before it; `postgres://host/db#x` keeps its `#`.
    let value = value.split(" #").next().unwrap_or(value).trim();
    Some(Line { indent, key, value })
}

/// The mappings at `indent`, each with the deeper lines under it.
fn mappings(lines: &[Line<'_>], at: &mut usize, indent: usize) -> Vec<Mapping> {
    let mut read: Vec<Mapping> = Vec::new();
    while let Some(line) = lines.get(*at) {
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            // A line deeper than a key that opened nothing: skipped rather than guessed at.
            *at += 1;
            continue;
        }
        *at += 1;
        let (anchor, value) = match line.value.strip_prefix('&') {
            Some(rest) => {
                let (name, value) = rest.split_once(' ').unwrap_or((rest, ""));
                (Some(name.to_owned()), value.trim())
            }
            None => (None, line.value),
        };
        let mut mapping = Mapping {
            key: line.key.to_owned(),
            value: value.to_owned(),
            anchor,
            ..Mapping::default()
        };
        if let Some(alias) = value.strip_prefix('*') {
            mapping.merges.push(alias.trim().to_owned());
        }
        if let Some(next) = lines.get(*at)
            && next.indent > indent
        {
            let children = mappings(lines, at, next.indent);
            for child in children {
                if child.key == "<<" {
                    mapping.merges.extend(merged(&child.value));
                } else {
                    mapping.children.push(child);
                }
            }
        }
        read.push(mapping);
    }
    read
}

/// The anchors a `<<:` value merges in: `*default`, or `[*one, *two]`.
fn merged(value: &str) -> Vec<String> {
    value
        .trim_matches(|bracket| bracket == '[' || bracket == ']')
        .split(',')
        .filter_map(|alias| alias.trim().strip_prefix('*'))
        .map(|alias| alias.trim().to_owned())
        .collect()
}

fn collect_anchors<'a>(mappings: &'a [Mapping], into: &mut BTreeMap<&'a str, &'a Mapping>) {
    for mapping in mappings {
        if let Some(anchor) = &mapping.anchor {
            into.insert(anchor, mapping);
        }
        collect_anchors(&mapping.children, into);
    }
}

/// What one database's mapping says its adapter is, following merges; `None` where it says
/// nothing. Merges are followed a few deep: a cycle is YAML no Rails would load.
fn adapter_of(
    mapping: &Mapping,
    anchors: &BTreeMap<&str, &Mapping>,
    depth: usize,
) -> Option<Adapter> {
    let child = |key: &str| mapping.children.iter().find(|child| child.key == key);
    if let Some(url) = child("url") {
        return Some(from_url(&url.value));
    }
    if let Some(adapter) = child("adapter") {
        let written = unquoted(&adapter.value);
        return Some(if written.is_empty() || written.contains("<%") {
            Adapter::Unknown
        } else {
            Adapter::Named(written.to_owned())
        });
    }
    if depth >= 8 {
        return None;
    }
    mapping
        .merges
        .iter()
        .filter_map(|alias| anchors.get(alias.as_str()))
        .find_map(|merged| adapter_of(merged, anchors, depth + 1))
}

/// The adapter a URL's scheme names, or [`Adapter::Unknown`] where the URL is ERB.
fn from_url(value: &str) -> Adapter {
    let written = unquoted(value);
    if written.contains("<%") {
        return Adapter::Unknown;
    }
    let Some((scheme, _)) = written.split_once(':') else {
        return Adapter::Unknown;
    };
    if scheme.is_empty() {
        return Adapter::Unknown;
    }
    let scheme = scheme.replace('-', "_");
    Adapter::Named(
        PROTOCOLS
            .iter()
            .find(|(protocol, _)| *protocol == scheme)
            .map_or(scheme, |(_, name)| (*name).to_owned()),
    )
}

fn unquoted(value: &str) -> &str {
    value
        .trim()
        .trim_matches(|quote| quote == '"' || quote == '\'')
}

/// What one adapter gem's file adds to the registry.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Registered {
    /// `(name, class)` from every `ActiveRecord::ConnectionAdapters.register(name, class, …)`.
    names: Vec<(String, String)>,
    /// `(class, the constant its superclass is written as, resolved candidates first)`.
    supers: Vec<(String, Vec<String>)>,
}

/// Read one of a gem's `*adapter.rb` files for the adapters it registers and the classes it
/// defines, so an application naming `postgis` gets the class `activerecord-postgis-adapter`
/// wrote.
///
/// Statements only, as every reader here: module and class bodies, and the blocks and
/// conditionals a registration is written in (`ActiveSupport.on_load(:active_record) do … end`).
/// A method's body is not read.
#[must_use]
pub fn read_registered(source: &str) -> Registered {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut read = Registered::default();
    let mut nesting = Vec::new();
    registered_in(
        source,
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
        &mut nesting,
        &mut read,
    );
    read
}

fn registered_in(
    source: &str,
    body: Option<Node<'_>>,
    nesting: &mut Vec<String>,
    into: &mut Registered,
) {
    let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
        return;
    };
    for statement in statements.body().iter() {
        if let Some(module) = statement.as_module_node() {
            nesting.push(constant_spelling(source, &module.constant_path()));
            registered_in(source, module.body(), nesting, into);
            nesting.pop();
        } else if let Some(class) = statement.as_class_node() {
            let written = constant_spelling(source, &class.constant_path());
            let qualified = qualified(nesting, &written);
            if let Some(superclass) = class.superclass() {
                let spelled = constant_spelling(source, &superclass);
                let outer = nesting.join("::");
                let resolved = if outer.is_empty() {
                    vec![spelled]
                } else {
                    candidates(&outer, &spelled)
                };
                into.supers.push((qualified.clone(), resolved));
            }
            nesting.push(written);
            registered_in(source, class.body(), nesting, into);
            nesting.pop();
        } else if let Some(call) = statement.as_call_node() {
            if let Some(pair) = registration(source, &call, nesting) {
                into.names.push(pair);
            }
            if let Some(block) = call.block().and_then(|block| block.as_block_node()) {
                registered_in(source, block.body(), nesting, into);
            }
        } else if let Some(conditional) = statement.as_if_node() {
            registered_in(
                source,
                conditional.statements().map(|body| body.as_node()),
                nesting,
                into,
            );
        } else if let Some(conditional) = statement.as_unless_node() {
            registered_in(
                source,
                conditional.statements().map(|body| body.as_node()),
                nesting,
                into,
            );
        }
    }
}

fn qualified(nesting: &[String], written: &str) -> String {
    if nesting.is_empty() {
        written.to_owned()
    } else {
        format!("{}::{written}", nesting.join("::"))
    }
}

/// `(name, class)` for a `register` call on `ActiveRecord::ConnectionAdapters`, written on it or
/// inside it, with two string literals.
fn registration(source: &str, call: &CallNode<'_>, nesting: &[String]) -> Option<(String, String)> {
    if call.name().as_slice() != b"register" {
        return None;
    }
    let on_registry = match call.receiver() {
        Some(receiver) => {
            constant_spelling(source, &receiver) == "ActiveRecord::ConnectionAdapters"
        }
        None => nesting.join("::") == "ActiveRecord::ConnectionAdapters",
    };
    if !on_registry {
        return None;
    }
    let mut arguments = call.arguments()?.arguments().iter();
    let (name, _) = super::syntax::string_literal(source, &arguments.next()?)?;
    let (class, _) = super::syntax::string_literal(source, &arguments.next()?)?;
    Some((name, class.trim_start_matches("::").to_owned()))
}

/// What a model's `connects_to` or `establish_connection` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    /// A database of `config/database.yml`, by its name: `:animals`.
    Database(String),
    /// An adapter written in the call itself: `adapter: "sqlite3"` or a URL.
    Adapter(Adapter),
    /// Anything only running Ruby can read.
    Unknown,
}

/// One model's own connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// What the call names. Empty for `establish_connection` with no argument, which is the
    /// environment's primary database again.
    names: Vec<Named>,
    /// Written under an `if` or `unless`, so the model may keep the application's connection.
    pub(super) conditional: bool,
    /// The call, which is where each member it answers is placed.
    at: (u32, u32),
}

/// Read one `connects_to` or `establish_connection`, or decline a call that is neither.
///
/// - `connects_to database: { writing: :primary, reading: :replica }` names each database.
///   `shards: { one: { writing: :one } }` names each shard's.
/// - `establish_connection :animals` names one; a string with a scheme is a URL, and a hash says
///   `adapter:` or `url:` itself.
/// - Every value that is not a literal is [`Named::Unknown`].
pub(super) fn read_connection(
    source: &str,
    call: &CallNode<'_>,
    called: &str,
) -> Option<Connection> {
    let message = call.message_loc()?;
    // A bare `establish_connection` has no arguments to end the span at: its name alone.
    let at = super::syntax::header(call)
        .unwrap_or((message.start_offset() as u32, message.end_offset() as u32));
    let names = match called {
        "connects_to" => {
            let mut names = Vec::new();
            if let Some(databases) = keyword(call, "database") {
                roles(source, &databases, &mut names);
            }
            if let Some(shards) = keyword(call, "shards") {
                match hash_values(&shards) {
                    Some(each) => {
                        for shard in each {
                            roles(source, &shard, &mut names);
                        }
                    }
                    None => names.push(Named::Unknown),
                }
            }
            if names.is_empty() {
                names.push(Named::Unknown);
            }
            names
        }
        "establish_connection" => match call.arguments() {
            None => Vec::new(),
            Some(arguments) => {
                let first = arguments.arguments().iter().next()?;
                vec![established(source, &first)]
            }
        },
        _ => return None,
    };
    Some(Connection {
        names,
        conditional: false,
        at,
    })
}

/// The databases one `{ writing: :primary, reading: :replica }` names.
fn roles(source: &str, hash: &Node<'_>, into: &mut Vec<Named>) {
    let Some(values) = hash_values(hash) else {
        into.push(Named::Unknown);
        return;
    };
    for value in values {
        into.push(match symbol_or_string(source, &value) {
            Some((name, _)) => Named::Database(name),
            None => Named::Unknown,
        });
    }
}

/// A braced hash literal's values; `None` for anything else. Inside a keyword's value a hash is
/// always braced.
fn hash_values<'pr>(node: &Node<'pr>) -> Option<Vec<Node<'pr>>> {
    node.as_hash_node()?
        .elements()
        .iter()
        .map(|element| Some(element.as_assoc_node()?.value()))
        .collect()
}

/// What `establish_connection`'s argument names.
fn established(source: &str, argument: &Node<'_>) -> Named {
    if let Some((written, _)) = symbol_or_string(source, argument) {
        // Rails reads a string as a URL (`build_db_config_from_string`), where it has a scheme.
        return if argument.as_string_node().is_some() && written.contains(':') {
            Named::Adapter(from_url(&written))
        } else {
            Named::Database(written)
        };
    }
    let pairs = match (argument.as_hash_node(), argument.as_keyword_hash_node()) {
        (Some(hash), _) => hash.elements(),
        (None, Some(hash)) => hash.elements(),
        (None, None) => return Named::Unknown,
    };
    for pair in pairs.iter().filter_map(|element| element.as_assoc_node()) {
        let Some((key, _)) = symbol_or_string(source, &pair.key()) else {
            continue;
        };
        let value = symbol_or_string(source, &pair.value()).map(|(value, _)| value);
        match (key.as_str(), value) {
            ("url", Some(url)) => return Named::Adapter(from_url(&url)),
            ("adapter", Some(name)) => return Named::Adapter(Adapter::Named(name)),
            ("url" | "adapter", None) => return Named::Unknown,
            _ => {}
        }
    }
    Named::Unknown
}

/// The adapters one application can connect with, and which class each is.
///
/// - **An application's `config/database.yml` narrows it** to the adapters its databases name. A
///   `DATABASE_URL` naming another adapter the bundle can load would override that at run time;
///   the file is taken as the application's word.
/// - **An adapter the file cannot say** (ERB, a URL from the environment, no file) is any the
///   bundle can load: Rails' own where the driver gem is bundled, and every gem's registration.
/// - **A name nothing registers answers nothing**: makara's `postgresql_makara` reaches Rails 7.2
///   through a deprecated `postgresql_makara_connection`, whose object is a delegator, not an
///   adapter.
/// - **An engine or a gem** is connected by its host application, so its answer is
///   [`ABSTRACT_ADAPTER`].
/// - **Several classes are their nearest common ancestor**: `mysql2` and `trilogy` are an
///   `AbstractMysqlAdapter`, PostgreSQL and SQLite an `AbstractAdapter`. A class whose chain the
///   files do not show reaching `AbstractAdapter` answers nothing beside another.
#[derive(Debug, Default)]
pub struct Resolver {
    classes: BTreeMap<String, String>,
    supers: BTreeMap<String, Vec<String>>,
    loadable: BTreeSet<String>,
    config: Option<DatabaseConfig>,
    application: bool,
}

impl Resolver {
    /// `driver` says whether the bundle declares a driver gem's constant.
    pub fn new(
        registered: &[&Registered],
        driver: &dyn Fn(&str) -> bool,
        config: Option<DatabaseConfig>,
        application: bool,
    ) -> Self {
        let mut resolver = Self {
            config,
            application,
            ..Self::default()
        };
        for (name, class, gem) in BUILT_IN {
            resolver.classes.insert(name.to_owned(), class.to_owned());
            if driver(gem) {
                resolver.loadable.insert(class.to_owned());
            }
        }
        for (class, superclass) in BUILT_IN_SUPERS {
            resolver
                .supers
                .insert(class.to_owned(), vec![superclass.to_owned()]);
        }
        for read in registered {
            for (name, class) in &read.names {
                resolver.classes.insert(name.clone(), class.clone());
                resolver.loadable.insert(class.clone());
            }
            for (class, superclass) in &read.supers {
                resolver
                    .supers
                    .entry(class.clone())
                    .or_insert_with(|| superclass.clone());
            }
        }
        resolver
    }

    /// What every model's connection is unless it names its own: each environment's primary
    /// database.
    #[must_use]
    pub fn primary(&self) -> Option<String> {
        if !self.application {
            return Some(ABSTRACT_ADAPTER.to_owned());
        }
        match &self.config {
            Some(config) => self.class_of(config.primaries()),
            None => self.class_of(vec![&Adapter::Unknown]),
        }
    }

    /// What one model's own connection is.
    #[must_use]
    pub fn connection(&self, connection: &Connection) -> Option<String> {
        if !self.application {
            return Some(ABSTRACT_ADAPTER.to_owned());
        }
        let config = self.config.as_ref();
        let mut adapters: Vec<Adapter> = Vec::new();
        let primaries = |into: &mut Vec<Adapter>| match config {
            Some(config) => into.extend(config.primaries().into_iter().cloned()),
            None => into.push(Adapter::Unknown),
        };
        if connection.names.is_empty() || connection.conditional {
            primaries(&mut adapters);
        }
        for named in &connection.names {
            match named {
                Named::Database(name) => {
                    let found = config.map(|config| config.named(name)).unwrap_or_default();
                    if found.is_empty() {
                        adapters.push(Adapter::Unknown);
                    } else {
                        adapters.extend(found.into_iter().cloned());
                    }
                }
                Named::Adapter(adapter) => adapters.push(adapter.clone()),
                Named::Unknown => adapters.push(Adapter::Unknown),
            }
        }
        self.class_of(adapters.iter().collect())
    }

    fn class_of(&self, adapters: Vec<&Adapter>) -> Option<String> {
        let mut classes: BTreeSet<&str> = BTreeSet::new();
        for adapter in adapters {
            match adapter {
                Adapter::Named(name) => classes.insert(self.classes.get(name)?),
                Adapter::Unknown => {
                    classes.extend(self.loadable.iter().map(String::as_str));
                    true
                }
            };
        }
        self.common(&classes)
    }

    /// The nearest class every one of `classes` is.
    fn common(&self, classes: &BTreeSet<&str>) -> Option<String> {
        let mut classes = classes.iter();
        let first = classes.next()?;
        let Some(second) = classes.next() else {
            return Some((*first).to_owned());
        };
        let chains: Vec<Vec<String>> = [*first, *second]
            .into_iter()
            .chain(classes.copied())
            .map(|class| self.chain(class))
            .collect::<Option<_>>()?;
        chains[0]
            .iter()
            .find(|class| chains.iter().all(|chain| chain.contains(class)))
            .cloned()
    }

    /// `class` and its superclasses up to [`ABSTRACT_ADAPTER`], or `None` where the files do not
    /// show it reaching there.
    fn chain(&self, class: &str) -> Option<Vec<String>> {
        let mut chain = vec![class.to_owned()];
        let mut current = class;
        while current != ABSTRACT_ADAPTER {
            if chain.len() > 16 {
                return None;
            }
            current = self.supers.get(current)?.iter().find(|candidate| {
                candidate.as_str() == ABSTRACT_ADAPTER || self.supers.contains_key(*candidate)
            })?;
            chain.push(current.to_owned());
        }
        Some(chain)
    }
}

/// The rows one model's own connection writes on its class object, placed at the call that made
/// it: `connection`, `connection_pool` holding that adapter, and on Rails 7.2 and later
/// `lease_connection` and `with_connection`. Only where the bundle declares
/// [`CONNECTION_HANDLING`]: activerecord is indexed.
pub fn connection_rows(
    facts: &mut Facts,
    (file, class): (&str, &str),
    adapter: &str,
    connection: &Connection,
    namespaces: &Namespaces,
) {
    if !namespaces.declares(CONNECTION_HANDLING) {
        return;
    }
    let (leasing, pool) = (
        namespaces.declares(LEASING),
        namespaces.declares(CONNECTION_POOL),
    );
    let because = format!(
        "From `{file}`: `{class}` connects through its own `connects_to` or \
         `establish_connection`, so its connection is a `{adapter}`."
    );
    for (name, parameters, returns) in rows(adapter, leasing, pool) {
        facts.declare(Declared {
            owner: Owner::Singleton(class.to_owned()),
            name: name.to_owned(),
            returns,
            parameters,
            because: because.clone(),
            at: Some((connection.at, connection.at)),
            from: Source::Convention,
            overloads: Vec::new(),
            private: false,
        });
    }
}

/// `(name, parameters, returns)` for the members that hand back a connection of `adapter`:
/// `leasing` is Rails 7.2 and later, and `pool` that the bundle declares the pool's class.
pub(super) fn rows(
    adapter: &str,
    leasing: bool,
    pool: bool,
) -> Vec<(&'static str, String, String)> {
    let mut rows = vec![("connection", "()".to_owned(), adapter.to_owned())];
    if pool {
        rows.push((
            "connection_pool",
            "()".to_owned(),
            format!("{CONNECTION_POOL}[{adapter}]"),
        ));
    }
    if leasing {
        rows.push(("lease_connection", "()".to_owned(), adapter.to_owned()));
        rows.push((
            "with_connection",
            format!("[T] (?prevent_permanent_checkout: bool) {{ ({adapter}) -> T }}"),
            "T".to_owned(),
        ));
    }
    rows
}

/// `(name, parameters, returns)` for a pool's own members, in its adapter's type parameter `A`:
/// `ActiveRecord::Base.connection_pool.with_connection { |conn| }` hands `conn` the adapter the
/// pool was made for. The pool's own `connection` is only declared before 7.2: 7.2 deprecates it
/// for `lease_connection`, and 8.0 removes it, so a row would declare a member the pool lacks.
pub(super) fn pool_rows(leasing: bool) -> Vec<(&'static str, String, String)> {
    if leasing {
        vec![
            ("lease_connection", "()".to_owned(), "A".to_owned()),
            (
                "with_connection",
                "[T] (?prevent_permanent_checkout: bool) { (A) -> T }".to_owned(),
                "T".to_owned(),
            ),
        ]
    } else {
        vec![
            ("connection", "()".to_owned(), "A".to_owned()),
            (
                "with_connection",
                "[T] () { (A) -> T }".to_owned(),
                "T".to_owned(),
            ),
        ]
    }
}

/// The module every adapter includes, whose transaction members Rails makes with `delegate`.
pub const DATABASE_STATEMENTS: &str = "ActiveRecord::ConnectionAdapters::DatabaseStatements";

/// What [`DATABASE_STATEMENTS`] hands on to the adapter's transaction manager, with
/// `delegate …, to: :transaction_manager`: the same ten in activerecord 7.2, 8.0 and 8.1. rubydex
/// reads no `delegate` in a gem, so `connection.open_transactions` found no member on an adapter.
const TRANSACTIONS: [&str; 10] = [
    "within_new_transaction",
    "open_transactions",
    "current_transaction",
    "begin_transaction",
    "commit_transaction",
    "rollback_transaction",
    "materialize_transactions",
    "disable_lazy_transactions!",
    "enable_lazy_transactions!",
    "dirty_current_transaction",
];

/// `(name, parameters, returns)` for [`TRANSACTIONS`]: each is the two calls `delegate` makes
/// ([`FORWARDED`]), which the types table makes where the member is called, and forwards every
/// argument and the block.
pub(super) fn transaction_rows() -> Vec<(&'static str, String, String)> {
    TRANSACTIONS
        .iter()
        .map(|name| {
            (
                *name,
                "(*untyped) ?{ (*untyped) -> untyped }".to_owned(),
                format!("{FORWARDED}[\"transaction_manager\", \"{name}\"]"),
            )
        })
        .collect()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn class(config: &str, drivers: &[&str], registered: &[Registered]) -> Option<String> {
        Resolver::new(
            &registered.iter().collect::<Vec<_>>(),
            &|driver| drivers.contains(&driver),
            Some(read_database_config(config)),
            true,
        )
        .primary()
    }

    const PG: &str = "ActiveRecord::ConnectionAdapters::PostgreSQLAdapter";

    /// The shapes `config/database.yml` is written in across the corpora: an anchor merged into
    /// each environment, a literal URL, ERB, and several databases per environment.
    #[test]
    fn what_each_environment_s_databases_are_read_as() {
        let read = read_database_config(
            "\
default: &default
  adapter: postgresql
  pool: <%= ENV['POOL'] %> # a comment

development:
  <<: *default
  database: app_development

test:
  adapter: mysql2
  url: postgres:///app_test

production:
  primary:
    <<: *default
  animals:
    adapter: sqlite3
    database: animals
  cache:
    url: <%= ENV['CACHE_URL'] %>
<% if true %>
staging: *default
",
        );
        let pairs: Vec<(&str, &str, &Adapter)> = read
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.environment.as_str(),
                    entry.name.as_str(),
                    &entry.adapter,
                )
            })
            .collect();
        let postgresql = Adapter::Named("postgresql".to_owned());
        assert_eq!(
            pairs,
            [
                ("default", "primary", &postgresql),
                ("development", "primary", &postgresql),
                ("test", "primary", &postgresql),
                ("production", "primary", &postgresql),
                (
                    "production",
                    "animals",
                    &Adapter::Named("sqlite3".to_owned())
                ),
                ("production", "cache", &Adapter::Unknown),
                ("staging", "primary", &postgresql),
            ]
        );
        assert_eq!(read.named("animals").len(), 1);
        assert_eq!(read.primaries().len(), 5);
    }

    #[test]
    fn a_url_s_scheme_names_its_adapter() {
        for (url, adapter) in [
            ("postgres://h/db", "postgresql"),
            ("mysql://h/db", "mysql2"),
            ("sqlite:db/x.sqlite3", "sqlite3"),
            ("trilogy://h/db", "trilogy"),
            ("\"postgis://h/db\"", "postgis"),
            ("my-db://x", "my_db"),
        ] {
            assert_eq!(from_url(url), Adapter::Named(adapter.to_owned()), "{url}");
        }
        for url in ["<%= ENV['X'] %>", "no-scheme", ":x"] {
            assert_eq!(from_url(url), Adapter::Unknown, "{url}");
        }
    }

    /// What the application's connection is, by what the file and the bundle say.
    #[test]
    fn the_primary_database_s_adapter_and_the_bundle_decide_the_class() {
        let pg = "development:\n  adapter: postgresql\n";
        assert_eq!(class(pg, &["PG"], &[]).as_deref(), Some(PG));
        // The file's word, even where the bundle could load another.
        assert_eq!(class(pg, &["PG", "SQLite3"], &[]).as_deref(), Some(PG));
        // A development SQLite beside a production PostgreSQL: both, so their common ancestor.
        let two = "development:\n  adapter: sqlite3\nproduction:\n  adapter: postgresql\n";
        assert_eq!(
            class(two, &["PG", "SQLite3"], &[]).as_deref(),
            Some(ABSTRACT_ADAPTER)
        );
        let mysql = "development:\n  adapter: mysql2\nproduction:\n  adapter: trilogy\n";
        assert_eq!(class(mysql, &[], &[]).as_deref(), Some(ABSTRACT_MYSQL));
        // ERB is anything the bundle loads: only `pg` here, so PostgreSQL.
        let erb = "development:\n  adapter: <%= ENV['ADAPTER'] %>\n";
        assert_eq!(class(erb, &["PG"], &[]).as_deref(), Some(PG));
        assert_eq!(
            class(erb, &["PG", "Mysql2"], &[]).as_deref(),
            Some(ABSTRACT_ADAPTER)
        );
        assert_eq!(class(erb, &[], &[]), None, "nothing the bundle can load");
        // A name nothing registers, as makara's is: nothing, not an `AbstractAdapter`.
        let makara = "development:\n  adapter: postgresql_makara\n";
        assert_eq!(class(makara, &["PG"], &[]), None);
    }

    /// A gem's registration names its class, and its `class X < Y` puts it in the family.
    #[test]
    fn an_adapter_gem_registers_its_class() {
        let postgis = read_registered(
            "\
module ActiveRecord
  module ConnectionAdapters
    class PostGISAdapter < PostgreSQLAdapter
    end
  end
end

ActiveSupport.on_load(:active_record) do
  ActiveRecord::ConnectionAdapters.register(\"postgis\", \"ActiveRecord::ConnectionAdapters::PostGISAdapter\", \"x\")
end
",
        );
        let own = read_registered(
            "\
module ActiveRecord
  module ConnectionAdapters
    register \"inhouse\", \"::InHouse::Adapter\"
    if defined?(Other)
      register \"other\", \"Other::Adapter\"
    end
  end
end
class InHouse::Adapter
end
",
        );
        assert_eq!(
            postgis.names,
            [(
                "postgis".to_owned(),
                "ActiveRecord::ConnectionAdapters::PostGISAdapter".to_owned()
            )]
        );
        assert_eq!(own.names.len(), 2);
        let config = "development:\n  adapter: postgis\nproduction:\n  adapter: postgresql\n";
        assert_eq!(
            class(config, &["PG"], std::slice::from_ref(&postgis)).as_deref(),
            Some(PG),
            "PostGIS is a PostgreSQL adapter"
        );
        let inhouse = "development:\n  adapter: inhouse\n";
        assert_eq!(
            class(inhouse, &[], std::slice::from_ref(&own)).as_deref(),
            Some("InHouse::Adapter")
        );
        // Beside another, a class whose ancestry the files do not show answers nothing.
        let mixed = "development:\n  adapter: inhouse\nproduction:\n  adapter: postgresql\n";
        assert_eq!(class(mixed, &["PG"], &[own]), None);
        assert_eq!(read_registered("register \"x\", \"Y\"\n").names.len(), 0);
    }

    /// An engine or a gem is connected by whatever application mounts it.
    #[test]
    fn an_engine_s_connection_is_any_adapter() {
        let resolver = Resolver::new(&[], &|driver| driver == "SQLite3", None, false);
        assert_eq!(resolver.primary().as_deref(), Some(ABSTRACT_ADAPTER));
        let resolver = Resolver::new(&[], &|driver| driver == "SQLite3", None, true);
        assert_eq!(
            resolver.primary().as_deref(),
            Some("ActiveRecord::ConnectionAdapters::SQLite3Adapter"),
            "an application with no file is what its bundle loads"
        );
    }

    fn connection(source: &str) -> Connection {
        let parsed = ruby_prism::parse(source.as_bytes());
        let statement = parsed
            .node()
            .as_program_node()
            .and_then(|program| program.statements().body().iter().next())
            .expect("one statement");
        let call = statement.as_call_node().expect("a call");
        let called = String::from_utf8_lossy(call.name().as_slice()).into_owned();
        read_connection(source, &call, &called).expect("a connection macro")
    }

    /// A model's own connection is the databases its call names, looked up in the file.
    #[test]
    fn a_model_s_connection_is_the_databases_its_call_names() {
        let resolver = Resolver::new(
            &[],
            &|driver| ["PG", "SQLite3"].contains(&driver),
            Some(read_database_config(
                "development:\n  primary:\n    adapter: postgresql\n  animals:\n    adapter: sqlite3\n  \
                 replica:\n    adapter: postgresql\n",
            )),
            true,
        );
        let sqlite = "ActiveRecord::ConnectionAdapters::SQLite3Adapter";
        for (source, expected) in [
            ("connects_to database: { writing: :animals }", Some(sqlite)),
            (
                "connects_to database: { writing: :primary, reading: :replica }",
                Some(PG),
            ),
            (
                "connects_to shards: { one: { writing: :animals } }",
                Some(sqlite),
            ),
            ("establish_connection :animals", Some(sqlite)),
            ("establish_connection \"animals\"", Some(sqlite)),
            ("establish_connection", Some(PG)),
            ("establish_connection adapter: \"sqlite3\"", Some(sqlite)),
            ("establish_connection \"sqlite3:db/x\"", Some(sqlite)),
            (
                "establish_connection({ url: \"postgres://h/x\" })",
                Some(PG),
            ),
            // Unknown: whatever the bundle loads, PostgreSQL and SQLite.
            ("establish_connection config", Some(ABSTRACT_ADAPTER)),
            ("establish_connection adapter: name", Some(ABSTRACT_ADAPTER)),
            ("connects_to database: databases", Some(ABSTRACT_ADAPTER)),
            (
                "connects_to database: { writing: name }",
                Some(ABSTRACT_ADAPTER),
            ),
            ("connects_to shards: shards", Some(ABSTRACT_ADAPTER)),
            ("connects_to role: :x", Some(ABSTRACT_ADAPTER)),
            (
                "connects_to database: { writing: :missing }",
                Some(ABSTRACT_ADAPTER),
            ),
            ("establish_connection pool: 5", Some(ABSTRACT_ADAPTER)),
        ] {
            assert_eq!(
                resolver.connection(&connection(source)).as_deref(),
                expected,
                "{source}"
            );
        }
        // Under an `if`, the application's own connection may stand.
        let mut conditional = connection("connects_to database: { writing: :animals }");
        conditional.conditional = true;
        assert_eq!(
            resolver.connection(&conditional).as_deref(),
            Some(ABSTRACT_ADAPTER)
        );
    }

    /// The shapes a scanner meets and skips: a line with no key, one indented under nothing, and
    /// an anchor merged into itself, which no Rails would load.
    #[test]
    fn what_the_scanner_skips() {
        let read = read_database_config(
            "    stray: indented\nloop: &loop\n  <<: *loop\njust text\n# a comment\n\
             sqlite: &sqlite\n  adapter: sqlite3\n\
             development:\n  adapter: \"postgresql\"\n  variables:\n    - one\n\
             'test':\n  <<: [*loop, *sqlite]\nblank:\n  adapter:\n\"quoted\":\n  adapter: mysql2\n",
        );
        let pairs: Vec<(&str, &Adapter)> = read
            .entries
            .iter()
            .map(|entry| (entry.environment.as_str(), &entry.adapter))
            .collect();
        let sqlite = Adapter::Named("sqlite3".to_owned());
        assert_eq!(
            pairs,
            [
                ("loop", &Adapter::Unknown),
                ("sqlite", &sqlite),
                ("development", &Adapter::Named("postgresql".to_owned())),
                // The first anchor says nothing, so the second answers.
                ("test", &sqlite),
                ("blank", &Adapter::Unknown),
                ("quoted", &Adapter::Named("mysql2".to_owned())),
            ]
        );
    }

    /// Registrations in the other shapes a gem writes, and a superclass chain that loops, which no
    /// Ruby would load and which answers nothing.
    #[test]
    fn registrations_under_unless_and_a_looping_chain() {
        let read = read_registered(
            "\
VERSION = 1
class Loop::One < Loop::Two
end
class Loop::Two < Loop::One
end
unless defined?(X)
  ActiveRecord::ConnectionAdapters.register(\"one\", \"Loop::One\")
end
ActiveRecord::ConnectionAdapters.register(\"two\", \"Loop::Two\")
ActiveRecord::ConnectionAdapters.register(:bad, \"X\")
",
        );
        assert_eq!(read.names.len(), 2);
        let both = "development:\n  adapter: one\nproduction:\n  adapter: two\n";
        assert_eq!(class(both, &[], &[read]), None);
        let lost = read_registered(
            "\
class Lost::Adapter < Somewhere::Base
end
ActiveRecord::ConnectionAdapters.register(\"lost\", \"Lost::Adapter\")
",
        );
        let beside = "development:\n  adapter: lost\nproduction:\n  adapter: postgresql\n";
        assert_eq!(
            class(beside, &["PG"], &[lost]),
            None,
            "a superclass no file declares reaches no common class"
        );
    }

    /// Where no file says, and where the project is an engine: the calls a model makes answer the
    /// same way the application's does.
    #[test]
    fn a_model_s_connection_without_a_file_or_an_application() {
        let unfiled = Resolver::new(&[], &|driver| driver == "PG", None, true);
        assert_eq!(
            unfiled
                .connection(&connection("establish_connection"))
                .as_deref(),
            Some(PG)
        );
        let engine = Resolver::new(&[], &|driver| driver == "PG", None, false);
        assert_eq!(
            engine
                .connection(&connection("connects_to database: { writing: :x }"))
                .as_deref(),
            Some(ABSTRACT_ADAPTER)
        );
        assert_eq!(
            unfiled
                .connection(&connection("establish_connection({ 1 => 2 })"))
                .as_deref(),
            Some(PG),
            "a key that is not a name says nothing"
        );
    }

    /// The rows each side writes: only where activerecord is indexed, and the pool's own arms by
    /// version.
    #[test]
    fn the_rows_a_connection_writes() {
        let mut facts = Facts::default();
        connection_rows(
            &mut facts,
            ("app/models/x.rb", "X"),
            PG,
            &connection("establish_connection :x"),
            &crate::generated::declaring(&[]),
        );
        assert!(facts.is_empty(), "no activerecord, no rows");
        let bare: Vec<&str> = rows(PG, false, false)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(bare, ["connection"], "no pool class, no `connection_pool`");
        let names = |leasing| -> Vec<&str> {
            pool_rows(leasing)
                .into_iter()
                .map(|(name, _, _)| name)
                .collect()
        };
        assert_eq!(names(false), ["connection", "with_connection"]);
        assert_eq!(
            names(true),
            ["lease_connection", "with_connection"],
            "8.0 removes the pool's own `connection`, which 7.2 deprecates"
        );
        assert!(transaction_rows().iter().any(|(name, _, returns)| {
            *name == "open_transactions"
                && returns
                    == &format!("{FORWARDED}[\"transaction_manager\", \"open_transactions\"]")
        }));
    }

    #[test]
    fn only_config_database_yml_is_the_file() {
        assert!(is_database_config(Path::new("/app/config/database.yml")));
        assert!(!is_database_config(Path::new("/app/database.yml")));
        assert!(!is_database_config(Path::new("/app/config/cable.yml")));
    }
}
