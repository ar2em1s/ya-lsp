//! The workspace: its root, its configuration, and which files belong to the index.

pub mod bundler;
pub mod config;
pub mod gems;
pub mod rbs;
pub mod ruby_version;
pub mod uri;

use std::path::{Path, PathBuf};

use glob::{MatchOptions, Pattern};

use crate::messages;

pub use config::{Config, Severity};
pub use gems::{Gem, Gems};
pub use rbs::Signatures;
pub use uri::DocUri;

/// Workspace root plus the configuration that governs it.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    config: Config,
    config_path: Option<PathBuf>,
    initialization_options: Option<serde_json::Value>,
    /// The process environment gem discovery reads. Captured once at load so a fixture test can
    /// substitute a whole synthetic version-manager tree.
    env: gems::Env,
    /// Resolved lazily, and only once: the whole point is that it happens off the startup path.
    gems: Option<Gems>,
    /// Same, for Ruby's own signatures. Discovery is a glob over the gem roots plus, in the
    /// worst case, extracting the vendored copy — neither belongs on the startup path.
    signatures: Option<Signatures>,
}

impl Workspace {
    /// Load configuration for `root`. Never fails: a broken config degrades to defaults and
    /// reports the reason through `problems`.
    #[must_use]
    pub fn load(
        root: PathBuf,
        initialization_options: Option<serde_json::Value>,
    ) -> (Self, Vec<String>) {
        Self::load_with_env(root, initialization_options, gems::Env::from_process())
    }

    /// `load`, with the environment gem discovery sees supplied explicitly.
    #[must_use]
    pub fn load_with_env(
        root: PathBuf,
        initialization_options: Option<serde_json::Value>,
        env: gems::Env,
    ) -> (Self, Vec<String>) {
        let loaded = config::load(&root, initialization_options.as_ref());
        let workspace = Self {
            root,
            config: loaded.config,
            config_path: loaded.path,
            initialization_options,
            env,
            gems: None,
            signatures: None,
        };
        (workspace, loaded.problems)
    }

    /// Replace the client's settings layer, for `workspace/didChangeConfiguration`.
    ///
    /// Nothing is re-read here: the caller follows with [`Workspace::reload`], so a settings
    /// change and a `ya-lsp.toml` change take exactly the same path afterwards — including the
    /// precedence, which stays "the file wins" whatever the editor was just told.
    pub fn set_options(&mut self, options: Option<serde_json::Value>) {
        self.initialization_options = options;
    }

    /// Re-read `ya-lsp.toml` after a `workspace/didChangeWatchedFiles`.
    pub fn reload(&mut self) -> Vec<String> {
        let loaded = config::load(&self.root, self.initialization_options.as_ref());
        self.config = loaded.config;
        self.config_path = loaded.path;
        // `[gems]` and `[rbs]` may have moved; the next caller re-discovers rather than
        // trusting a result that was computed under the old configuration.
        self.gems = None;
        self.signatures = None;
        loaded.problems
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The `ya-lsp.toml` actually in use, if there is one.
    #[must_use]
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// Find the project's gems, once. The result is cached until the configuration reloads.
    ///
    /// The walk is a few hundred `read_dir` calls, so this is not free — call it off the
    /// critical path.
    pub fn gems(&mut self) -> &Gems {
        self.gems
            .get_or_insert_with(|| gems::discover(&self.root, &self.config.gems, &self.env))
    }

    /// Find Ruby's own signatures, once. The result is cached until the configuration reloads.
    ///
    /// Independent of `[gems] enabled`: see `rbs::newest_installed`.
    pub fn signatures(&mut self) -> &Signatures {
        self.signatures.get_or_insert_with(|| {
            rbs::discover(&self.root, &self.config.rbs, &self.config.gems, &self.env)
        })
    }

    /// Absolute load paths to resolve `require` against, workspace first.
    ///
    /// Order is Ruby's: the project's own `$LOAD_PATH` entries shadow a gem of the same name,
    /// which is what `require "version"` inside an app has always meant.
    #[must_use]
    pub fn load_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .config
            .index
            .load_paths
            .iter()
            .map(|relative| self.root.join(relative))
            .filter(|path| path.is_dir())
            .collect();
        if let Some(gems) = self.gems.as_ref() {
            paths.extend(gems.load_paths());
        }
        paths
    }

