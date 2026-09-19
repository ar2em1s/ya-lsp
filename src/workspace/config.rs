//! `ya-lsp.toml`: project configuration.
//!
//! Precedence: `ya-lsp.toml` > the client's `initializationOptions` > defaults.
//! - The file wins, so a project can commit one setup that works for the whole team in any editor.
//! - An absent file must always mean a working server.
//!
//! Deliberately *not* `rubydex.toml`: that is rubydex's own linter config, and ya-lsp must not
//! silently reinterpret it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::messages;

pub const CONFIG_FILE_NAME: &str = "ya-lsp.toml";

/// How loudly a diagnostic rule reports, or `Off` to silence it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Off,
    Error,
    Warning,
    Information,
    Hint,
}

// ---------------------------------------------------------------------------
// Resolved configuration — what the rest of the server reads.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub index: IndexConfig,
    pub log: LogConfig,
    pub rails: RailsConfig,
    pub trees: TreesConfig,
    pub gems: GemsConfig,
    pub rbs: RbsConfig,
    pub types: TypesConfig,
    pub hints: HintsConfig,
    pub diagnostics: DiagnosticsConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexConfig {
    /// Globs, relative to the workspace root, of files to index.
    ///
    /// The default covers every shape Ruby is written in, not only `.rb`. An application's
    /// `Rakefile`, `Gemfile`, rackup files (any `.ru`), `lib/tasks/*.rake` and `.gemspec` define
    /// constants and methods like any other Ruby. `sig/**/*.rbs` is the project saying what those
    /// methods return.
    pub include: Vec<String>,
    /// Globs, relative to the workspace root, to skip.
    pub exclude: Vec<String>,
    /// Extra roots to index and to resolve `require` against: the project's own `$LOAD_PATH`.
    ///
    /// Relative to the workspace root, or absolute. A path outside the root is how a monorepo names
    /// a shared tree. It is walked for `.rb` and `.rbs`, counts as the user's own code, and the
    /// server asks the editor to claim it, since no client's selector reaches outside its folder.
    /// `..` and a symlink out of the tree both work; see `workspace::resolve_load_path`.
    pub load_paths: Vec<PathBuf>,
    /// Hard ceiling on indexed files. Refuse rather than thrash.
    pub max_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GemsConfig {
    pub enabled: bool,
    /// Index the gems that ship inside Ruby itself (`json`, `uri`, `forwardable`, `optparse` and
    /// dozens more) by adding Ruby's own library directory as a load path.
    ///
    /// Separate from `enabled`: it is a separate directory with a separate cost. With
    /// `[rbs] stdlib` on, much of it is described twice (signatures carry the types, the library
    /// the implementation), and a project may want one without the other.
    pub default_gems: bool,
    /// Override when auto-detection picks the wrong Ruby.
    pub ruby_version: Option<String>,
    /// Gem roots to search *before* the built-in table, relative to the workspace root or absolute.
    /// Each should contain `gems/`: the output of `gem env gemdir`.
    ///
    /// The escape hatch for a layout the table does not know: a container image, a Nix store path,
    /// a hand-rolled install. The table is guesswork about other people's filesystems, and without
    /// this a machine it guesses wrong has no recourse.
    pub paths: Vec<PathBuf>,
    /// Hard ceiling on indexed gem files. Separate from `index.max_files`, so a large bundle cannot
    /// quietly eat the workspace's budget.
    pub max_files: usize,
}

/// Ruby's own signatures: `String`, `Array`, `Hash`, `Kernel`, and the stdlib.
///
/// RBS files, not Ruby, from the `rbs` gem, or from the copy vendored into this binary when no Ruby
/// is installed. See `workspace::rbs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RbsConfig {
    /// Index Ruby's core signatures. Off means the graph holds only the five built-in classes
    /// rubydex fabricates.
    pub enabled: bool,
    /// Also index the stdlib signatures: `Set`, `CSV`, `URI`, `Pathname`, `Logger` and the rest.
    /// Separate from core because it adds far more to the resolve every request pays.
    pub stdlib: bool,
    /// An explicit rbs root: a directory holding `core/`, usually an unpacked `rbs-x.y.z` gem.
    /// Skips discovery and the vendored copy.
    pub path: Option<PathBuf>,
}

/// Whether a body of knowledge applies to this project: yes, no, or work it out.
///
/// `true`/`false` and `"on"`/`"off"` are the same two answers. People reach for both, and a config
/// that rejects one wastes an afternoon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Switch {
    /// `true` or `false`.
    Fixed(bool),
    /// `"auto"`, `"on"` or `"off"`.
    Word(Word),
}

/// [`Switch`]'s spelled-out form. Separate so `"auto"` is a value serde can name in its error, not
/// a string it silently fails to read as a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Word {
    Auto,
    On,
    Off,
}

