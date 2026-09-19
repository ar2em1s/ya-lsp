//! The workspace: its root, its configuration, and which files belong to the index.

pub mod bundler;
pub mod config;
pub mod features;
pub mod gems;
pub mod rails;
pub mod rbs;
pub mod ruby_version;
pub mod uri;

use std::path::{Path, PathBuf};

use glob::{MatchOptions, Pattern};

use crate::messages;

pub use config::{Config, Severity};
pub use features::Features;
pub use gems::{Gem, Gems};
pub use rbs::Signatures;
pub use uri::DocUri;

/// Workspace root plus the configuration that governs it.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    config: Config,
    /// Which bodies of knowledge apply here, with `rails.enabled = "auto"` already decided.
    ///
    /// Held, not recomputed: deciding reads the filesystem (`config/application.rb`, then
    /// `Gemfile.lock`). [`Workspace::reload`] rebuilds it with everything else the configuration
    /// decides.
    features: Features,
    /// Which way `rails.enabled` went and why, as one sentence.
    ///
    /// Held, not logged on the spot: the decision happens inside `load`, before `[log]` is read and
    /// the file sink exists. See [`Workspace::say_which_way_rails_went`].
    rails_detection: String,
    config_path: Option<PathBuf>,
    initialization_options: Option<serde_json::Value>,
    /// The process environment gem discovery reads. Captured once at load, so a test can substitute
    /// a whole synthetic version-manager tree.
    env: gems::Env,
    /// Resolved lazily and once, off the startup path.
    gems: Option<Gems>,
    /// Same, for Ruby's own signatures. Discovery globs the gem roots and may extract the vendored
    /// copy. Neither belongs on the startup path.
    signatures: Option<Signatures>,
}

