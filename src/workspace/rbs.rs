//! Ruby's own signatures: where they come from, and how they reach the graph as files.
//!
//! Ruby's core classes are written in C, so no Ruby source on disk defines `String#upcase`. What
//! exists instead is RBS — the signature language `ruby/rbs` maintains — and rubydex indexes it
//! natively: `LanguageId::Rbs`, dispatched off the `.rbs` extension by `index_files`. So the work
//! here is not indexing. It is deciding *which* copy of the signatures to index, and making sure
//! there is always one.
//!
//! # The ladder
//!
//! 1. **`[rbs] path`** — an explicit root. The escape hatch, same role `[gems] paths` plays.
//! 2. **On disk** — the highest-versioned `rbs-*` gem holding a `core/`, in the gem roots gem
//!    discovery already knows how to find. This is the copy that matches the Ruby the project
//!    actually runs, so it wins whenever it exists.
//! 3. **Vendored** — the copy `build.rs` embedded. The only rung that survives `PATH` pointing at
//!    an empty directory, which is the situation this whole server is built for.
//!
//! # Why the vendored copy is written to disk
//!
//! rubydex keys documents by `Url::from_file_path`, and go-to-definition on `String#upcase` has
//! to answer with a URI the editor can open. An in-memory document would resolve and hover and
//! then fail at the one moment the user asked to see it. So the embedded copy is extracted once
//! to a cache directory and indexed from there.
//!
//! That is not an index cache. Nothing is read back that this binary did not just write, the
//! directory is keyed by the version the binary carries, and a failed extraction falls back to
//! having no signatures rather than to having wrong ones — so none of the invalidation problems
//! that make caching the *graph* a bad trade apply.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    config::{GemsConfig, RbsConfig},
    gems::{self, Env},
};
use crate::messages;

/// The vendored signatures, as `(relative path, contents)`, plus the `VERSION` they came from.
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/rbs_embedded.rs"));
}

/// The `rbs` release the vendored signatures were taken from.
///
/// Public because `--licenses` names it: a reader holding only the binary needs to know which
/// signatures are in it, and the notice is worth little without that.
#[must_use]
pub fn vendored_version() -> &'static str {
    embedded::VERSION
}

/// Which rung of the ladder answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `[rbs] path` named it.
    Configured,
    /// An `rbs-*` gem on this machine.
    Discovered,
    /// The copy vendored into this binary, extracted to the cache directory.
    Vendored,
}

impl Origin {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Configured => "configured",
            Origin::Discovered => "discovered",
            Origin::Vendored => "vendored",
        }
    }
}

/// A resolved set of signature files, ready to hand to `index_files`.
#[derive(Debug, Clone, Default)]
pub struct Signatures {
    /// The directory holding `core/` and `stdlib/`. Kept for logging and for the URI prefix
    /// that keeps these documents out of `is_own_code`.
    pub root: PathBuf,
    pub version: String,
    pub origin: Option<Origin>,
    /// `core/**/*.rbs` — the classes the interpreter itself provides.
    pub core: Vec<PathBuf>,
    /// `stdlib/**/*.rbs`. Empty when `[rbs] stdlib = false`.
    pub stdlib: Vec<PathBuf>,
    pub problems: Vec<String>,
}

impl Signatures {
    /// Every file to index, core first.
    #[must_use]
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = self.core.clone();
        files.extend(self.stdlib.iter().cloned());
        files
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.core.is_empty() && self.stdlib.is_empty()
    }
}

/// Find the signatures to index.
///
/// Never fails. Every way this can go wrong ends in an empty result plus a line in `problems`:
/// a server that will not start because it could not find `String` is worse than one that starts
/// without it.
#[must_use]
pub fn discover(
    workspace_root: &Path,
    config: &RbsConfig,
    gems_config: &GemsConfig,
    env: &Env,
) -> Signatures {
    let mut signatures = Signatures::default();
    if !config.enabled {
        return signatures;
    }

    let Some((root, version, origin)) =
        locate(workspace_root, config, gems_config, env, &mut signatures)
    else {
        return signatures;
    };

    signatures.core = rbs_files(&root.join("core"));
    if config.stdlib {
        signatures.stdlib = rbs_files(&root.join("stdlib"));
    }
    if signatures.core.is_empty() {
        signatures
            .problems
            .push(messages::no_core_signatures(&root.join("core")));
    }
    tracing::info!(
        "rbs {version} ({}) at {}: {} core, {} stdlib files",
        origin.as_str(),
        root.display(),
        signatures.core.len(),
        signatures.stdlib.len()
    );

    signatures.root = root;
    signatures.version = version;
    signatures.origin = Some(origin);
    signatures
}

