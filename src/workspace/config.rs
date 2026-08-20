//! `ya-lsp.toml` — project configuration.
//!
//! Precedence: `ya-lsp.toml` > the client's `initializationOptions` > defaults. The file wins
//! so a project can commit one setup that works for the whole team regardless of editor, and
//! an absent file must always mean a working server.
//!
//! Deliberately *not* reusing rubydex's `rubydex.toml`: that is rubydex's own linter config,
//! and a project already using rubydex should not have ya-lsp silently reinterpreting it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;

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
    pub gems: GemsConfig,
    pub rbs: RbsConfig,
    pub diagnostics: DiagnosticsConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexConfig {
    /// Globs, relative to the workspace root, of files to index.
    pub include: Vec<String>,
    /// Globs, relative to the workspace root, to skip.
    pub exclude: Vec<String>,
    /// Extra roots to index and to resolve `require` against.
    pub load_paths: Vec<PathBuf>,
    /// Hard ceiling on indexed files. Refuse rather than thrash.
    pub max_files: usize,
    /// Skip anything git ignores. Correct for a code indexer by default (it prunes
    /// `vendor/bundle`, `node_modules`, `tmp`), but a generated-source project may need it off.
    pub respect_gitignore: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GemsConfig {
    pub enabled: bool,
    /// Index the gems that ship inside Ruby itself — `json`, `uri`, `forwardable`, `optparse`
    /// and ~40 others — by adding Ruby's own library directory as a load path.
    ///
    /// Separate from `enabled` because it is a separate directory with a separate cost, and
    /// because much of it is described twice once `[rbs] stdlib` is on: the signatures have the
    /// types, Ruby's own library has the implementation, and a project may reasonably want one
    /// without the other.
    pub default_gems: bool,
    /// Override when auto-detection picks the wrong Ruby.
    pub ruby_version: Option<String>,
    /// Gem roots to search *before* the built-in table, relative to the workspace root or
    /// absolute. Each should be a directory containing `gems/` — the output of `gem env gemdir`.
    ///
    /// The escape hatch for a layout the table does not know: a container image, a Nix store
    /// path, a hand-rolled install. Without it, a machine we guess wrong about has no recourse
    /// at all, and the whole table is guesswork about other people's filesystems.
    pub paths: Vec<PathBuf>,
    /// Hard ceiling on indexed gem files, separate from `index.max_files` so a large bundle
    /// cannot quietly consume the workspace's budget.
    pub max_files: usize,
}

/// Ruby's own signatures: `String`, `Array`, `Hash`, `Kernel`, and the stdlib.
///
/// These are RBS files, not Ruby, and they come from the `rbs` gem — or, when no Ruby is
/// installed, from the copy vendored into this binary. See `workspace::rbs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RbsConfig {
    /// Index Ruby's core signatures. Off means the graph holds the five built-in classes
    /// rubydex fabricates and nothing else — which is what every release before M7 shipped.
    pub enabled: bool,
    /// Also index the stdlib signatures: `Set`, `CSV`, `URI`, `Pathname`, `Logger`, and the
    /// other ~57 libraries. Measured at ~3.5 ms added to the resolve every request pays, so it
    /// is separable from core, which costs ~0.05 ms.
    pub stdlib: bool,
    /// An explicit rbs root — a directory holding `core/`, usually an unpacked `rbs-x.y.z` gem.
    /// Skips discovery, and skips the vendored copy.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsConfig {
    pub enabled: bool,
    /// Per-rule severity, keyed by rubydex's own rule name (`parse-error`,
    /// `dynamic-constant-reference`, ...) — the same string the editor shows in a diagnostic's
    /// `code` field.
    ///
    /// Rules absent from the map use the default in `analysis::diagnostics`, where most of them
    /// are `Off` because they fire on correct Ruby. Names are validated there too: this map is
    /// open-ended by necessity, so a typo has to be caught against the real rule list.
    pub rules: BTreeMap<String, Severity>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            include: vec!["**/*.rb".to_owned()],
            exclude: vec![
                "vendor/**/*".to_owned(),
                ".bundle/**/*".to_owned(),
                "tmp/**/*".to_owned(),
                "node_modules/**/*".to_owned(),
            ],
            load_paths: vec![PathBuf::from("lib"), PathBuf::from("app")],
            max_files: 50_000,
            respect_gitignore: true,
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
// Wire format — every field optional, so "absent" is distinguishable from
// "explicitly set to the default value". Layering needs that distinction.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialConfig {
    pub index: Option<PartialIndex>,
    pub gems: Option<PartialGems>,
    pub rbs: Option<PartialRbs>,
    pub diagnostics: Option<PartialDiagnostics>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialIndex {
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub load_paths: Option<Vec<PathBuf>>,
    pub max_files: Option<usize>,
    pub respect_gitignore: Option<bool>,
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
            replace(&mut self.index.respect_gitignore, index.respect_gitignore);
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
        if let Some(diagnostics) = layer.diagnostics {
            replace(&mut self.diagnostics.enabled, diagnostics.enabled);
            // Rules merge key-by-key rather than wholesale, so a project file can silence one
            // rule without restating everything the client already configured.
            if let Some(rules) = diagnostics.rules {
                self.diagnostics.rules.extend(rules);
            }
        }
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
    /// Human-readable problems worth surfacing to the editor. Never fatal: a broken config
    /// degrades to defaults rather than taking the server down.
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
            Err(error) => problems.push(format!("ignoring initializationOptions: {error}")),
        }
    }

    let path = root.join(CONFIG_FILE_NAME);
    let found = match std::fs::read_to_string(&path) {
        Ok(text) => {
            match toml::from_str::<PartialConfig>(&text) {
                Ok(layer) => config.apply(layer),
                // serde's message already names the offending key and lists the valid ones.
                Err(error) => problems.push(format!("ignoring {CONFIG_FILE_NAME}: {error}")),
            }
            Some(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            problems.push(format!("could not read {}: {error}", path.display()));
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

/// Catch the mistakes that would otherwise show up as "the server indexes nothing".
fn validate(config: &Config) -> Vec<String> {
    let mut problems = Vec::new();

    for (field, patterns) in [
        ("index.include", &config.index.include),
        ("index.exclude", &config.index.exclude),
    ] {
        for pattern in patterns {
            if let Err(error) = glob::Pattern::new(pattern) {
                problems.push(format!("{field}: invalid glob {pattern:?}: {error}"));
            }
        }
    }

    if config.index.include.is_empty() {
        problems.push("index.include is empty: nothing will be indexed".to_owned());
    }
    if config.index.max_files == 0 {
        problems.push("index.max_files is 0: nothing will be indexed".to_owned());
    }

    // A configured rbs root that is not one is worth saying out loud: the fallback is silent,
    // and "my signatures are the wrong version" is not a symptom anyone traces back to a typo.
    if let Some(path) = &config.rbs.path
        && !path.join("core").is_dir()
    {
        problems.push(format!(
            "rbs.path {} has no core/ directory: falling back to the vendored signatures",
            path.display()
        ));
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;

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
            respect_gitignore = false

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
        assert!(!config.index.respect_gitignore);
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
}