impl Workspace {
    /// Load configuration for `root`. Never fails: a broken config falls back to defaults and
    /// reports why in `problems`.
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
        let (features, rails_detection) = Features::resolve(&root, &loaded.config);
        let workspace = Self {
            root,
            features,
            rails_detection,
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
    /// Nothing is re-read here. The caller follows with [`Workspace::reload`], so a settings change
    /// and a `ya-lsp.toml` change take the same path. The file still wins.
    pub fn set_options(&mut self, options: Option<serde_json::Value>) {
        self.initialization_options = options;
    }

    /// Re-read `ya-lsp.toml` after a `workspace/didChangeWatchedFiles`.
    pub fn reload(&mut self) -> Vec<String> {
        let loaded = config::load(&self.root, self.initialization_options.as_ref());
        (self.features, self.rails_detection) = Features::resolve(&self.root, &loaded.config);
        self.config = loaded.config;
        self.config_path = loaded.path;
        // `[gems]` and `[rbs]` may have moved. The next caller re-discovers instead of trusting a
        // stale result.
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

    /// Which bodies of knowledge apply here. See [`features`].
    #[must_use]
    pub fn features(&self) -> Features {
        self.features
    }

    /// Say which way `rails.enabled` went, once, at `info`.
    ///
    /// **Called by whoever just pointed the log**, never from `load`. Detection runs before `[log]`
    /// is read, so a line written there reaches stderr but never the log file — the file a user
    /// attaches to a bug report.
    pub fn say_which_way_rails_went(&self) {
        tracing::info!("{}", self.rails_detection);
    }

    /// The `ya-lsp.toml` actually in use, if there is one.
    #[must_use]
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// Find the project's gems, once. Cached until the configuration reloads.
    ///
    /// The walk is a few hundred `read_dir` calls. Call it off the critical path.
    pub fn gems(&mut self) -> &Gems {
        self.gems
            .get_or_insert_with(|| gems::discover(&self.root, &self.config.gems, &self.env))
    }

    /// Find Ruby's own signatures, once. Cached until the configuration reloads.
    ///
    /// Independent of `[gems] enabled`: see `rbs::newest_installed`.
    pub fn signatures(&mut self) -> &Signatures {
        self.signatures.get_or_insert_with(|| {
            rbs::discover(&self.root, &self.config.rbs, &self.config.gems, &self.env)
        })
    }

    /// Absolute load paths to resolve `require` against, workspace first.
    ///
    /// That is Ruby's order: the project's own `$LOAD_PATH` entries shadow a gem of the same name.
    #[must_use]
    pub fn load_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.project_load_paths();
        if let Some(gems) = self.gems.as_ref() {
            paths.extend(gems.load_paths());
        }
        paths
    }

    /// The project's own load paths, resolved, without the bundle's.
    ///
    /// Two callers want exactly this list: [`Workspace::external_load_paths`] below, and the tests
    /// that pin the resolution.
    #[must_use]
    pub fn project_load_paths(&self) -> Vec<PathBuf> {
        self.config
            .index
            .load_paths
            .iter()
            .filter_map(|relative| resolve_load_path(&self.root, relative))
            .collect()
    }

    /// The project's load paths that lie **outside** the workspace root.
    ///
    /// The split decides who indexes them:
    /// - **Inside the root**: the walk already collected it. Walking it again would index every
    ///   file twice: two declarations of every class, every definition listed twice.
    /// - **Outside the root**: nothing else reaches it. The walk starts at the root with
    ///   `follow_links` off, so a sibling directory, or a symlink to one, stays invisible until
    ///   something names it.
    #[must_use]
    pub fn external_load_paths(&self) -> Vec<PathBuf> {
        self.project_load_paths()
            .into_iter()
            .filter(|path| !path.starts_with(&self.root))
            .collect()
    }

    /// Walk the workspace and collect the files to index.
    #[must_use]
    pub fn discover(&self) -> Discovery {
        discover(&self.root, &self.config.index)
    }

    /// Whether [`Workspace::discover`]'s walk would have collected `path`.
    ///
    /// The file watcher's question: a change arrives as a path, and the server must decide whether
    /// this workspace indexes it. The two must never disagree:
    /// - The walk indexes it and this rejects it: the file never refreshes.
    /// - This admits it and the walk skips it: an excluded file gets indexed.
    ///
    /// So it reuses the same globs and the same walker, restricted to the directories between the
    /// root and `path`.
    ///
    /// `index.max_files` is not consulted. It is a budget over the whole index, not a property of
    /// one path, and only the caller knows how much is spent.
    #[must_use]
    pub fn indexes(&self, path: &Path) -> bool {
        indexes(&self.root, &self.config.index, path)
    }

    /// Every directory [`Workspace::discover`]'s walk went into, for the server's own watcher.
    ///
    /// **One list, used two ways**, so a watcher cannot disagree with the index:
    /// - **Linux**: each directory gets an inotify watch. A recursive watch would cost one per
    ///   directory (`node_modules`, `tmp`, `log`, a vendored bundle) against `max_user_watches`.
    /// - **Elsewhere**: one recursive watch on the root is cheap, and this list filters its events.
    ///
    /// Either way `.git` and `vendor/bundle` are out because the walk never went in. That is
    /// `index.exclude` and the hidden-file rule, not a second list to keep in step.
    ///
    /// The root is always in it: a file created directly under it is a change, and on Linux nothing
    /// else would hear it.
    #[must_use]
    pub fn watched_directories(&self) -> Vec<PathBuf> {
        watched_directories(&self.root, &self.config.index)
    }

    /// Whether the project has ruled `path` out, for a file `index.include` can never name.
    ///
    /// This is [`Workspace::indexes`] without its include half. That half lists the shapes **Ruby**
    /// is written in, so it always says no to a `db/structure.sql`. The real question is whether
    /// the user wants that directory looked at, and `index.exclude` and the hidden-file rule answer
    /// it. Same walk, same compiled globs, so the two cannot drift.
    ///
    /// The one caller is the `db/*structure.sql` reader. This is not a licence to read anything: it
    /// gates the one non-Ruby file the generator pass knows.
    #[must_use]
    pub fn admits(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let mut reported_by_the_walk = Vec::new();
        let excluded = Globs::compile(&self.config.index, &mut reported_by_the_walk).exclude;
        !matches_any(&excluded, relative)
            && !prunes_an_ancestor(&excluded, relative)
            && visible(&self.root, path)
    }
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub files: Vec<PathBuf>,
    /// True when `index.max_files` cut the walk short — the index is knowingly incomplete.
    pub truncated: bool,
    pub problems: Vec<String>,
}

/// Glob semantics: `*` never crosses a path separator, so `vendor/*` does not swallow `vendor/a/b`.
/// Only `**` spans directories, as in `.gitignore` and every other glob tool.
const MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// `index.include` and `index.exclude`, compiled.
///
/// One compiled set, two entry points: [`discover`]'s walk and [`indexes`]' single path. Two
/// implementations of the same globs could drift silently.
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

/// A configured load path as the filesystem really spells it, or `None` if it is not a directory.
///
/// Every path here is compared against the graph's URIs, and the graph never writes a `..`. A bare
/// `is_dir` check lets two spellings through that then match no document, with no warning:
/// - `load_paths = ["../shared"]`, because `is_dir` follows `..` happily.
/// - A `shared` that is a **symlink** out of the tree.
///
/// So the path is canonicalized, then, if it is inside the root, spelled back the way the root
/// spells it. That second step matters: `canonicalize` resolves every symlink, and a root often
/// reaches disk through one (`/tmp` and `/var` on macOS). A canonicalized `lib` under `/tmp/...`
/// comes back as `/private/tmp/.../lib` and stops prefixing any indexed document.
///
/// Root spelling inside, canonical spelling outside: the only pairing where both comparisons hold.
fn resolve_load_path(root: &Path, relative: &Path) -> Option<PathBuf> {
    let resolved = root.join(relative).canonicalize().ok()?;
    if !resolved.is_dir() {
        return None;
    }
    // Canonicalize the root too: the question is whether the *real* directories nest.
    // `<root>/../shared` can canonicalize back inside a root that is itself a symlink.
    match root.canonicalize() {
        Ok(canonical_root) => match resolved.strip_prefix(&canonical_root) {
            Ok(inside) => Some(root.join(inside)),
            Err(_) => Some(resolved),
        },
        // A root that cannot be canonicalized was deleted under us. The resolved path is still the
        // best answer, and the walk would have failed on it too.
        Err(_) => Some(resolved),
    }
}

/// The ignore rules, spelled once. The other half of what both entry points share.
///
/// **No ignore file of any kind is read.** A `.gitignore` can name a tracked file, which git keeps
/// indexing whatever the ignore file says. Honouring it would leave a cursor in that file answering
/// nothing, and a rename would leave the file behind.
///
/// [`pruning_walker`] prunes whole trees from `index.exclude` instead: a list the user can read in
/// their own config. It is the only thing that can hide a file, which keeps "why is this file not
/// indexed?" answerable.
fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(true)
        .follow_links(false)
        // Every ignore-file source the crate has, turned off by name. `ignore` enables them all by
        // default, so leaving one out brings it back.
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .parents(false);
    walker
}