impl Switch {
    /// What this switch means, given what detection found.
    ///
    /// `detected` is consulted only for `auto`, which keeps detection off the path of a project
    /// that has already answered.
    #[must_use]
    pub fn decide(self, detected: bool) -> bool {
        match self {
            Switch::Fixed(decided) => decided,
            Switch::Word(Word::On) => true,
            Switch::Word(Word::Off) => false,
            Switch::Word(Word::Auto) => detected,
        }
    }
}

/// What ya-lsp knows about Rails, and which parts of it this project wants.
///
/// **These exist for correct answers, not speed.** "Turn Rails off to make it faster" is the wrong
/// reason: generator lists are reference-filtered before anything is read, so a project with no
/// `belongs_to`, `schema.rb` or `routes.rb` opens no files.
///
/// What a non-Rails project does pay is narrower:
/// - the projection walk;
/// - the **path conventions**, asked of filenames, not calls. Any project with `app/views/` gets
///   `rails::controller_of` applied, so a Sinatra or Hanami app gets a *Derived* card citing a
///   controller that does not exist.
///
/// So the switch is for correctness and trust. A non-Rails project should not be told about Rails,
/// and a Rails project that skips one family should be able to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailsConfig {
    /// All of the below at once, and `auto` by default, not `true`.
    ///
    /// A plain boolean makes somebody hunt for a setting to turn off a thing they never asked for.
    /// `auto` says yes when the root holds `config/application.rb` **or** `Gemfile.lock` locks
    /// `railties`. Both halves are needed: an engine has no `config/application.rb`, and a fresh
    /// clone has no `Gemfile.lock`.
    pub enabled: Switch,
    /// `db/*schema.rb`, `db/*structure.sql`, and the macros that rename a table.
    pub schema: bool,
    /// Associations, `enum`, `attribute`, `delegate`, the 17 tail macros, and the query interface a
    /// model's relations are written against.
    pub models: bool,
    /// `config/routes.rb` and the helper module every controller and view includes.
    pub routes: bool,
    /// Mailers, jobs and Sidekiq workers.
    pub entrypoints: bool,
    /// The view context (what a bare word in a template may call), and the rung that types a
    /// template's `@story` from the controller its path names.
    pub views: bool,
}

/// Where this project keeps the trees the fence is about.
///
/// `analysis::environment` decides that from four hard-coded words and one hard-coded pair, tuned
/// on a few repositories that happen to agree. Projects that differ had no way to say so, and each
/// either lost answers or kept wrong ones:
/// - a suite in `qa/`;
/// - a `db/` that really is autoloaded;
/// - a monorepo whose `test/` is a published library.
///
/// **One list replaces and one extends. That asymmetry is deliberate.**
/// - A name on the *target* list deletes answers when wrong. An appending key would invite adding
///   `lib` and silently deleting the whole workspace from `completion`, `definition` and `hover`.
/// - A name on the *cursor* list only turns the fence **off**, so a wrong one costs only the
///   protection. It takes the additive shape `gems.paths` has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreesConfig {
    /// The test trees, **replacing** `spec`, `test`, `tests` and `features`. `None` is the built-in
    /// list; `Some([])` turns the target fence off, a legitimate answer.
    pub test: Option<Vec<String>>,
    /// Extra directory names that turn the fence **off** when the cursor is inside one, on top of
    /// the built-in `testing_support`. Additive, never replacing.
    pub test_support: Vec<String>,
    /// The migration trees, as `parent/mark` pairs, **replacing** `db/migrat`. `None` is the
    /// built-in pair; `Some([])` turns the migration fence off.
    ///
    /// A pair, not a name: `migrate` is ordinary enough for `app/services/migrate/`, and only the
    /// tree under `db/` is unloadable. The mark is a **substring** of the directory name, so
    /// `db/migrate`, `db/post_migrate` and `db/old_migrations` are one rule.
    pub migration: Option<Vec<String>>,
}

/// The types ya-lsp derives, and the one rung of them a user may want silenced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypesConfig {
    /// `Struct.new` and `Data.define`. Plain Ruby, so in `[types]`, not `[rails]`: a Rails word
    /// over it would be the first to leak.
    pub structs: bool,
    /// A Sorbet `sig` and a YARD `@return`. In `[types]` for `structs`' reason.
    pub annotations: bool,
    /// Answer from a receiver's own name when nothing else can: `@user` is a `User`, `person` is a
    /// `Person`.
    ///
    /// The only answer ya-lsp gives that may be wrong. It is labelled as a guess wherever it
    /// appears (a hover footnote, a completion card), and it never displaces an answer the code
    /// states or a signature derives. Off leaves only checkable answers, a defensible thing to
    /// want.
    pub guess_from_names: bool,
}

/// Which inlay hints are drawn. One flag per family, because taste is per family.
///
/// No `enabled` beside them, unlike `[diagnostics]`. A diagnostic is pushed and the editor has no
/// switch for it. An inlay hint is pulled, and every client already has a master switch; a
/// duplicate would be a second place to look when the margin is empty.
///
/// **No flag turns the bottom tier on.** A type matched on a name alone is never drawn, in any
/// family. That is a property of the module, not a default; see [`hints`](crate::analysis::hints).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintsConfig {
    /// `stories.each do |story|`: what the called method's signature says its block receives.
    pub block_parameters: bool,
    /// `author = story.author`: what the call on the right hands back. Only where the assignment
    /// does not already name the class.
    pub locals: bool,
    /// `def title`: what a signature declares the method returns, where the source does not.
    pub returns: bool,
}