    /// Walk the workspace and collect the files to index.
    #[must_use]
    pub fn discover(&self) -> Discovery {
        discover(&self.root, &self.config.index)
    }

    /// Whether [`Workspace::discover`]'s walk would have collected `path`.
    ///
    /// The question a file watcher asks: a change arrives as a path, and the server has to
    /// decide whether it is one this workspace indexes at all. Two answers that disagree is the
    /// failure this exists to prevent — a file the walk indexes and this rejects is a file that
    /// never refreshes, and the reverse indexes something the user excluded — so it is the same
    /// globs and the same walker, restricted to the directories between the root and `path`
    /// rather than a second reading of what `.gitignore` means.
    ///
    /// `index.max_files` is deliberately not consulted: it is a budget over the whole index
    /// rather than a property of one path, and only the caller knows how much of it is spent.
    #[must_use]
    pub fn indexes(&self, path: &Path) -> bool {
        indexes(&self.root, &self.config.index, path)
    }
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub files: Vec<PathBuf>,
    /// True when `index.max_files` cut the walk short — the index is knowingly incomplete.
    pub truncated: bool,
    pub problems: Vec<String>,
}

/// Glob semantics: `*` never crosses a path separator, so `vendor/*` does not swallow
/// `vendor/a/b`. `**` is the only wildcard that spans directories, which is what users expect
/// from `.gitignore` and from every other tool that takes globs.
const MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// `index.include` and `index.exclude`, compiled.
///
/// One compiled set with two entry points — [`discover`]'s walk and [`indexes`]' one path —
/// rather than two implementations of the same globs, which is the half of the predicate that
/// could drift silently.
struct Globs {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

impl Globs {
    fn compile(index: &config::IndexConfig, problems: &mut Vec<String>) -> Self {
        Self {
            include: compile(&index.include, "index.include", problems),
            exclude: compile(&index.exclude, "index.exclude", problems),
        }
    }

    /// Whether a path *relative to the workspace root* passes both lists.
    fn admits(&self, relative: &Path) -> bool {
        if !matches_any(&self.include, relative) {
            return false;
        }
        !matches_any(&self.exclude, relative)
    }
}

fn matches_any(patterns: &[Pattern], relative: &Path) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern.matches_path_with(relative, MATCH_OPTIONS))
}

/// The ignore rules, spelled once. The other half of what both entry points share.
fn walker(root: &Path, index: &config::IndexConfig) -> ignore::WalkBuilder {
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(true)
        .follow_links(false)
        .git_ignore(index.respect_gitignore)
        .git_exclude(index.respect_gitignore)
        // Honour .gitignore even when the workspace is not itself a git repository, which is
        // common for a subdirectory opened on its own.
        .require_git(false)
        // Only rules the project itself commits. Two deliberate exclusions:
        //
        // `parents` would apply .gitignore files from *above* the workspace root. Those belong
        // to a different project and can silently empty the index — a checkout living under a
        // directory an outer repo ignores would index nothing, with no error anywhere.
        //
        // `git_global` (~/.config/git/ignore) is per-machine, so two developers on the same
        // repo would get different indexes. Keeping both off makes "why is this file not
        // indexed?" answerable from the repository alone.
        .parents(false)
        .git_global(false);
    walker
}