/// Walk the ladder. `None` only when even the vendored copy could not be written out.
fn locate(
    workspace_root: &Path,
    config: &RbsConfig,
    gems_config: &GemsConfig,
    env: &Env,
    signatures: &mut Signatures,
) -> Option<(PathBuf, String, Origin)> {
    if let Some(configured) = &config.path {
        let root = workspace_root.join(configured);
        if root.join("core").is_dir() {
            let version = version_of(&root).unwrap_or_else(|| "configured".to_owned());
            return Some((root, version, Origin::Configured));
        }
        // Not fatal, and already reported by `config::validate` — but say which path was
        // skipped, because the fallback is otherwise indistinguishable from success.
        tracing::warn!("rbs.path {} has no core/; falling back", root.display());
    }

    if let Some((root, version)) = newest_installed(workspace_root, gems_config, env) {
        return Some((root, version, Origin::Discovered));
    }

    match extract(env) {
        Ok(root) => Some((root, embedded::VERSION.to_owned(), Origin::Vendored)),
        Err(problem) => {
            signatures.problems.push(problem);
            None
        }
    }
}

/// The highest-versioned unpacked `rbs` gem across the machine's gem roots.
///
/// Deliberately independent of `[gems] enabled`: whether a bundle should be indexed and where
/// Ruby is installed are different questions, and someone who turned gem indexing off to save
/// the memory did not thereby ask for `String` to disappear.
fn newest_installed(
    workspace_root: &Path,
    gems_config: &GemsConfig,
    env: &Env,
) -> Option<(PathBuf, String)> {
    let mut best: Option<(Version, PathBuf, String)> = None;

    for root in gems::roots(workspace_root, gems_config, env) {
        let pattern = root.join("gems").join("rbs-*");
        let Ok(entries) = glob::glob(&pattern.to_string_lossy()) else {
            continue;
        };
        for path in entries.flatten() {
            // A `sig/` directory is not signatures for Ruby, it is signatures for rbs itself.
            if !path.join("core").is_dir() {
                continue;
            }
            let Some(spelled) = version_of(&path) else {
                continue;
            };
            let version = Version::parse(&spelled);
            if best
                .as_ref()
                .is_none_or(|(current, _, _)| version > *current)
            {
                best = Some((version, path, spelled));
            }
        }
    }

    best.map(|(_, path, spelled)| (path, spelled))
}

/// A gem version, ordered the way RubyGems orders one.
///
/// Only enough of it to pick between installed copies: numeric segments compare numerically,
/// and anything with a non-numeric segment (`4.2.0.pre1`) sorts below the same numbers without
/// one, which is RubyGems' rule and the one that matters — a prerelease must never outrank the
/// release it precedes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    numbers: Vec<u64>,
    /// `false` sorts first, so a prerelease loses to the release with the same numbers.
    release: bool,
}

impl Version {
    fn parse(spelled: &str) -> Self {
        let mut numbers = Vec::new();
        let mut release = true;
        for segment in spelled.split(['.', '-']) {
            match segment.parse::<u64>() {
                Ok(number) if release => numbers.push(number),
                // Everything after the first non-numeric segment is prerelease or platform
                // noise; neither participates in the numeric comparison.
                _ => {
                    release = false;
                }
            }
        }
        Self { numbers, release }
    }
}

fn version_of(root: &Path) -> Option<String> {
    root.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("rbs-"))
        .map(str::to_owned)
}