/// Where the log goes, and how much of it.
///
/// The log **is** an interface: when a user asks why they have no completions, it is the only thing
/// that answers. Two sinks, not one tee'd into two, so the file can sit at `debug` while stderr
/// stays at `info`. One filter over both would let the quiet sink decide what the loud one carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// What stderr carries. A bare level (`info`, `debug`, `off`) is scoped to ya-lsp itself;
    /// anything with an `=` or a `,` goes to `EnvFilter` as written.
    ///
    /// `YA_LSP_LOG` outranks it: the one deliberate inversion of this file's precedence. The
    /// environment variable is what someone debugging from a terminal types, and a project file
    /// quietly overriding it would defeat the point.
    pub level: String,
    /// Write a second copy to a file, so a bug report can attach it instead of a screenshot of an
    /// output channel.
    ///
    /// Off by default: a server installed for hover must not start writing into the repository from
    /// the first keystroke.
    pub file: bool,
    /// Where that copy goes, relative to the workspace root or absolute.
    ///
    /// `tmp/`, not the root: Rails projects gitignore `tmp`, and `index.exclude` prunes it. A
    /// root-level `.ya-lsp.log` would dirty the working tree the moment the file was turned on.
    pub file_path: PathBuf,
    /// What the file carries, read like `level`. `debug`, not `info`: the reason to turn the file
    /// on is the per-request detail stderr lacks.
    pub file_level: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsConfig {
    pub enabled: bool,
    /// Per-rule severity, keyed by rubydex's rule name (`parse-error`,
    /// `dynamic-constant-reference`, ...), the same string the editor shows in a diagnostic's
    /// `code`.
    ///
    /// Absent rules use the default in `analysis::diagnostics`, where most are `Off` because they
    /// fire on correct Ruby. Names are validated there too: this map is open-ended, so a typo must
    /// be caught against the real rule list.
    pub rules: BTreeMap<String, Severity>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            // Extensions first, then fixed names, each spelled `**/` so it matches at every depth:
            // an engine keeps its own `Rakefile`, a monorepo a `.gemspec` per gem. `Gemfile.lock`
            // is not here: it is data Bundler writes, which `bundler.rs` reads as text.
            //
            // `**/*.ru`, not `**/config.ru`: a rackup file is Ruby whatever its name, and an
            // application may mount several (`admin.ru`, `sidekiq.ru`). The extension is also all a
            // client routing by extension can express, so a narrower glob would disagree with what
            // Claude Code sends (`tests/claude_plugin.rs`).
            //
            // `.jbuilder`, `.builder` and `.ruby` are Rails' other template handlers:
            // - A template is `name.format.handler`, and every glob here reads the *handler*.
            // - All three are plain Ruby with no markup, so none needs ERB's blanking.
            // - Like `.erb`, Rails compiles them into a method body; `erb::is_compiled_template` is
            //   the one place that matters.
            // - `**/`, not anchored at `app/views`, because an engine in a monorepo keeps its own
            //   views (e.g. under `promotions/lib/views/`).
            include: vec![
                "**/*.rb".to_owned(),
                "**/*.erb".to_owned(),
                "**/*.jbuilder".to_owned(),
                "**/*.builder".to_owned(),
                "**/*.ruby".to_owned(),
                "**/*.rbs".to_owned(),
                "**/*.rake".to_owned(),
                "**/*.gemspec".to_owned(),
                "**/*.ru".to_owned(),
                "**/Rakefile".to_owned(),
                "**/Gemfile".to_owned(),
            ],
            exclude: vec![
                "vendor/**/*".to_owned(),
                ".bundle/**/*".to_owned(),
                "tmp/**/*".to_owned(),
                "node_modules/**/*".to_owned(),
            ],
            load_paths: vec![PathBuf::from("lib"), PathBuf::from("app")],
            max_files: 50_000,
        }
    }
}

impl Default for GemsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_gems: true,
            ruby_version: None,
            paths: Vec::new(),
            max_files: 300_000,
        }
    }
}

impl Default for RbsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stdlib: true,
            path: None,
        }
    }
}

impl Default for RailsConfig {
    fn default() -> Self {
        Self {
            enabled: Switch::Word(Word::Auto),
            schema: true,
            models: true,
            routes: true,
            entrypoints: true,
            views: true,
        }
    }
}

impl Default for TypesConfig {
    fn default() -> Self {
        Self {
            structs: true,
            annotations: true,
            guess_from_names: true,
        }
    }
}