fn discover(root: &Path, index: &config::IndexConfig) -> Discovery {
    let mut problems = Vec::new();
    let globs = Globs::compile(index, &mut problems);

    let mut files = Vec::new();
    let mut truncated = false;

    for entry in walker(root, index).build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                problems.push(messages::workspace_scan_failed(&error));
                continue;
            }
        };

        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }

        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };

        if !globs.admits(relative) {
            continue;
        }

        if files.len() >= index.max_files {
            truncated = true;
            break;
        }
        files.push(entry.into_path());
    }

    if files.is_empty() {
        let defaults = config::IndexConfig::default();
        if index.include == defaults.include && index.exclude == defaults.exclude {
            // A folder with no Ruby in it is not a misconfiguration — it is a folder with no
            // Ruby in it, which in a multi-root workspace is an ordinary thing to have open.
            // Warning here handed that user a remedy that was wrong for them ("widen
            // index.include" when the globs are untouched and there is simply nothing to match)
            // about a folder they had not opened. Still said, because every feature answering
            // nothing needs something to point at, but said where someone reading a log will
            // find it rather than in a notification nobody asked for.
            tracing::debug!(
                "no Ruby file under {}: navigation, completion and diagnostics answer nothing \
                 for this folder",
                root.display()
            );
        } else {
            // The globs were written by hand and matched nothing, which is the case the warning
            // was always for: otherwise invisible, because every feature just returns nothing,
            // which reads as "the server is broken" rather than "the server indexed nothing".
            problems.push(messages::nothing_matched(&index.include, root));
        }
    }

    if truncated {
        problems.push(messages::index_truncated(index.max_files));
    }

    // Stable order keeps logs and test fixtures reproducible; indexing itself is parallel.
    files.sort();

    Discovery {
        files,
        truncated,
        problems,
    }
}

/// Whether [`discover`] would have collected `path`. See [`Workspace::indexes`].
fn indexes(root: &Path, index: &config::IndexConfig, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    // A pattern that does not compile drops itself and is reported by the walk, which has
    // already run by the time anything asks this. Reporting it again, once per changed file,
    // would say nothing new and say it hundreds of times during a branch switch.
    let mut reported_by_the_walk = Vec::new();
    if !Globs::compile(index, &mut reported_by_the_walk).admits(relative) {
        return false;
    }

    // `.gitignore`, `.ignore`, `.git/info/exclude` and the hidden-file rule are the walker's,
    // and the walker is where they stay: this descends only the directories between the root
    // and `path`, which is a handful of `read_dir` calls rather than a second implementation of
    // git's ignore semantics to keep in step with the first.
    let wanted = path.to_path_buf();
    walker(root, index)
        .filter_entry(move |entry| wanted.starts_with(entry.path()))
        .build()
        .filter_map(Result::ok)
        .any(|entry| entry.path() == path && entry.file_type().is_some_and(|kind| kind.is_file()))
}

