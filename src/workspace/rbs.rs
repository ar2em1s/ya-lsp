//! Ruby's own signatures: where they come from, and how they reach the graph as files.
//!
//! Ruby's core classes are written in C, so there is no Ruby source anywhere on disk that
//! defines `String#upcase`. What exists instead is RBS — the signature language `ruby/rbs`
//! maintains — and rubydex indexes it natively: `LanguageId::Rbs`, dispatched off the `.rbs`
//! extension by `index_files`. So the work here is not indexing. It is deciding *which* copy of
//! the signatures to index, and making sure there is always one.
//!
//! # The ladder
//!
//! 1. **`[rbs] path`** — an explicit root. The escape hatch, same role `[gems] paths` plays.
//! 2. **On disk** — the highest-versioned `rbs-*` gem holding a `core/`, in the gem roots gem
//!    discovery already knows how to find. This is the copy that matches the Ruby the project
//!    actually runs, so it wins whenever it exists.
//! 3. **Vendored** — the copy `build.rs` embedded. The only rung that survives `PATH` pointing
//!    at an empty directory, which is the situation this whole server is built for.
//!
//! # Why the vendored copy is written to disk
//!
//! rubydex keys documents by `Url::from_file_path`, and go-to-definition on `String#upcase` has
//! to answer with a URI the editor can open. An in-memory document would resolve and hover and
//! then fail at the one moment the user asked to see it. So the embedded copy is extracted once
//! to a cache directory and indexed from there.
//!
//! This is not the index cache that §8 of PLAN.md rejected. Nothing is read back that this
//! binary did not just write, the directory is keyed by the version the binary carries, and a
//! failed extraction falls back to having no signatures rather than to having wrong ones —
//! none of the invalidation problems that made caching the *graph* a bad trade apply.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    config::{GemsConfig, RbsConfig},
    gems::{self, Env},
};

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
/// a server that will not start because it could not find `String` is worse than one that
/// starts without it, which is exactly what every release before this one did.
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
        signatures.problems.push(format!(
            "no signatures under {}: built-in classes will be missing",
            root.join("core").display()
        ));
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
    let base = cache_dir(env)
        .ok_or_else(|| "no cache directory (HOME is unset): no built-in signatures".to_owned())?;
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
                .map_err(|error| format!("creating {}: {error}", parent.display()))?;
        }
        fs::write(&path, contents)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
    }
    fs::write(&marker, embedded::VERSION)
        .map_err(|error| format!("writing {}: {error}", marker.display()))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(home: &Path) -> Env {
        Env {
            home: Some(home.to_path_buf()),
            xdg_cache_home: Some(home.join("cache")),
            ..Env::default()
        }
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
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let env = env_with(home.path());

        // Stated rather than assumed: the rung below is only reached because nothing on the
        // machine is visible from here. `Env::default()` carries no system roots for exactly
        // this reason — when it did, a CI runner with a system `rbs` gem answered `Discovered`
        // and this assertion failed on a machine nobody could see.
        assert!(
            gems::roots(workspace.path(), &GemsConfig::default(), &env).is_empty(),
            "gem discovery escaped the fixture"
        );

        let signatures = discover(
            workspace.path(),
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
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = RbsConfig {
            stdlib: false,
            ..RbsConfig::default()
        };
        let signatures = discover(
            workspace.path(),
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        assert!(!signatures.core.is_empty());
        assert!(signatures.stdlib.is_empty());
    }

    #[test]
    fn disabled_finds_nothing_at_all() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = RbsConfig {
            enabled: false,
            ..RbsConfig::default()
        };
        let signatures = discover(
            workspace.path(),
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        assert!(signatures.is_empty());
        assert_eq!(signatures.origin, None);
    }

    #[test]
    fn an_installed_gem_outranks_the_vendored_copy() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

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
            workspace.path(),
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
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let configured = workspace.path().join("sig/rbs-9.9.9");
        fs::create_dir_all(configured.join("core")).unwrap();
        fs::write(configured.join("core/string.rbs"), "class String\nend\n").unwrap();

        let config = RbsConfig {
            path: Some(PathBuf::from("sig/rbs-9.9.9")),
            ..RbsConfig::default()
        };
        let signatures = discover(
            workspace.path(),
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        assert_eq!(signatures.origin, Some(Origin::Configured));
        assert_eq!(signatures.version, "9.9.9");
    }

    #[test]
    fn a_configured_path_that_is_not_one_falls_back() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = RbsConfig {
            path: Some(PathBuf::from("nowhere")),
            ..RbsConfig::default()
        };
        let signatures = discover(
            workspace.path(),
            &config,
            &GemsConfig::default(),
            &env_with(home.path()),
        );

        // Degraded, not broken: the whole point is that built-ins never simply vanish.
        assert_eq!(signatures.origin, Some(Origin::Vendored));
    }
}