impl Default for HintsConfig {
    fn default() -> Self {
        Self {
            block_parameters: true,
            locals: true,
            returns: true,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: crate::logging::DEFAULT_LOG_FILTER.to_owned(),
            file: false,
            file_path: PathBuf::from("tmp").join("ya-lsp.log"),
            file_level: "debug".to_owned(),
        }
    }
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rules: BTreeMap::new(),
        }
    }
}

impl DiagnosticsConfig {
    /// Severity for a rule, or `default` when the user has not spoken.
    #[must_use]
    pub fn severity(&self, rule: &str, default: Severity) -> Severity {
        if !self.enabled {
            return Severity::Off;
        }
        self.rules.get(rule).copied().unwrap_or(default)
    }
}

// ---------------------------------------------------------------------------
// Wire format. Every field is optional, so "absent" differs from "explicitly the default". Layering
// needs that.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialConfig {
    pub index: Option<PartialIndex>,
    pub log: Option<PartialLog>,
    pub rails: Option<PartialRails>,
    pub trees: Option<PartialTrees>,
    pub gems: Option<PartialGems>,
    pub rbs: Option<PartialRbs>,
    pub types: Option<PartialTypes>,
    pub hints: Option<PartialHints>,
    pub diagnostics: Option<PartialDiagnostics>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialIndex {
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub load_paths: Option<Vec<PathBuf>>,
    pub max_files: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialGems {
    pub enabled: Option<bool>,
    pub default_gems: Option<bool>,
    pub ruby_version: Option<String>,
    pub paths: Option<Vec<PathBuf>>,
    pub max_files: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialRbs {
    pub enabled: Option<bool>,
    pub stdlib: Option<bool>,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialRails {
    pub enabled: Option<Switch>,
    pub schema: Option<bool>,
    pub models: Option<bool>,
    pub routes: Option<bool>,
    pub entrypoints: Option<bool>,
    pub views: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialTrees {
    pub test: Option<Vec<String>>,
    pub test_support: Option<Vec<String>>,
    pub migration: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialTypes {
    pub structs: Option<bool>,
    pub annotations: Option<bool>,
    pub guess_from_names: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialHints {
    pub block_parameters: Option<bool>,
    pub locals: Option<bool>,
    pub returns: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialLog {
    pub level: Option<String>,
    pub file: Option<bool>,
    pub file_path: Option<PathBuf>,
    pub file_level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialDiagnostics {
    pub enabled: Option<bool>,
    pub rules: Option<BTreeMap<String, Severity>>,
}

impl Config {
    /// Apply one layer on top of this config. Later layers win, field by field.
    pub fn apply(&mut self, layer: PartialConfig) {
        if let Some(index) = layer.index {
            replace(&mut self.index.include, index.include);
            replace(&mut self.index.exclude, index.exclude);
            replace(&mut self.index.load_paths, index.load_paths);
            replace(&mut self.index.max_files, index.max_files);
        }
        if let Some(log) = layer.log {
            replace(&mut self.log.level, log.level);
            replace(&mut self.log.file, log.file);
            replace(&mut self.log.file_path, log.file_path);
            replace(&mut self.log.file_level, log.file_level);
        }
        if let Some(gems) = layer.gems {
            replace(&mut self.gems.enabled, gems.enabled);
            replace(&mut self.gems.default_gems, gems.default_gems);
            replace(&mut self.gems.paths, gems.paths);
            replace(&mut self.gems.max_files, gems.max_files);
            if gems.ruby_version.is_some() {
                self.gems.ruby_version = gems.ruby_version;
            }
        }
        if let Some(rbs) = layer.rbs {
            replace(&mut self.rbs.enabled, rbs.enabled);
            replace(&mut self.rbs.stdlib, rbs.stdlib);
            if rbs.path.is_some() {
                self.rbs.path = rbs.path;
            }
        }
        if let Some(rails) = layer.rails {
            replace(&mut self.rails.enabled, rails.enabled);
            replace(&mut self.rails.schema, rails.schema);
            replace(&mut self.rails.models, rails.models);
            replace(&mut self.rails.routes, rails.routes);
            replace(&mut self.rails.entrypoints, rails.entrypoints);
            replace(&mut self.rails.views, rails.views);
        }
        if let Some(trees) = layer.trees {
            // `test` and `migration` replace, so they take the layer's list whole, including an
            // empty one (how a fence is turned off). `test_support` extends, so it is assigned like
            // any other field: *its* list is the additive one the user wrote.
            if trees.test.is_some() {
                self.trees.test = trees.test;
            }
            if trees.migration.is_some() {
                self.trees.migration = trees.migration;
            }
            replace(&mut self.trees.test_support, trees.test_support);
        }
        if let Some(types) = layer.types {
            replace(&mut self.types.structs, types.structs);
            replace(&mut self.types.annotations, types.annotations);
            replace(&mut self.types.guess_from_names, types.guess_from_names);
        }
        if let Some(hints) = layer.hints {
            replace(&mut self.hints.block_parameters, hints.block_parameters);
            replace(&mut self.hints.locals, hints.locals);
            replace(&mut self.hints.returns, hints.returns);
        }
        if let Some(diagnostics) = layer.diagnostics {
            replace(&mut self.diagnostics.enabled, diagnostics.enabled);
            // Rules merge key by key, so a project file can silence one rule without restating the
            // client's.
            if let Some(rules) = diagnostics.rules {
                self.diagnostics.rules.extend(rules);
            }
        }
    }
}

impl Config {
    /// Which tables a layer actually moved, for the one line saying what a config file did.
    ///
    /// Tables, not keys. A key-by-key diff would be a second copy of the schema to keep in step.
    /// The log reader needs the pointer (*this project configures gems and hints*), not the values,
    /// which are in the file.
    ///
    /// Empty means the file changed nothing, which is worth saying: a `ya-lsp.toml` whose keys are
    /// all defaults reads exactly like one never found.
    #[must_use]
    pub fn changed_from_defaults(&self) -> Vec<&'static str> {
        let defaults = Self::default();
        let mut changed = Vec::new();
        for (name, differs) in [
            ("index", self.index != defaults.index),
            ("log", self.log != defaults.log),
            ("rails", self.rails != defaults.rails),
            ("trees", self.trees != defaults.trees),
            ("gems", self.gems != defaults.gems),
            ("rbs", self.rbs != defaults.rbs),
            ("types", self.types != defaults.types),
            ("hints", self.hints != defaults.hints),
            ("diagnostics", self.diagnostics != defaults.diagnostics),
        ] {
            if differs {
                changed.push(name);
            }
        }
        changed
    }
}

fn replace<T>(slot: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *slot = value;
    }
}

/// The outcome of loading configuration, including anything the user should be told about.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// `Some` when a `ya-lsp.toml` was found and read.
    pub path: Option<PathBuf>,
    /// Human-readable problems to surface to the editor. Never fatal: a broken config falls back to
    /// defaults instead of taking the server down.
    pub problems: Vec<String>,
}

/// Load configuration for `root`, layering `initializationOptions` under `ya-lsp.toml`.
#[must_use]
pub fn load(root: &Path, initialization_options: Option<&serde_json::Value>) -> Loaded {
    let mut config = Config::default();
    let mut problems = Vec::new();

    if let Some(options) = initialization_options {
        match serde_json::from_value::<PartialConfig>(options.clone()) {
            Ok(layer) => config.apply(layer),
            Err(error) => problems.push(messages::initialization_options_ignored(&error)),
        }
    }

    let path = root.join(CONFIG_FILE_NAME);
    let found = match std::fs::read_to_string(&path) {
        Ok(text) => {
            match toml::from_str::<PartialConfig>(&text) {
                Ok(layer) => config.apply(layer),
                // serde's message already names the offending key and lists the valid ones.
                Err(error) => {
                    problems.push(messages::config_file_ignored(CONFIG_FILE_NAME, &error));
                }
            }
            Some(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            problems.push(messages::config_file_unreadable(&path, &error));
            None
        }
    };

    problems.extend(validate(&config));

    Loaded {
        config,
        path: found,
        problems,
    }
}

/// One `trees.migration` entry, split into the directory and the name of the one above it.
///
/// The **last** separator, so `lib/data/migrat` reads as directory `data` holding the tree: the
/// shape a deeper path takes. An entry with no separator is not a pair and is refused, not guessed
/// at.
#[must_use]
pub fn migration_pair(entry: &str) -> Option<(&str, &str)> {
    let (parent, mark) = entry.rsplit_once('/')?;
    let parent = parent.rsplit('/').next().unwrap_or(parent);
    (!parent.is_empty() && !mark.is_empty()).then_some((parent, mark))
}

/// Catch the mistakes that would otherwise show up as "the server indexes nothing".
fn validate(config: &Config) -> Vec<String> {
    let mut problems = Vec::new();

    for (field, patterns) in [
        ("index.include", &config.index.include),
        ("index.exclude", &config.index.exclude),
    ] {
        for pattern in patterns {
            if let Err(error) = glob::Pattern::new(pattern) {
                problems.push(messages::invalid_glob(field, pattern, &error));
            }
        }
    }

    if config.index.include.is_empty() {
        problems.push(messages::include_is_empty(&IndexConfig::default().include));
    }
    if config.index.max_files == 0 {
        problems.push(messages::max_files_is_zero(
            IndexConfig::default().max_files,
        ));
    }

    // A `trees.migration` entry with no parent is the one spelling of this key that deletes
    // answers: a bare `migrate` fences `app/services/migrate/`, which the project really does load.
    for entry in config.trees.migration.iter().flatten() {
        if migration_pair(entry).is_none() {
            problems.push(messages::migration_needs_a_parent(entry));
        }
    }

    // A configured rbs root that is not one must be reported. The fallback is silent, and nobody
    // traces "my signatures are the wrong version" back to a typo.
    if let Some(path) = &config.rbs.path
        && !path.join("core").is_dir()
    {
        problems.push(messages::rbs_path_has_no_core(path));
    }

    problems
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    fn parse(toml_text: &str) -> Config {
        let mut config = Config::default();
        config.apply(toml::from_str(toml_text).expect("valid config"));
        config
    }

    #[test]
    fn absent_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(dir.path(), None);
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.path.is_none());
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    }

    #[test]
    fn a_config_file_that_cannot_be_read_is_a_problem_not_a_missing_file() {
        // "Not there" is ordinary and says nothing. Anything else (a directory where the file
        // should be, a permission error) is a config the user believes is in effect.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(CONFIG_FILE_NAME)).unwrap();

        let loaded = load(dir.path(), None);
        assert_eq!(loaded.config, Config::default(), "the defaults still apply");
        assert!(loaded.path.is_none());
        assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
        assert!(
            loaded.problems[0].contains("could not be opened"),
            "{:?}",
            loaded.problems
        );
    }

    #[test]
    fn initialization_options_that_do_not_parse_are_reported_and_dropped() {
        // The editor's layer is the one the user cannot see. Ignoring it silently would make every
        // setting they changed appear to do nothing.
        let dir = tempfile::tempdir().unwrap();
        let options = serde_json::json!({ "index": { "max_files": "lots" } });
        let loaded = load(dir.path(), Some(&options));
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
        assert!(
            loaded.problems[0].contains("initializationOptions"),
            "{:?}",
            loaded.problems
        );
    }

    #[test]
    fn a_configuration_that_would_index_nothing_says_so() {
        // Each of these starts a server that then answers everything with silence: the hardest
        // failure to trace back to a setting.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[index]\ninclude = []\nmax_files = 0\nexclude = [\"[\"]\n",
        )
        .unwrap();

        let loaded = load(dir.path(), None);
        let joined = loaded.problems.join("\n");
        for expected in [
            "index.include is empty",
            "index.max_files is 0",
            "index.exclude has an invalid glob",
        ] {
            assert!(
                joined.contains(expected),
                "missing {expected:?} in {joined}"
            );
        }
        assert!(loaded.path.is_some(), "the file was still read");
    }

    #[test]
    fn an_rbs_path_that_is_not_a_signature_root_is_worth_saying_out_loud() {
        // The fallback is silent, and nobody traces "my signatures are the wrong version" back to a
        // typo in a path.
        let dir = tempfile::tempdir().unwrap();
        let signatures = dir.path().join("signatures");
        std::fs::create_dir_all(&signatures).unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            format!("[rbs]\npath = \"{}\"\n", signatures.display()),
        )
        .unwrap();

        let loaded = load(dir.path(), None);
        assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
        assert!(
            loaded.problems[0].contains("has no core directory"),
            "{:?}",
            loaded.problems
        );

        // With `core/` there, it is a signature root and there is nothing to report.
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        assert!(
            load(dir.path(), None).problems.is_empty(),
            "a real signature root should be silent"
        );
    }

    #[test]
    fn the_default_include_is_every_shape_ruby_is_written_in() {
        // Spelled out, not derived, because this list *is* the decision:
        // - it is what a project gets with no configuration;
        // - the extension's manifest republishes it (`tests/vscode_manifest.rs`);
        // - each entry becomes a file watcher the client registers.
        //
        // Adding one is a settings change and should read as one in the diff.
        assert_eq!(
            IndexConfig::default().include,
            vec![
                "**/*.rb",
                "**/*.erb",
                // Rails' three other template handlers, plain Ruby and read as such.
                // `erb::is_template` stays false for all three, so none is blanked.
                "**/*.jbuilder",
                "**/*.builder",
                "**/*.ruby",
                "**/*.rbs",
                "**/*.rake",
                "**/*.gemspec",
                "**/*.ru",
                "**/Rakefile",
                "**/Gemfile",
            ]
        );
    }

    #[test]
    fn partial_file_only_overrides_what_it_names() {
        let config = parse("[index]\nmax_files = 10\n");
        assert_eq!(config.index.max_files, 10);
        // Untouched fields keep their defaults rather than being reset.
        assert_eq!(config.index.include, IndexConfig::default().include);
        assert_eq!(config.gems, GemsConfig::default());
    }

    #[test]
    fn full_file_parses() {
        let config = parse(
            r#"
            [index]
            include = ["lib/**/*.rb"]
            exclude = ["spec/**/*"]
            load_paths = ["lib", "app"]
            max_files = 1234

            [gems]
            enabled = false
            ruby_version = "3.1.4"
            paths = ["/opt/gems"]
            max_files = 4321

            [diagnostics]
            enabled = true

            [diagnostics.rules]
            parse-error = "error"
            dynamic-constant-reference = "hint"
            "#,
        );

        assert_eq!(config.index.include, vec!["lib/**/*.rb"]);
        assert_eq!(config.index.max_files, 1234);
        assert!(!config.gems.enabled);
        assert_eq!(config.gems.ruby_version.as_deref(), Some("3.1.4"));
        assert_eq!(config.gems.paths, vec![PathBuf::from("/opt/gems")]);
        assert_eq!(config.gems.max_files, 4321);
        assert_eq!(
            config
                .diagnostics
                .severity("parse-error", Severity::Warning),
            Severity::Error
        );
        assert_eq!(
            config
                .diagnostics
                .severity("dynamic-constant-reference", Severity::Warning),
            Severity::Hint
        );
        // Unmentioned rules fall back to the caller's default.
        assert_eq!(
            config
                .diagnostics
                .severity("parse-warning", Severity::Warning),
            Severity::Warning
        );
    }

    #[test]
    fn the_log_table_is_read_field_by_field_like_every_other_one() {
        let config = parse(
            r#"
            [log]
            level = "warn"
            file = true
            file_path = "/var/log/ya-lsp.log"
            file_level = "trace"
            "#,
        );
        assert_eq!(config.log.level, "warn");
        assert!(config.log.file);
        assert_eq!(config.log.file_path, PathBuf::from("/var/log/ya-lsp.log"));
        assert_eq!(config.log.file_level, "trace");

        // And the defaults, which the VS Code manifest republishes: off, in `tmp/`, and louder than
        // stderr, because the reason to turn the file on is the detail stderr lacks.
        let defaults = LogConfig::default();
        assert!(!defaults.file);
        assert_eq!(defaults.file_path, PathBuf::from("tmp/ya-lsp.log"));
        assert_eq!(defaults.level, crate::logging::DEFAULT_LOG_FILTER);
        assert_eq!(defaults.file_level, "debug");

        // A table that names one key leaves the other three alone, like every other table here.
        let partial = parse("[log]\nfile = true\n");
        assert!(partial.log.file);
        assert_eq!(partial.log.level, defaults.level);
        assert_eq!(partial.log.file_path, defaults.file_path);
    }

    #[test]
    fn a_config_that_changes_nothing_says_so_and_one_that_does_names_the_tables() {
        // "It is reading my ya-lsp.toml" and "it reads it, but every key is already the default"
        // must not log identically. The second is what someone who mistyped a table name has.
        assert!(Config::default().changed_from_defaults().is_empty());
        assert!(
            parse("[types]\nguess_from_names = true\n")
                .changed_from_defaults()
                .is_empty(),
            "a value equal to the default is not a change"
        );

        assert_eq!(
            parse("[types]\nguess_from_names = false\n").changed_from_defaults(),
            vec!["types"]
        );
        // Every table, in the order they are declared in, so one reading is one line.
        let everything = parse(
            r#"
            [index]
            max_files = 1
            [log]
            file = true
            [gems]
            enabled = false
            [rbs]
            stdlib = false
            [types]
            guess_from_names = false
            [hints]
            locals = false
            [diagnostics]
            enabled = false
            "#,
        );
        assert_eq!(
            everything.changed_from_defaults(),
            vec![
                "index",
                "log",
                "gems",
                "rbs",
                "types",
                "hints",
                "diagnostics"
            ]
        );
    }

    #[test]
    fn a_migration_entry_is_a_directory_and_the_one_above_it() {
        // The **last** separator, so `lib/data/migrat` reads as directory `data` holding the tree:
        // the shape a deeper path takes.
        assert_eq!(migration_pair("db/migrat"), Some(("db", "migrat")));
        assert_eq!(migration_pair("lib/data/migrat"), Some(("data", "migrat")));
        // Half a pair is not a pair. A bare `migrate` would fence `app/services/migrate/`, which
        // the project really does load, so it is refused.
        assert_eq!(migration_pair("migrate"), None);
        assert_eq!(migration_pair("db/"), None);
        assert_eq!(migration_pair("/migrat"), None);
    }

    #[test]
    fn a_migration_entry_with_no_parent_is_reported_rather_than_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[trees]\nmigration = [\"db/migrat\", \"migrate\"]\n",
        )
        .unwrap();

        let loaded = load(dir.path(), None);
        assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
        assert!(
            loaded.problems[0].contains("trees.migration"),
            "{:?}",
            loaded.problems
        );
        // The good entry is still in force; one bad row does not throw the key away.
        assert_eq!(
            loaded.config.trees.migration.as_deref(),
            Some(["db/migrat".to_owned(), "migrate".to_owned()].as_slice())
        );
    }

    #[test]
    fn the_two_lists_that_replace_take_an_empty_list_as_a_fence_turned_off() {
        // `None` and `Some([])` are different answers, and that difference is the whole shape of
        // these keys: "nobody said" versus "no fence, and I mean it".
        assert!(Config::default().trees.test.is_none());
        assert!(Config::default().trees.migration.is_none());

        let off = parse("[trees]\ntest = []\nmigration = []\n");
        assert_eq!(off.trees.test.as_deref(), Some([].as_slice()));
        assert_eq!(off.trees.migration.as_deref(), Some([].as_slice()));

        // The additive one is a plain list, empty by default, that a layer replaces wholesale: the
        // shape `gems.paths` has.
        assert!(Config::default().trees.test_support.is_empty());
        assert_eq!(
            parse("[trees]\ntest_support = [\"fixtures\"]\n")
                .trees
                .test_support,
            vec!["fixtures"]
        );
    }

    #[test]
    fn the_rails_switch_takes_a_word_or_a_boolean_and_only_auto_asks() {
        // Both spellings: people reach for both, and a config that rejects one wastes an afternoon.
        assert_eq!(Config::default().rails.enabled, Switch::Word(Word::Auto));
        assert_eq!(
            parse("[rails]\nenabled = true\n").rails.enabled,
            Switch::Fixed(true)
        );
        assert_eq!(
            parse("[rails]\nenabled = \"off\"\n").rails.enabled,
            Switch::Word(Word::Off)
        );
        // `decide` consults detection only for `auto`, which keeps a filesystem test off the path
        // of a project that has already answered.
        assert!(Switch::Fixed(true).decide(false));
        assert!(!Switch::Fixed(false).decide(true));
        assert!(Switch::Word(Word::On).decide(false));
        assert!(!Switch::Word(Word::Off).decide(true));
        assert!(Switch::Word(Word::Auto).decide(true));
        assert!(!Switch::Word(Word::Auto).decide(false));
    }

    #[test]
    fn disabling_diagnostics_silences_every_rule() {
        let config = parse(
            "[diagnostics]\nenabled = false\n\n[diagnostics.rules]\nparse-error = \"error\"\n",
        );
        assert_eq!(
            config.diagnostics.severity("parse-error", Severity::Error),
            Severity::Off
        );
    }

    #[test]
    fn typos_are_reported_and_the_server_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[index]\nexcludes = []\n",
        )
        .unwrap();

        let loaded = load(dir.path(), None);
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
        assert!(
            loaded.problems[0].contains("excludes"),
            "{:?}",
            loaded.problems
        );
    }

    #[test]
    fn file_wins_over_initialization_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[index]\nmax_files = 1\n",
        )
        .unwrap();

        let options = serde_json::json!({ "index": { "max_files": 2, "include": ["a/**/*.rb"] } });
        let loaded = load(dir.path(), Some(&options));

        assert_eq!(
            loaded.config.index.max_files, 1,
            "file overrides the client"
        );
        assert_eq!(
            loaded.config.index.include,
            vec!["a/**/*.rb"],
            "but fields the file omits still come from the client"
        );
    }

    #[test]
    fn invalid_globs_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[index]\ninclude = [\"lib/**/[.rb\"]\n",
        )
        .unwrap();

        let loaded = load(dir.path(), None);
        assert!(
            loaded.problems.iter().any(|p| p.contains("invalid glob")),
            "{:?}",
            loaded.problems
        );
    }

    #[test]
    fn empty_include_is_reported_as_a_dead_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "[index]\ninclude = []\n").unwrap();
        let loaded = load(dir.path(), None);
        assert!(
            loaded
                .problems
                .iter()
                .any(|p| p.contains("nothing will be indexed")),
            "{:?}",
            loaded.problems
        );
    }

    #[test]
    fn ruby_that_is_not_named_rb_is_indexed_with_no_configuration() {
        // Ruby not named `.rb`: `Rakefile`, `Gemfile`, `*.gemspec`, `*.rake` and `*.ru` define
        // constants and methods like any other Ruby, and a `**/*.rb` include misses them.
        //
        // rubydex dispatches on the extension and treats everything but `.rbs` as Ruby, so no name
        // needs a special case. Only the glob did.
        let mut harness = Harness::new();
        harness.write("Rakefile", "class RakeRoot\nend\n");
        harness.write(
            "lib/tasks/build.rake",
            "class BuildTask\n  def run\n  end\nend\n",
        );
        harness.write("Gemfile", "class GemfileRoot\nend\n");
        harness.write("thing.gemspec", "class GemspecRoot\nend\n");
        harness.write("config.ru", "class RackRoot\nend\n");
        // A rackup file that is not the conventional one. An application may mount several
        // (`admin.ru`, `sidekiq.ru`), and they are Ruby by extension, not name. `**/config.ru`
        // would index none of these, and would disagree with a client that can only route by
        // extension.
        harness.write("admin.ru", "class AdminRack\nend\n");
        harness.write("ops/sidekiq.ru", "class SidekiqRack\nend\n");
        // Data Bundler writes, not Ruby. It must stay out, however similar the name.
        harness.write("Gemfile.lock", "GEM\n  specs:\n");
        let source = "BuildTask.new.run\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        for name in [
            "RakeRoot",
            "BuildTask",
            "GemfileRoot",
            "GemspecRoot",
            "RackRoot",
            "AdminRack",
            "SidekiqRack",
        ] {
            assert!(harness.has(name), "{name} was not indexed");
        }
        assert!(!harness.has("GEM"), "Gemfile.lock is not Ruby");

        // In one line: a `.rake` file's method is a go-to-definition target.
        let definition = harness.definition_at(&uri, source, "run");
        let target = definition[0]["targetUri"].as_str().expect("a target uri");
        assert!(target.ends_with("lib/tasks/build.rake"), "{definition}");
    }
}