fn compile(patterns: &[String], field: &str, problems: &mut Vec<String>) -> Vec<Pattern> {
    patterns
        .iter()
        .filter_map(|pattern| match Pattern::new(pattern) {
            Ok(compiled) => Some(compiled),
            Err(error) => {
                problems.push(messages::invalid_glob(field, pattern, &error));
                None
            }
        })
        .collect()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn names(discovery: &Discovery, root: &Path) -> Vec<String> {
        discovery
            .files
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn an_invalid_glob_is_reported_against_the_field_it_came_from() {
        // Both pattern lists go through the same compiler, and the field name is the only thing
        // in the message that tells a user which key in their `ya-lsp.toml` to go and fix. A
        // bad pattern drops itself and nothing else — an unreadable `exclude` must not take
        // `include` down with it and leave the workspace unindexed.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib/thing.rb", "class Thing; end\n");

        let index = config::IndexConfig {
            include: vec!["**/*.rb".to_owned(), "lib/[".to_owned()],
            exclude: vec!["vendor/[".to_owned()],
            ..config::IndexConfig::default()
        };
        let discovery = discover(dir.path(), &index);

        assert_eq!(
            names(&discovery, dir.path()),
            vec!["lib/thing.rb".to_owned()]
        );
        assert_eq!(discovery.problems.len(), 2, "{:?}", discovery.problems);
        assert!(
            discovery.problems[0].starts_with("index.include has an invalid glob \"lib/[\""),
            "{:?}",
            discovery.problems
        );
        assert!(
            discovery.problems[1].starts_with("index.exclude has an invalid glob \"vendor/[\""),
            "{:?}",
            discovery.problems
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_read_is_reported_and_the_rest_is_indexed() {
        // A workspace with one unreadable directory in it is not an unindexable workspace. The
        // walker surfaces the failure per entry, and swallowing it would leave a project
        // silently missing whatever was under there with nothing said about it.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib/thing.rb", "class Thing; end\n");
        write(dir.path(), "secret/hidden.rb", "class Hidden; end\n");
        let secret = dir.path().join("secret");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();

        let discovery = discover(dir.path(), &config::IndexConfig::default());

        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            names(&discovery, dir.path()),
            vec!["lib/thing.rb".to_owned()]
        );
        assert_eq!(discovery.problems.len(), 1, "{:?}", discovery.problems);
        assert!(
            discovery.problems[0].starts_with("part of the workspace could not be scanned"),
            "{:?}",
            discovery.problems
        );
    }

    #[test]
    fn the_config_a_workspace_actually_loaded_is_the_one_it_names() {
        // `config_path` is what the startup log and any "which settings am I running?" question
        // read. It is `None` for defaults rather than a guessed path, so that a project with no
        // `ya-lsp.toml` cannot be reported as having one.
        let dir = tempfile::tempdir().unwrap();
        let (bare, problems) =
            Workspace::load_with_env(dir.path().to_path_buf(), None, gems::Env::default());
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(bare.config_path(), None);

        write(dir.path(), "ya-lsp.toml", "[gems]\nenabled = false\n");
        let (configured, problems) =
            Workspace::load_with_env(dir.path().to_path_buf(), None, gems::Env::default());
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            configured.config_path(),
            Some(dir.path().join("ya-lsp.toml").as_path())
        );
    }

    #[test]
    fn double_star_matches_at_every_depth_including_the_root() {
        // Pins the `glob` crate's behaviour under `require_literal_separator`, which the
        // default `**/*.rb` include depends on.
        let pattern = Pattern::new("**/*.rb").unwrap();
        assert!(pattern.matches_path_with(Path::new("foo.rb"), MATCH_OPTIONS));
        assert!(pattern.matches_path_with(Path::new("lib/foo.rb"), MATCH_OPTIONS));
        assert!(pattern.matches_path_with(Path::new("lib/a/b/foo.rb"), MATCH_OPTIONS));
        assert!(!pattern.matches_path_with(Path::new("lib/foo.rbs"), MATCH_OPTIONS));

        let single = Pattern::new("vendor/*").unwrap();
        assert!(single.matches_path_with(Path::new("vendor/a"), MATCH_OPTIONS));
        assert!(
            !single.matches_path_with(Path::new("vendor/a/b"), MATCH_OPTIONS),
            "`*` must not cross a separator"
        );
    }

    #[test]
    fn finds_ruby_files_and_applies_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "app/models/user.rb", "class User; end");
        write(root, "lib/thing.rb", "module Thing; end");
        write(root, "vendor/bundle/gem.rb", "x");
        write(root, "README.md", "hi");

        let discovery = discover(root, &config::IndexConfig::default());
        assert_eq!(
            names(&discovery, root),
            vec!["app/models/user.rb", "lib/thing.rb"]
        );
        assert!(discovery.problems.is_empty(), "{:?}", discovery.problems);
    }

    #[test]
    fn gitignore_files_above_the_workspace_root_are_not_applied() {
        // Regression: a checkout under a directory that an outer repo ignores used to index
        // zero files, silently. Discovered by running the real binary against `tmp/<repo>`,
        // which this repo's own .gitignore excludes.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "/checkout/**/*\n");
        let root = dir.path().join("checkout");
        write(&root, "lib/thing.rb", "x");

        let discovery = discover(&root, &config::IndexConfig::default());
        assert_eq!(names(&discovery, &root), vec!["lib/thing.rb"]);
        assert!(discovery.problems.is_empty(), "{:?}", discovery.problems);
    }

    /// A folder with no Ruby in it says nothing to the user.
    ///
    /// The remedy the warning carries — widen `index.include` — is wrong advice for someone who
    /// never narrowed it, and in a multi-root workspace an infrastructure or docs folder sitting
    /// beside a Ruby one is ordinary rather than a mistake. What is lost goes to the log.
    #[test]
    fn a_folder_with_no_ruby_is_not_a_misconfiguration() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "README.md", "no ruby here");

        let discovery = discover(dir.path(), &config::IndexConfig::default());
        assert!(discovery.files.is_empty());
        assert!(
            discovery.problems.is_empty(),
            "untouched globs over a folder with no Ruby is not something to warn about: {:?}",
            discovery.problems
        );
    }

    /// Globs written by hand that match nothing are still reported.
    ///
    /// This is the case the warning has always been for, and the one the test above must not
    /// take down with it: the user asked for something specific, got an index of nothing, and
    /// every feature answering nothing reads as a broken server rather than an empty index.
    #[test]
    fn globs_that_were_narrowed_by_hand_and_matched_nothing_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib/thing.rb", "x");

        let index = config::IndexConfig {
            include: vec!["app/**/*.rb".to_owned()],
            ..config::IndexConfig::default()
        };

        let discovery = discover(dir.path(), &index);
        assert!(discovery.files.is_empty());
        assert!(
            discovery
                .problems
                .iter()
                .any(|p| p.contains("nothing will be indexed")),
            "{:?}",
            discovery.problems
        );
    }

    /// A hand-written `index.exclude` counts as narrowing too, not only `include`.
    ///
    /// Excluding everything is the other half of the same mistake, and reaching it through
    /// `exclude` leaves `include` at its default — so a guard that only watched `include` would
    /// fall silent on it.
    #[test]
    fn an_exclude_that_swallows_the_workspace_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib/thing.rb", "x");

        let index = config::IndexConfig {
            exclude: vec!["**/*".to_owned()],
            ..config::IndexConfig::default()
        };

        let discovery = discover(dir.path(), &index);
        assert!(discovery.files.is_empty());
        assert!(
            discovery
                .problems
                .iter()
                .any(|p| p.contains("nothing will be indexed")),
            "{:?}",
            discovery.problems
        );
    }

    #[test]
    fn respects_gitignore_by_default_and_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".gitignore", "generated/\n");
        write(root, "lib/thing.rb", "x");
        write(root, "generated/out.rb", "x");

        let mut index = config::IndexConfig::default();
        assert_eq!(names(&discover(root, &index), root), vec!["lib/thing.rb"]);

        index.respect_gitignore = false;
        assert_eq!(
            names(&discover(root, &index), root),
            vec!["generated/out.rb", "lib/thing.rb"]
        );
    }

    #[test]
    fn max_files_truncates_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..5 {
            write(root, &format!("lib/f{i}.rb"), "x");
        }

        let index = config::IndexConfig {
            max_files: 2,
            ..config::IndexConfig::default()
        };
        let discovery = discover(root, &index);

        assert_eq!(discovery.files.len(), 2);
        assert!(discovery.truncated);
        assert!(
            discovery.problems.iter().any(|p| p.contains("max_files")),
            "{:?}",
            discovery.problems
        );
    }

    // ------------------------------------------------------- the walk and the predicate

    /// Every file in a tree, ignoring nothing.
    ///
    /// Deliberately not `ignore::Walk`: the list the predicate is checked against must come
    /// from something the walker had no part in producing, or the two agree by construction
    /// and the assertion is vacuous.
    fn every_file(dir: &Path, into: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                every_file(&path, into);
            } else {
                into.push(path);
            }
        }
    }

    /// A tree holding one of every rule that decides whether a file is indexed.
    fn rule_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write(root, "app/models/user.rb", "class User; end");
        write(root, "lib/thing.rb", "module Thing; end");
        write(root, "lib/deep/a/b/c.rb", "module C; end");
        write(root, "README.md", "not ruby");
        write(root, "sig/thing.rbs", "class Thing end");
        // `index.exclude`, both a default entry and the depth `*` must not reach.
        write(
            root,
            "vendor/bundle/ruby/3.4.0/gems/rails/lib/rails.rb",
            "x",
        );
        write(root, "tmp/cache/thing.rb", "x");
        // `.gitignore` at the root, and a second one *below* it — the case a single compiled
        // ignore file at the workspace root gets wrong, silently and only for that directory.
        write(root, ".gitignore", "generated/\n*.gen.rb\n!keep.gen.rb\n");
        write(root, "generated/out.rb", "x");
        write(root, "lib/thing.gen.rb", "x");
        write(root, "lib/keep.gen.rb", "x");
        write(root, "app/.gitignore", "secret.rb\n");
        write(root, "app/secret.rb", "x");
        write(root, "app/models/secret.rb", "x");
        // Hidden, which the walker prunes at the directory.
        write(root, ".hidden/thing.rb", "x");

        dir
    }

    /// The predicate and the walk are one set of rules, asserted against each other.
    ///
    /// This is the invariant the file watcher rests on. A file the walk indexes and the
    /// predicate rejects is a file that never refreshes once it changes on disk; a file the
    /// predicate admits and the walk skips is one the user excluded and gets indexed anyway.
    /// Neither shows up as an error — both are silent for the life of the process — so the two
    /// are checked against each other over every path in a tree rather than each against a
    /// hand-written list that could be wrong in the same way twice.
    #[test]
    fn the_predicate_answers_exactly_what_the_walk_collected() {
        let dir = rule_fixture();
        let root = dir.path();
        let index = config::IndexConfig::default();

        let collected = discover(root, &index).files;
        assert!(
            collected.len() > 1,
            "a fixture the walk finds nothing in proves nothing: {collected:?}"
        );

        let mut present = Vec::new();
        every_file(root, &mut present);
        for path in &present {
            assert_eq!(
                indexes(root, &index, path),
                collected.contains(path),
                "{} is on one side and not the other",
                path.strip_prefix(root).unwrap().display()
            );
        }

        // And the fixture really does exercise each rule, rather than passing because
        // everything answered `false`.
        let indexed: Vec<String> = collected
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            indexed,
            vec![
                "app/models/user.rb",
                "lib/deep/a/b/c.rb",
                "lib/keep.gen.rb",
                "lib/thing.rb",
            ],
            "include, exclude, both .gitignore files, the negation and the hidden directory"
        );
    }

    /// A path the walk could never have produced is not indexed.
    ///
    /// The watcher is the client's, shared across every server it runs, so a change from
    /// another project or a directory rather than a file can arrive here. Both have to answer
    /// `false` from the rules rather than from a walk that happens to find nothing.
    #[test]
    fn the_predicate_refuses_what_is_not_a_file_under_this_root() {
        let dir = rule_fixture();
        let root = dir.path();
        let index = config::IndexConfig::default();

        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "lib/other.rb", "x");
        assert!(!indexes(root, &index, &outside.path().join("lib/other.rb")));

        // A directory named like an included file. `**/*.rb` matches its path, so only the
        // walk's file test keeps it out.
        std::fs::create_dir_all(root.join("lib/directory.rb")).unwrap();
        assert!(!indexes(root, &index, &root.join("lib/directory.rb")));

        // A file that is not there. The watcher's deletions come through this shape, and the
        // caller has to decide them from the graph rather than from here.
        assert!(!indexes(root, &index, &root.join("lib/deleted.rb")));
    }

    /// An unreadable glob is reported by the walk, once, and not again per changed file.
    #[test]
    fn the_predicate_does_not_re_report_what_the_walk_already_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "lib/thing.rb", "x");
        let index = config::IndexConfig {
            include: vec!["**/*.rb".to_owned(), "lib/[".to_owned()],
            ..config::IndexConfig::default()
        };

        // The bad pattern drops itself here exactly as it does in the walk, so the good one
        // still answers — and a branch switch does not push one notification per file.
        assert!(indexes(
            dir.path(),
            &index,
            &dir.path().join("lib/thing.rb")
        ));
    }

    #[test]
    fn load_paths_resolve_against_the_root_and_skip_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "lib/thing.rb", "x");

        let (workspace, problems) = Workspace::load(root.to_path_buf(), None);
        assert!(problems.is_empty(), "{problems:?}");
        // `app` does not exist, so only `lib` survives.
        assert_eq!(workspace.load_paths(), vec![root.join("lib")]);
    }
}