/// [`walker`], with `index.exclude` pruning directories, not only filtering files.
///
/// **`Globs::admits` tests files, and a file test cannot stop a walk.** Without pruning, the walk
/// would descend all of `node_modules` or `vendor/bundle`, then drop every entry one glob at a
/// time.
///
/// Deliberately **not** in [`walker`]: [`visible`] sets its own `filter_entry` and ignores both
/// glob lists, and `ignore` keeps one filter instead of composing them.
fn pruning_walker(root: &Path, excluded: Vec<Pattern>) -> ignore::WalkBuilder {
    let mut walker = walker(root);
    let base = root.to_path_buf();
    // Asked of every entry, not only directories: same answer, fewer branches.
    // - A *file* the list names would be dropped by `Globs::admits` a step later anyway.
    // - A *directory* it names must not be entered.
    //
    // The root strips to the empty path, which matches no pattern, so the walk cannot prune itself.
    walker.filter_entry(move |entry| {
        entry
            .path()
            .strip_prefix(&base)
            .is_ok_and(|relative| !matches_any(&excluded, relative))
    });
    walker
}

fn discover(root: &Path, index: &config::IndexConfig) -> Discovery {
    let mut problems = Vec::new();
    let globs = Globs::compile(index, &mut problems);

    let mut files = Vec::new();
    let mut truncated = false;

    for entry in pruning_walker(root, globs.exclude.clone()).build() {
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
            // A folder with no Ruby in it is not a misconfiguration. In a multi-root workspace it
            // is ordinary, and "widen index.include" would be the wrong remedy. Still logged, so a
            // user whose features answer nothing has something to point at, but not as a
            // notification.
            tracing::debug!(
                "no Ruby file under {}: navigation, completion and diagnostics answer nothing \
                 for this folder",
                root.display()
            );
        } else {
            // Hand-written globs that match nothing: the case the warning is for. Otherwise every
            // feature just returns nothing, which reads as "the server is broken", not "the server
            // indexed nothing".
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

/// The walk's directories, as [`Workspace::watched_directories`] describes them.
///
/// Free, not only a method: a watcher outlives the `Workspace` it came from (the analysis thread
/// takes ownership at startup) and re-runs this itself when a directory appears.
#[must_use]
pub fn watched_directories(root: &Path, index: &config::IndexConfig) -> Vec<PathBuf> {
    // Already reported by the walk, which runs beside this. Saying it again adds nothing.
    let mut reported_by_the_walk = Vec::new();
    let excluded = Globs::compile(index, &mut reported_by_the_walk).exclude;
    // `pruning_walker` never enters an excluded directory, so `vendor/bundle` and `node_modules`
    // are simply absent. The root is in the list because the walk yields it: a file written
    // directly under it is a change, and on Linux nothing else would hear it.
    pruning_walker(root, excluded)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_dir()))
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

/// Whether [`discover`] would have collected `path`. See [`Workspace::indexes`].
fn indexes(root: &Path, index: &config::IndexConfig, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    // An uncompilable pattern drops itself, and the walk already reported it. Reporting it again
    // per changed file would repeat it hundreds of times during a branch switch.
    let mut reported_by_the_walk = Vec::new();
    let globs = Globs::compile(index, &mut reported_by_the_walk);

    globs.admits(relative) && !prunes_an_ancestor(&globs.exclude, relative) && visible(root, path)
}

/// Whether `index.exclude` names a **directory** on the way to `relative`.
///
/// [`pruning_walker`] never enters one, so a file inside is never collected, whatever its own name.
/// This predicate must agree, or the watcher and the walk silently disagree about that file for the
/// life of the process. It checks the ancestors only; `relative` itself is [`Globs::admits`]'s
/// half.
///
/// A glob test, not a walk, because that is what the walker's filter does: this list against one
/// directory path at a time.
fn prunes_an_ancestor(excluded: &[Pattern], relative: &Path) -> bool {
    relative
        .ancestors()
        .skip(1)
        .any(|ancestor| !ancestor.as_os_str().is_empty() && matches_any(excluded, ancestor))
}

/// Whether the walk would reach `path` at all, ignoring both glob lists.
///
/// The hidden-file rule stays in the walker. This descends only the directories between the root
/// and `path`, a handful of `read_dir` calls, instead of re-implementing it.
///
/// [`walker`], not [`pruning_walker`]: the exclude list is the caller's half (`indexes` asks
/// `Globs::admits` first). Pruning here too would mix *is it excluded* into *is it hidden*.
fn visible(root: &Path, path: &Path) -> bool {
    let wanted = path.to_path_buf();
    walker(root)
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

    /// [`names`] with the walk's order taken out, for a fixture spanning several directories.
    fn sorted(discovery: &Discovery, root: &Path) -> Vec<String> {
        let mut found = names(discovery, root);
        found.sort();
        found
    }

    fn watched(root: &Path, index: &config::IndexConfig) -> Vec<String> {
        let mut found: Vec<String> = watched_directories(root, index)
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn an_invalid_glob_is_reported_against_the_field_it_came_from() {
        // Both pattern lists share one compiler. The field name is the only thing in the message
        // that tells a user which `ya-lsp.toml` key to fix. A bad pattern drops only itself: an
        // unreadable `exclude` must not take `include` down and leave the workspace unindexed.
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
        // One unreadable directory does not make a workspace unindexable. The walker reports the
        // failure per entry; swallowing it would silently lose whatever was under there.
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
        // `config_path` feeds the startup log and any "which settings am I running?" question. It
        // is `None` for defaults, never a guessed path, so a project without a `ya-lsp.toml` is
        // never reported as having one.
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
    fn no_ignore_file_hides_a_file_any_more_wherever_it_is_written() {
        // **Why ignore files are not read**: a `.gitignore` can name a **tracked** file. Lobsters'
        // names `app/views/about/about.*`, which the project ships and a deployment replaces.
        // Honouring it would leave the template answering nothing, and a rename of a constant it
        // uses would leave it behind: the outcome `renaming.md` exists to prevent.
        //
        // The outer file covers a checkout under a directory its parent repository ignores.
        // `.ignore`, another tool's convention, is not read either.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "/checkout/**/*\n");
        let root = dir.path().join("checkout");
        write(&root, ".gitignore", "generated/\napp/views/about/about.*\n");
        write(&root, ".ignore", "lib/tool.rb\n");
        write(&root, "app/views/about/about.html.erb", "x");
        write(&root, "generated/out.rb", "x");
        write(&root, "lib/tool.rb", "x");

        let discovery = discover(&root, &config::IndexConfig::default());
        assert_eq!(
            sorted(&discovery, &root),
            vec![
                "app/views/about/about.html.erb",
                "generated/out.rb",
                "lib/tool.rb"
            ]
        );
        assert!(discovery.problems.is_empty(), "{:?}", discovery.problems);
    }

    /// A folder with no Ruby in it says nothing to the user.
    ///
    /// The warning's remedy, widen `index.include`, is wrong for someone who never narrowed it. In
    /// a multi-root workspace a docs folder beside a Ruby one is ordinary. The message goes to the
    /// log.
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
    /// This is the case the warning is for, and the test above must not silence it: every feature
    /// answering nothing reads as a broken server, not an empty index.
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
    /// Excluding everything leaves `include` at its default, so a guard that watched only `include`
    /// would stay silent.
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
    fn index_exclude_prunes_the_directory_rather_than_only_filtering_its_files() {
        // **A file test cannot stop a walk**, so the walk prunes. Without it, `vendor/bundle` would
        // be descended in full and dropped one glob at a time, and every directory in it would get
        // an inotify watch.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "lib/thing.rb", "x");
        write(root, "vendor/bundle/ruby/gems/shout/lib/shout.rb", "x");

        let index = config::IndexConfig::default();
        assert_eq!(names(&discover(root, &index), root), vec!["lib/thing.rb"]);
        // `vendor` itself stays: the default pattern is `vendor/**/*`, and `vendor`'s own path does
        // not match it.
        assert_eq!(watched(root, &index), vec!["", "lib", "vendor"]);
    }

    #[test]
    fn a_file_named_by_index_exclude_is_still_excluded_on_its_own() {
        // Pruning is **additive**: a glob that matches no directory prunes nothing, and the file
        // test does the work. That keeps `index.exclude` able to name a single file, the only way
        // to drop one template from the index.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "app/views/about/about.html.erb", "x");
        write(root, "app/views/about/_subnav.html.erb", "x");

        let index = config::IndexConfig {
            exclude: vec!["app/views/about/about.*".to_owned()],
            ..config::IndexConfig::default()
        };
        assert_eq!(
            sorted(&discover(root, &index), root),
            vec!["app/views/about/_subnav.html.erb"]
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
    /// Deliberately not `ignore::Walk`: the expected list must come from something the walker had
    /// no part in, or the two agree by construction and the assertion proves nothing.
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

    /// The defaults, plus the `index.exclude` entries [`rule_fixture`] exists to exercise.
    ///
    /// Written out here, not in the fixture: two of the three tests that read it assert against
    /// `discover` with this exact list. It is also the only thing that can hide a file, so
    /// exclusions from an ignore file would test a rule the walk does not have.
    fn rule_config() -> config::IndexConfig {
        let defaults = config::IndexConfig::default();
        let mut exclude = defaults.exclude.clone();
        exclude.push("generated".to_owned());
        exclude.push("**/*.gen.rb".to_owned());
        exclude.push("app/secret.rb".to_owned());
        config::IndexConfig {
            exclude,
            ..defaults
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
        // Ruby that is not `.rb`, and the project's own signatures: an extension at the root, an
        // extension nested, and the two fixed names.
        write(root, "sig/thing.rbs", "class Thing end");
        write(root, "Rakefile", "task :default");
        write(root, "config.ru", "run App");
        // A rackup file under its own name, at depth. `**/*.ru` takes it, so an application that
        // mounts several sees them all.
        write(root, "ops/admin.ru", "run Admin");
        write(root, "thing.gemspec", "Gem::Specification.new");
        write(root, "lib/tasks/build.rake", "task :build");
        // `index.exclude`, both a default entry and the depth `*` must not reach.
        write(
            root,
            "vendor/bundle/ruby/3.4.0/gems/rails/lib/rails.rb",
            "x",
        );
        write(root, "tmp/cache/thing.rb", "x");
        // Ignore files, written and *not read*. A walk that started honouring one again shows up
        // here, not in a corpus sweep.
        write(root, ".gitignore", "generated/\n*.gen.rb\n");
        write(root, ".ignore", "lib/thing.rb\n");
        write(root, "app/.gitignore", "secret.rb\n");
        // What `rule_config` excludes instead, in the three shapes a user writes:
        // - a whole tree by its own name;
        // - a glob reaching across directories;
        // - one file at a fixed path, with `app/models/secret.rb` beside it as the control: naming
        //   one file names only that file.
        write(root, "generated/out.rb", "x");
        write(root, "lib/thing.gen.rb", "x");
        write(root, "lib/keep.gen.rb", "x");
        write(root, "app/secret.rb", "x");
        write(root, "app/models/secret.rb", "x");
        // Hidden, which the walker prunes at the directory.
        write(root, ".hidden/thing.rb", "x");
        // The one shape `index.include` can never name, in each of the four positions
        // `Workspace::admits` must answer differently. None changes what the walk collects, because
        // none is Ruby.
        write(root, "db/structure.sql", "CREATE TABLE t (id bigint);");
        write(root, "tmp/db/structure.sql", "x");
        write(root, "vendor/db/structure.sql", "x");
        write(root, "db/ignored_structure.sql", "x");
        write(root, "generated/structure.sql", "x");

        dir
    }

    /// `Workspace::admits` is `indexes` without its include half, and strictly wider.
    ///
    /// A second predicate that drifts from the first is silent for the life of the process, as in
    /// `the_predicate_answers_exactly_what_the_walk_collected`. Everything the walk collects must
    /// be admitted here too, or a rule would apply to Ruby but not to the one non-Ruby file this
    /// server reads.
    #[test]
    fn what_the_project_excluded_is_excluded_for_a_file_the_include_cannot_name() {
        let dir = rule_fixture();
        let root = dir.path();
        // Through the real loader, so the exclude list is one a user could write, not one built
        // past the parser.
        std::fs::write(
            root.join("ya-lsp.toml"),
            "[index]\nexclude = [\"vendor/**/*\", \"tmp/**/*\", \"db/ignored_*\", \"generated\"]\n",
        )
        .unwrap();
        let (workspace, problems) = Workspace::load(root.to_path_buf(), None);
        assert!(problems.is_empty(), "{problems:?}");
        let index = workspace.config().index.clone();

        let rows = [
            // `index.include` rejects every one of these, since it lists the shapes Ruby is written
            // in. So `indexes` cannot be the gate.
            ("db/structure.sql", true),
            ("tmp/db/structure.sql", false),
            ("vendor/db/structure.sql", false),
            ("db/ignored_structure.sql", false),
            // Inside an excluded **directory**: `generated/structure.sql` matches no pattern,
            // `generated` does, and the walk never enters. Without `prunes_an_ancestor`, a dump
            // would be read out of a tree the project told the server to skip.
            ("generated/structure.sql", false),
            // Not there at all, and outside the root, which a watcher can send.
            ("db/absent_structure.sql", false),
        ];
        let answers: Vec<(&str, bool)> = rows
            .iter()
            .map(|(path, _)| (*path, workspace.admits(&root.join(path))))
            .collect();
        assert_eq!(answers, rows.to_vec());
        assert!(!workspace.admits(Path::new("/somewhere/else/db/structure.sql")));
        for path in &rows {
            assert!(!workspace.indexes(&root.join(path.0)), "{}", path.0);
        }

        // Wider, over every file in the tree: what the walk collects, this admits.
        let mut present = Vec::new();
        every_file(root, &mut present);
        for path in &present {
            assert!(
                !indexes(root, &index, path) || workspace.admits(path),
                "{} is indexed and not admitted",
                path.strip_prefix(root).unwrap().display()
            );
        }
    }

    /// The predicate and the walk are one set of rules, asserted against each other.
    ///
    /// The file watcher rests on this. Both failures are silent for the life of the process:
    /// - The walk indexes a file the predicate rejects: it never refreshes after a change on disk.
    /// - The predicate admits a file the walk skips: an excluded file gets indexed.
    ///
    /// So they are checked against each other over every path in a tree, not each against a
    /// hand-written list that could be wrong the same way twice.
    #[test]
    fn the_predicate_answers_exactly_what_the_walk_collected() {
        let dir = rule_fixture();
        let root = dir.path();
        let index = rule_config();

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
        let mut indexed: Vec<String> = collected
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        indexed.sort();
        assert_eq!(
            indexed,
            vec![
                "Rakefile",
                "app/models/secret.rb",
                "app/models/user.rb",
                "config.ru",
                "lib/deep/a/b/c.rb",
                "lib/tasks/build.rake",
                "lib/thing.rb",
                "ops/admin.ru",
                "sig/thing.rbs",
                "thing.gemspec",
            ],
            "the include list, the three exclude shapes, the hidden directory — and \
             `lib/thing.rb`, which an unread `.ignore` names and which is indexed anyway"
        );
    }

    /// Every file the walk indexes lives in a directory the watcher listens to.
    ///
    /// **Another entry point on the walk's rules, and one that fails silently.** An indexed file
    /// whose directory is missing from this list never refreshes. On Linux nothing listens inside
    /// it; elsewhere this same list filters its events out. Asserted against `discover` itself,
    /// because a hand-written expectation can be wrong the same way twice.
    ///
    /// The other direction is deliberately *not* asserted. The list is wider than the files'
    /// parents on purpose: `app/assets` holds no Ruby today, but a generator may put some there.
    /// Listening too widely costs a dropped notification; listening too narrowly costs a file
    /// nobody notices is stale.
    #[test]
    fn every_indexed_file_sits_in_a_directory_the_watcher_listens_to() {
        let dir = rule_fixture();
        let root = dir.path();
        let index = rule_config();

        let watched: std::collections::HashSet<PathBuf> =
            watched_directories(root, &index).into_iter().collect();
        assert!(watched.contains(root), "the root is always watched");

        let collected = discover(root, &index).files;
        assert!(collected.len() > 1, "{collected:?}");
        for file in &collected {
            let parent = file.parent().expect("a file has a parent");
            assert!(
                watched.contains(parent),
                "{} is indexed and nothing is watching {}",
                file.strip_prefix(root).unwrap().display(),
                parent.strip_prefix(root).unwrap_or(parent).display()
            );
        }

        // It really is the walk's answer, not every directory on disk. A directory is out when it
        // is hidden, or when `index.exclude` names the tree's top directory or only what lies under
        // it.
        let relative: Vec<String> = watched
            .iter()
            .filter_map(|path| path.strip_prefix(root).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        for out in [".hidden", "generated", "vendor/bundle", "tmp/cache"] {
            assert!(
                !relative.iter().any(|path| path == out),
                "{out} is watched and nothing in it is indexed: {relative:?}"
            );
        }
        // `vendor/**/*` does not name `vendor`, and one watch on it is what notices a directory
        // appearing there. The same for `tmp`.
        for kept in ["vendor", "tmp"] {
            assert!(
                relative.iter().any(|path| path == kept),
                "{kept} itself is not excluded: {relative:?}"
            );
        }
    }

    /// A path the walk could never have produced is not indexed.
    ///
    /// The client's watcher is shared across every server it runs, so a change from another
    /// project, or a directory, can arrive here. Both must answer `false` from the rules, not from
    /// a walk that happens to find nothing.
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
        // Spelled the way the *root* is, not the filesystem. A macOS temp directory reaches disk
        // through `/var -> /private/var`, so a canonicalized `lib` would come back under
        // `/private/...` and stop prefixing any indexed document.
        assert!(
            workspace.load_paths()[0].starts_with(root),
            "a load path inside the root keeps the root's spelling: {:?}",
            workspace.load_paths()[0]
        );
        // Inside the root, so nothing here is the walk's to be told about a second time.
        assert!(workspace.external_load_paths().is_empty());
    }

    #[test]
    fn a_load_path_that_leaves_the_root_resolves_and_is_external() {
        // `../shared` passes `is_dir`, which follows `..`, but matches no document: the graph never
        // writes a URI with `..` in it. `require` would silently resolve nothing.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join("shared/models")).unwrap();
        std::fs::write(base.join("shared/models/user.rb"), "class User; end\n").unwrap();
        let root = base.join("app");
        write(&root, "lib/thing.rb", "x");
        std::fs::write(
            root.join("ya-lsp.toml"),
            "[index]\nload_paths = [\"../shared\"]\n",
        )
        .unwrap();

        let (workspace, problems) = Workspace::load(root.clone(), None);
        assert!(problems.is_empty(), "{problems:?}");
        let resolved = workspace.project_load_paths();
        assert_eq!(resolved.len(), 1, "{resolved:?}");
        assert!(
            !resolved[0].to_string_lossy().contains(".."),
            "a `..` survives into every prefix comparison downstream: {:?}",
            resolved[0]
        );
        assert!(resolved[0].ends_with("shared"), "{:?}", resolved[0]);
        // Outside the root, so the walk never reaches it and something else has to.
        assert_eq!(workspace.external_load_paths(), resolved);
    }

    #[test]
    #[cfg(unix)]
    fn a_load_path_that_is_a_symlink_out_of_the_tree_resolves_to_where_the_files_are() {
        // The other spelling of the same monorepo. `follow_links` is off in the walk, so it never
        // indexes a symlinked directory. Naming it here is the only route, and only if the link
        // resolves to its target.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join("shared/models")).unwrap();
        std::fs::write(base.join("shared/models/user.rb"), "class User; end\n").unwrap();
        let root = base.join("app");
        write(&root, "lib/thing.rb", "x");
        std::os::unix::fs::symlink(base.join("shared"), root.join("shared")).unwrap();
        std::fs::write(
            root.join("ya-lsp.toml"),
            "[index]\nload_paths = [\"shared\"]\n",
        )
        .unwrap();

        let (workspace, _) = Workspace::load(root.clone(), None);
        let external = workspace.external_load_paths();
        assert_eq!(external.len(), 1, "{external:?}");
        assert!(
            external[0].join("models/user.rb").is_file(),
            "the link has to resolve to the directory that really holds the files: {:?}",
            external[0]
        );
        // The guard, and the point of *external*: the link sits inside the root. A resolution that
        // stopped at the link would put it on the wrong side of the split, and the walk would be
        // expected to have indexed it. It did not.
        assert!(
            !external[0].starts_with(&root),
            "resolved past the link, not to it: {:?}",
            external[0]
        );
    }

    #[test]
    fn a_load_path_that_is_not_a_directory_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "lib/thing.rb", "x");
        std::fs::write(root.join("notes.txt"), "not a directory").unwrap();
        std::fs::write(
            root.join("ya-lsp.toml"),
            "[index]\nload_paths = [\"notes.txt\", \"nowhere\", \"lib\"]\n",
        )
        .unwrap();

        let (workspace, _) = Workspace::load(root.to_path_buf(), None);
        assert_eq!(
            workspace.project_load_paths(),
            vec![root.join("lib")],
            "a file and a missing directory are both dropped, and the real one survives"
        );
    }
}