/// Write the vendored signatures out, once per version, and answer with where they went.
///
/// The marker file is written last and holds the version, so an extraction killed halfway
/// through is redone rather than half-trusted.
fn extract(env: &Env) -> Result<PathBuf, String> {
    let base = cache_dir(env).ok_or_else(messages::no_cache_directory)?;
    let root = base.join(format!("rbs-{}", embedded::VERSION));
    let marker = root.join(".complete");

    if fs::read_to_string(&marker).is_ok_and(|stamp| stamp.trim() == embedded::VERSION) {
        return Ok(root);
    }

    let started = std::time::Instant::now();
    for (relative, contents) in embedded::FILES {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| messages::signatures_not_unpacked(parent, &error))?;
        }
        fs::write(&path, contents)
            .map_err(|error| messages::signatures_not_unpacked(&path, &error))?;
    }
    fs::write(&marker, embedded::VERSION)
        .map_err(|error| messages::signatures_not_unpacked(&marker, &error))?;

    tracing::info!(
        "extracted {} vendored rbs {} files to {} in {:.2?}",
        embedded::FILES.len(),
        embedded::VERSION,
        root.display(),
        started.elapsed()
    );
    Ok(root)
}

/// Where a user's cache lives, by the convention of each platform.
fn cache_dir(env: &Env) -> Option<PathBuf> {
    if let Some(xdg) = &env.xdg_cache_home {
        return Some(xdg.join("ya-lsp"));
    }
    if let Some(local) = &env.local_app_data {
        return Some(local.join("ya-lsp").join("cache"));
    }
    let home = env.home.as_ref()?;
    // Not `~/Library/Caches` on macOS. This is developer-tool state that someone may well want
    // to `rm -rf`, and every other language server on the machine puts it in `~/.cache`.
    Some(home.join(".cache").join("ya-lsp"))
}

/// Every `.rbs` file under `directory`, sorted so the index order does not depend on the
/// filesystem's.
fn rbs_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(directory, &mut found);
    found.sort();
    found
}

fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&path, found),
            Ok(_) if path.extension().is_some_and(|extension| extension == "rbs") => {
                found.push(path);
            }
            _ => {}
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture workspace, inside the fixture's home.
    ///
    /// Inside, because `ruby_version::resolve` walks from the workspace root up to `$HOME`: a
    /// workspace in a *second* temp directory has no ceiling on that chain and would read
    /// whatever `/tmp` and `/` happen to hold on the machine running the test.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("workspace");
        (home, workspace)
    }

    fn env_with(home: &Path) -> Env {
        Env {
            home: Some(home.to_path_buf()),
            xdg_cache_home: Some(home.join("cache")),
            ..Env::default()
        }
    }

    #[test]
    fn each_origin_names_itself_the_way_the_log_and_the_docs_spell_it() {
        // The three rungs of the ladder, and the only place they are given a human name. It is
        // the string a user greps the startup log for when built-ins are missing, and it is the
        // one in `ya-lsp.toml`'s documentation — so a renamed variant must not silently rename
        // what the server says it did.
        assert_eq!(Origin::Configured.as_str(), "configured");
        assert_eq!(Origin::Discovered.as_str(), "discovered");
        assert_eq!(Origin::Vendored.as_str(), "vendored");
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_cache_is_reported_rather_than_left_half_extracted() {
        // The vendored copy is the bottom rung, and it needs to write ~250 files into
        // `~/.cache`. A cache directory that cannot be written — a read-only home, a container
        // running as a different user — has to come back as a problem the user can read.
        // Failing silently here means `String` does not exist and nothing says why.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o500)).unwrap();

        let env = Env {
            home: Some(dir.path().to_path_buf()),
            xdg_cache_home: Some(cache.clone()),
            ..Env::default()
        };
        let result = extract(&env);

        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();

        let error = result.expect_err("an unwritable cache cannot be extracted into");
        assert!(error.contains("could not be unpacked"), "{error}");
        assert!(error.contains(&cache.display().to_string()), "{error}");
    }

    #[test]
    fn versions_order_like_rubygems() {
        let order = ["3.9.0", "3.10.0", "4.0.2", "4.1.3"];
        for pair in order.windows(2) {
            assert!(
                Version::parse(pair[0]) < Version::parse(pair[1]),
                "{} should sort below {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_prerelease_loses_to_its_release() {
        assert!(Version::parse("4.1.0.pre1") < Version::parse("4.1.0"));
        // And a platform suffix does not make a gem newer than the plain build.
        assert!(Version::parse("4.1.0-java") < Version::parse("4.1.0"));
    }

    #[test]
    fn a_number_after_a_prerelease_segment_is_not_part_of_the_version() {
        // `4.1-rc.2` is 4.1 with a tag on it, not 4.1.2. Counting the trailing number would
        // make a release candidate outrank the release.
        assert_eq!(
            Version::parse("4.1-rc.2"),
            Version {
                numbers: vec![4, 1],
                release: false,
            }
        );
        assert!(Version::parse("4.1-rc.2") < Version::parse("4.1"));
        assert!(Version::parse("4.1-rc.2") < Version::parse("4.1.0"));
    }

    #[test]
    fn a_cache_directory_is_named_by_each_platforms_own_convention() {
        // Windows has no XDG variable and `~/.cache` is not where anything looks.
        let windows = Env {
            local_app_data: Some(PathBuf::from("C:/Users/x/AppData/Local")),
            home: Some(PathBuf::from("C:/Users/x")),
            ..Env::default()
        };
        assert_eq!(
            cache_dir(&windows),
            Some(PathBuf::from("C:/Users/x/AppData/Local/ya-lsp/cache"))
        );

        // XDG wins wherever it is set, on any platform.
        let xdg = Env {
            xdg_cache_home: Some(PathBuf::from("/x/.cache")),
            local_app_data: Some(PathBuf::from("C:/never")),
            home: Some(PathBuf::from("/home/x")),
            ..Env::default()
        };
        assert_eq!(cache_dir(&xdg), Some(PathBuf::from("/x/.cache/ya-lsp")));

        // Not `~/Library/Caches` on macOS: this is developer-tool state someone may want to
        // `rm -rf`, and every other language server puts it in `~/.cache`.
        let unix = Env {
            home: Some(PathBuf::from("/home/x")),
            ..Env::default()
        };
        assert_eq!(
            cache_dir(&unix),
            Some(PathBuf::from("/home/x/.cache/ya-lsp"))
        );

        // Nowhere to put it, which is what makes the vendored copy unusable.
        assert_eq!(cache_dir(&Env::default()), None);
    }

    #[test]
    fn signatures_are_empty_only_when_neither_half_has_a_file() {
        let mut signatures = Signatures::default();
        assert!(signatures.is_empty());
        signatures.stdlib.push(PathBuf::from("stdlib/set.rbs"));
        assert!(
            !signatures.is_empty(),
            "stdlib alone is still signatures to index"
        );
        signatures.core.push(PathBuf::from("core/string.rbs"));
        assert!(!signatures.is_empty());
    }

    #[test]
    fn a_configured_root_whose_core_is_empty_is_reported_rather_than_silently_useless() {
        // `core/` is there, so the ladder stops here — and there is nothing in it, so every
        // built-in class is about to be missing. Saying so is the only way anyone finds out.
        let dir = tempfile::tempdir().unwrap();
        let signatures_root = dir.path().join("signatures");
        std::fs::create_dir_all(signatures_root.join("core")).unwrap();

        let config = RbsConfig {
            path: Some(signatures_root.clone()),
            ..RbsConfig::default()
        };
        let found = discover(
            dir.path(),
            &config,
            &GemsConfig::default(),
            &env_with(dir.path()),
        );

        assert!(found.core.is_empty());
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        assert!(
            found.problems[0].contains("built-in classes will be missing"),
            "{:?}",
            found.problems
        );
    }

    #[test]
    fn an_rbs_gem_without_rubys_signatures_in_it_is_not_the_one() {
        // The `rbs` gem ships its *own* signatures in `sig/`, which are signatures for rbs
        // rather than for Ruby. Taking one of those would replace `String` with `RBS::Parser`.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        let root = home.join("gems");

        // The newest one on disk holds only its own `sig/`, so the older one wins — a copy with
        // no `core/` is not a copy of Ruby's signatures at all.
        std::fs::create_dir_all(root.join("gems/rbs-4.1.3/sig")).unwrap();
        std::fs::create_dir_all(root.join("gems/rbs-4.0.2/core")).unwrap();

        let env = Env {
            gem_home: Some(root.clone()),
            home: Some(home),
            ..Env::default()
        };
        assert_eq!(
            newest_installed(&workspace, &GemsConfig::default(), &env),
            Some((root.join("gems/rbs-4.0.2"), "4.0.2".to_owned()))
        );
    }

    #[test]
    fn an_rbs_directory_that_does_not_name_a_version_never_wins() {
        // `rbs-*` is a glob over directory names and a match is not a promise: a `git`-sourced
        // rbs unpacks as `rbs-<sha>`, and a half-deleted gem leaves `rbs-` behind. Each of
        // those still parses — into a `Version` with no numbers in it, which is what sorts them
        // below every real release rather than above one on a string comparison.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        let root = home.join("gems");

        for name in ["rbs-cafebabe", "rbs-", "rbs-head"] {
            std::fs::create_dir_all(root.join("gems").join(name).join("core")).unwrap();
        }
        std::fs::create_dir_all(root.join("gems/rbs-4.0.2/core")).unwrap();

        let env = Env {
            gem_home: Some(root.clone()),
            home: Some(home),
            ..Env::default()
        };
        assert_eq!(
            newest_installed(&workspace, &GemsConfig::default(), &env),
            Some((root.join("gems/rbs-4.0.2"), "4.0.2".to_owned()))
        );
    }

    #[test]
    fn a_gem_root_that_reads_as_a_glob_pattern_costs_only_itself() {
        // The pattern is built by joining onto a path we were handed, so a `[` anywhere above
        // the gems is a `PatternError` rather than a directory listing. Users do have such
        // paths — a checkout named `feature[2]`, a bundle under a Windows-ish directory — and
        // the cost of one has to be that root, not every built-in Ruby has.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = home.join("workspace");
        let unglobbable = home.join("gems[unclosed");
        let ordinary = home.join("gems");

        std::fs::create_dir_all(unglobbable.join("gems/rbs-9.9.9/core")).unwrap();
        std::fs::create_dir_all(ordinary.join("gems/rbs-4.0.2/core")).unwrap();

        let env = Env {
            // `gem_path` is searched after `gem_home`, so both roots are seen.
            gem_home: Some(unglobbable),
            gem_path: vec![ordinary.clone()],
            home: Some(home),
            ..Env::default()
        };
        assert_eq!(
            newest_installed(&workspace, &GemsConfig::default(), &env),
            Some((ordinary.join("gems/rbs-4.0.2"), "4.0.2".to_owned())),
            "the unglobbable root is skipped, the one beside it is not"
        );
    }

    #[test]
    fn the_vendored_copy_carries_core_and_stdlib() {
        let names: Vec<&str> = embedded::FILES.iter().map(|(name, _)| *name).collect();
        assert!(
            names.contains(&"core/string.rbs"),
            "no core/string.rbs among {} vendored files",
            names.len()
        );
        assert!(names.iter().any(|name| name.starts_with("stdlib/")));
        // The point of vendoring: `String#upcase` has to actually be in there.
        let string = embedded::FILES
            .iter()
            .find(|(name, _)| *name == "core/string.rbs")
            .expect("core/string.rbs");
        assert!(string.1.contains("def upcase"));
    }

    #[test]
    fn extraction_writes_once_and_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let env = env_with(home.path());

        let root = extract(&env).expect("extracted");
        assert!(root.join("core/string.rbs").is_file());
        assert_eq!(
            fs::read_to_string(root.join(".complete")).unwrap(),
            embedded::VERSION
        );

        // Second call must not rewrite: proven by editing a file and finding it untouched.
        fs::write(root.join("core/string.rbs"), "# clobbered\n").unwrap();
        let again = extract(&env).expect("extracted");
        assert_eq!(again, root);
        assert_eq!(
            fs::read_to_string(root.join("core/string.rbs")).unwrap(),
            "# clobbered\n"
        );

        // A half-finished extraction is redone, which is the only reason the marker exists.
        fs::remove_file(root.join(".complete")).unwrap();
        extract(&env).expect("extracted");
        assert!(
            fs::read_to_string(root.join("core/string.rbs"))
                .unwrap()
                .contains("def upcase")
        );
    }

    #[test]
    fn discovery_falls_back_to_the_vendored_copy() {
        let (home, workspace) = fixture();
        let env = env_with(home.path());

        // Stated rather than assumed: the rung below is only reached because nothing on the
        // machine is visible from here. `Env::default()` carries no system roots for exactly
        // this reason — when it did, a CI runner with a system `rbs` gem answered `Discovered`
        // and this assertion failed on a machine nobody could see.
        assert!(
            gems::roots(&workspace, &GemsConfig::default(), &env).is_empty(),
            "gem discovery escaped the fixture"
        );

        let signatures = discover(
            &workspace,
            &RbsConfig::default(),
            &GemsConfig::default(),
            &env,
        );

        assert_eq!(signatures.origin, Some(Origin::Vendored));
        assert_eq!(signatures.version, embedded::VERSION);
        assert!(!signatures.core.is_empty());
        assert!(!signatures.stdlib.is_empty());
        assert!(signatures.problems.is_empty(), "{:?}", signatures.problems);
    }

    #[test]
    fn stdlib_can_be_turned_off_without_losing_core() {
        let (home, workspace) = fixture();
        let config = RbsConfig {
            stdlib: false,
            ..RbsConfig::default()
        };
        let signatures = discover(
            &workspace,
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        assert!(!signatures.core.is_empty());
        assert!(signatures.stdlib.is_empty());
    }

    #[test]
    fn disabled_finds_nothing_at_all() {
        let (home, workspace) = fixture();
        let config = RbsConfig {
            enabled: false,
            ..RbsConfig::default()
        };
        let signatures = discover(
            &workspace,
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        assert!(signatures.is_empty());
        assert_eq!(signatures.origin, None);
    }

    #[test]
    fn the_startup_line_says_which_rbs_answered_and_how_much_it_found() {
        // The one line a user greps when `String` has no methods. It has to name the rung that
        // answered, the version, and the counts — "vendored 4.1.3, 89 core files" is the
        // difference between "no Ruby installed, working as designed" and "something is wrong".
        // A log nobody asserts is a log that quietly stops saying anything useful.
        let (home, workspace) = fixture();
        let env = env_with(home.path());

        let (signatures, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            discover(
                &workspace,
                &RbsConfig::default(),
                &GemsConfig::default(),
                &env,
            )
        });

        assert_eq!(signatures.origin, Some(Origin::Vendored));
        assert!(logged.contains("vendored"), "which rung answered: {logged}");
        assert!(
            logged.contains(&format!("rbs {}", signatures.version)),
            "which version: {logged}"
        );
        assert!(
            logged.contains(&format!("{} core", signatures.core.len())),
            "how much it found: {logged}"
        );
    }

    #[test]
    fn an_installed_gem_outranks_the_vendored_copy() {
        let (home, workspace) = fixture();

        // A gem root shaped the way `gems::roots` recognises one, with two rbs versions in it.
        let root = home.path().join("gems/ruby/3.4.0");
        for version in ["3.10.0", "4.0.2"] {
            let core = root.join(format!("gems/rbs-{version}/core"));
            fs::create_dir_all(&core).unwrap();
            fs::write(core.join("string.rbs"), "class String\nend\n").unwrap();
        }

        let mut env = env_with(home.path());
        env.gem_home = Some(root.clone());

        let signatures = discover(
            &workspace,
            &RbsConfig::default(),
            &GemsConfig::default(),
            &env,
        );

        assert_eq!(signatures.origin, Some(Origin::Discovered));
        // The newer of the two, not whichever `glob` happened to yield first.
        assert_eq!(signatures.version, "4.0.2");
        assert_eq!(signatures.core.len(), 1);
    }

    #[test]
    fn a_configured_path_outranks_everything() {
        let (home, workspace) = fixture();
        let configured = workspace.join("sig/rbs-9.9.9");
        fs::create_dir_all(configured.join("core")).unwrap();
        fs::write(configured.join("core/string.rbs"), "class String\nend\n").unwrap();

        let config = RbsConfig {
            path: Some(PathBuf::from("sig/rbs-9.9.9")),
            ..RbsConfig::default()
        };
        let signatures = discover(
            &workspace,
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        assert_eq!(signatures.origin, Some(Origin::Configured));
        assert_eq!(signatures.version, "9.9.9");
    }

    #[test]
    fn a_configured_path_that_is_not_one_falls_back() {
        let (home, workspace) = fixture();
        let config = RbsConfig {
            path: Some(PathBuf::from("nowhere")),
            ..RbsConfig::default()
        };
        let signatures = discover(
            &workspace,
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        // Degraded, not broken: the whole point is that built-ins never simply vanish.
        assert_eq!(signatures.origin, Some(Origin::Vendored));
    }
}
