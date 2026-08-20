//! Finding a project's gems on disk, without Bundler and without Ruby.
//!
//! This is the moat. `Bundler.locked_gems.specs` and `Gem::Specification#full_gem_path` — what
//! every other Ruby language server calls — require a Ruby runtime inside the project's bundle.
//! Reimplementing them means knowing the filesystem shape each version manager installs into,
//! which is exactly the kind of knowledge that rots: a layout that changes upstream degrades us
//! to "no gem intelligence" with no error anywhere.
//!
//! So every layout below is covered by a fixture test that builds the tree and asserts we find
//! it. When a guess goes stale, CI says so.
//!
//! # Shape
//!
//! A *gem root* is a directory holding `gems/` (unpacked gems, one directory per
//! `name-version[-platform]`), `specifications/` (RubyGems' own serialised gemspecs), and, when
//! Bundler has checked out git sources, `bundler/gems/`. Every layout in the table below ends at
//! a directory of that shape; they differ only in how you get there.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use super::{
    bundler::{self, Lockfile, SourceKind},
    config::GemsConfig,
    ruby_version::{self, Resolved},
};

/// Process environment that changes where gems live.
///
/// Passed in rather than read at the point of use so the fixture tests can build a whole
/// version-manager tree in a temp directory and point discovery at it. Reading `std::env`
/// directly would make every one of those tests depend on the machine running them.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub gem_home: Option<PathBuf>,
    pub gem_path: Vec<PathBuf>,
    pub bundle_path: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub asdf_data_dir: Option<PathBuf>,
    pub mise_data_dir: Option<PathBuf>,
    /// Where the vendored signatures are extracted to. Not a gem path — it lives here because
    /// this struct is the seam that lets a fixture test run against a temp directory instead of
    /// the machine, and `workspace::rbs` needs exactly that seam for the same reason.
    pub xdg_cache_home: Option<PathBuf>,
    /// The Windows spelling of the same thing.
    pub local_app_data: Option<PathBuf>,
    /// Where a system-wide Ruby keeps its gems, in priority order.
    ///
    /// These are absolute paths that no environment variable steers, so they are the one rung of
    /// discovery a fixture could not neutralise while they were written into `gem_roots` — and a
    /// test whose answer depends on whether the machine running it has Ruby installed passes on
    /// a laptop and fails on CI. `from_process` fills them in; `Default` leaves them empty, so
    /// every fixture is hermetic by construction.
    pub system_roots: Vec<PathBuf>,
}

/// The system-wide gem roots `Env::from_process` searches.
const SYSTEM_GEM_ROOTS: [&str; 4] = [
    "/opt/homebrew/lib/ruby/gems",
    "/usr/local/lib/ruby/gems",
    "/usr/lib/ruby/gems",
    "/Library/Ruby/Gems",
];

impl Env {
    #[must_use]
    pub fn from_process() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
        };
        Self {
            home: var("HOME").or_else(|| var("USERPROFILE")),
            gem_home: var("GEM_HOME"),
            gem_path: std::env::var_os("GEM_PATH")
                .map(|raw| {
                    std::env::split_paths(&raw)
                        .filter(|p| !p.as_os_str().is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            bundle_path: var("BUNDLE_PATH"),
            xdg_data_home: var("XDG_DATA_HOME"),
            asdf_data_dir: var("ASDF_DATA_DIR"),
            mise_data_dir: var("MISE_DATA_DIR"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
            local_app_data: var("LOCALAPPDATA"),
            system_roots: SYSTEM_GEM_ROOTS.iter().map(PathBuf::from).collect(),
        }
    }
}

/// One resolved gem: where it lives and which of its directories are on the load path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gem {
    pub name: String,
    /// `name-version[-platform]` — the directory name RubyGems used.
    pub full_name: String,
    pub path: PathBuf,
    /// Absolute, and only the ones that exist. Usually one: `<path>/lib`.
    pub load_paths: Vec<PathBuf>,
}

/// Everything discovery found, plus everything it could not.
#[derive(Debug, Clone, Default)]
pub struct Gems {
    pub gems: Vec<Gem>,
    /// Gem roots that exist, in search order. Logged, because "which directories did you look
    /// in" is the first question when no gems are found.
    pub roots: Vec<PathBuf>,
    /// Ruby's own library directory — `lib/ruby/<abi>` — and its platform subdirectory.
    ///
    /// This is where the *default gems* actually live. Their entries under `gems/` are empty
    /// placeholder directories: `gems/json-2.18.0/` exists and holds nothing, while the code is
    /// in `lib/ruby/4.0.0/json.rb`. So resolution "succeeds" and then finds no load path, and
    /// `require "json"` has always led nowhere. Held once rather than per gem, because one
    /// directory holds all forty of them and attaching it to each would walk it forty times.
    pub ruby_lib: Vec<PathBuf>,
    pub ruby_version: Option<Resolved>,
    /// Locked gems with no directory on disk — usually default gems that ship inside Ruby
    /// itself, or a platform-specific gem this machine never installed.
    pub unresolved: Vec<String>,
    pub problems: Vec<String>,
}

impl Gems {
    /// Every load path outside the workspace, in gem order. This is what `require "..."`
    /// resolves against, after the project's own.
    ///
    /// Ruby's own library comes last: a gem that ships a newer `json` than the one inside Ruby
    /// is the whole reason `json` is also a gem, and Bundler puts the gem first.
    #[must_use]
    pub fn load_paths(&self) -> Vec<PathBuf> {
        self.gems
            .iter()
            .flat_map(|gem| gem.load_paths.iter().cloned())
            .chain(self.ruby_lib.iter().cloned())
            .collect()
    }
}

/// The gem roots on this machine, without cataloguing what is inside them.
///
/// `discover` needs the roots *and* the catalogue, which is the expensive half. `rbs::discover`
/// needs only the roots, and needs them even when `[gems] enabled = false` — where Ruby is
/// installed and whether the project's bundle should be indexed are different questions.
#[must_use]
pub fn roots(workspace_root: &Path, config: &GemsConfig, env: &Env) -> Vec<PathBuf> {
    let lockfile = read_lockfile(workspace_root);
    let version = ruby_version::resolve(
        workspace_root,
        config.ruby_version.as_deref(),
        lockfile.as_ref().map(|(_, lockfile)| lockfile),
    );
    gem_roots(
        workspace_root,
        config,
        env,
        version.as_ref().map(|it| it.version.as_str()),
    )
}

/// Locate the gems a project depends on.
///
/// Never fails. Every way this can go wrong — no lockfile, no Ruby installed, a gem root that
/// vanished — ends in an empty result plus a line in `problems`, because a server that refuses
/// to start because it could not find gems is worse than one that works without them.
#[must_use]
pub fn discover(workspace_root: &Path, config: &GemsConfig, env: &Env) -> Gems {
    let mut gems = Gems::default();

    if !config.enabled {
        return gems;
    }

    let lockfile = read_lockfile(workspace_root);

    gems.ruby_version = ruby_version::resolve(
        workspace_root,
        config.ruby_version.as_deref(),
        lockfile.as_ref().map(|(_, lockfile)| lockfile),
    );

    let version = gems.ruby_version.as_ref().map(|it| it.version.as_str());
    gems.roots = gem_roots(workspace_root, config, env, version);
    if config.default_gems {
        gems.ruby_lib = ruby_lib_dirs(&gems.roots, version);
    }

    // Ruby's own library is found before this point deliberately. A project with no bundle
    // still calls `JSON.parse` and still writes `require "forwardable"`, and the stdlib is not
    // something Bundler grants it.
    let Some((lockfile_path, lockfile)) = lockfile else {
        // Not a problem worth showing the user: plenty of Ruby projects have no bundle.
        tracing::debug!(
            "no Gemfile.lock under {}; only Ruby's own library to index",
            workspace_root.display()
        );
        return gems;
    };

    let installed = Catalogue::build(&gems.roots);
    let mut seen: HashMap<String, usize> = HashMap::new();
    // Kept by name as well as full name: a lockfile resolved for seven platforms lists
    // `nokogiri` seven times, and six of those directories will never exist on this machine.
    // Reporting those six as missing would bury the one case that matters.
    let mut unresolved: Vec<(String, String)> = Vec::new();

    for (source, spec) in lockfile.specs() {
        let resolved = match source.kind {
            SourceKind::Rubygems => installed.gem_dir(&spec.full_name(), &spec.name),
            SourceKind::Git => source
                .git_checkout_name()
                .and_then(|name| installed.git_dir(&name, &spec.name)),
            // A path source is the user's own code, sitting inside their own repository. It is
            // already covered by workspace discovery — and by the workspace's exclude globs,
            // which indexing it here would quietly bypass.
            SourceKind::Path => {
                let Some(remote) = source.remote.as_deref() else {
                    continue;
                };
                let path = workspace_root.join(remote);
                if path.starts_with(workspace_root) && path.is_dir() {
                    continue;
                }
                path.is_dir().then_some(path)
            }
            SourceKind::Plugin => continue,
        };

        // The same gem can be listed by two sources (a `PATH` override of a `GEM` entry), and a
        // lockfile that resolved for several platforms lists one spec per platform. Bundler
        // applies the first that works; so do we.
        if seen.contains_key(&spec.name) {
            continue;
        }

        let Some(path) = resolved else {
            unresolved.push((spec.name.clone(), spec.full_name()));
            continue;
        };

        seen.insert(spec.name.clone(), gems.gems.len());

        let load_paths = load_paths_for(&installed, source.kind, &path, spec);
        if load_paths.is_empty() {
            // Every require path was absolute, or none of them exists. Either way there is
            // nothing here to index.
            tracing::debug!("gem {} has no usable load path", spec.full_name());
            continue;
        }

        gems.gems.push(Gem {
            name: spec.name.clone(),
            full_name: spec.full_name(),
            path,
            load_paths,
        });
    }

    gems.unresolved = unresolved
        .into_iter()
        .filter(|(name, _)| !seen.contains_key(name))
        .map(|(_, full_name)| full_name)
        .collect();

    // A bundle we mostly could not find is the failure that matters, and it is otherwise
    // invisible: every feature simply stops finding things inside gems, which reads as a broken
    // server rather than as a missing `bundle install`.
    //
    // The threshold is a majority rather than "any", because *some* misses are normal: default
    // gems live inside Ruby itself, and a lockfile resolved for seven platforms names six
    // directories that will never exist here. Neither of those ever accounts for half a bundle.
    // Measured: a healthy Rails app resolves 151/151, a project whose Ruby is not installed
    // resolves 4/201.
    let found = gems.gems.len();
    let expected = found + gems.unresolved.len();
    if expected > 0 && found * 2 < expected {
        gems.problems.push(format!(
            "found {} but only {found} of its {expected} gems are installed anywhere ya-lsp \
             looked{}. Navigation into gems will mostly not work. Run `bundle install`, or set \
             [gems].paths in ya-lsp.toml to the output of `gem env gemdir`, or set \
             [gems].enabled = false to silence this.",
            lockfile_path.display(),
            if gems.roots.is_empty() {
                " (no gem directory found at all)".to_owned()
            } else {
                format!(
                    " ({} gem directories searched, for ruby {})",
                    gems.roots.len(),
                    gems.ruby_version
                        .as_ref()
                        .map_or("unknown", |it| it.version.as_str())
                )
            },
        ));
    }

    tracing::info!(
        "resolved {}/{} locked gems against {} gem root(s); ruby {}",
        gems.gems.len(),
        seen.len() + gems.unresolved.len(),
        gems.roots.len(),
        gems.ruby_version.as_ref().map_or_else(
            || "unknown".to_owned(),
            |it| format!("{} (from {:?})", it.version, it.source)
        ),
    );
    if !gems.unresolved.is_empty() {
        // Expected, not alarming: default gems live inside Ruby itself rather than in `gems/`,
        // and platform-specific gems for other platforms are never installed here.
        tracing::debug!(
            "{} locked gems are not installed here: {}",
            gems.unresolved.len(),
            gems.unresolved.join(", ")
        );
    }

    gems
}

/// Bundler accepts `Gemfile`/`Gemfile.lock` and the newer `gems.rb`/`gems.locked`, and
/// `BUNDLE_GEMFILE` overrides both.
fn read_lockfile(root: &Path) -> Option<(PathBuf, Lockfile)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(gemfile) = std::env::var_os("BUNDLE_GEMFILE") {
        let gemfile = root.join(gemfile);
        // `BUNDLE_GEMFILE` names the Gemfile; the lockfile sits beside it with `.lock` appended
        // — `Gemfile` -> `Gemfile.lock`, `gems.rb` -> `gems.locked`.
        if gemfile.file_name().is_some_and(|name| name == "gems.rb") {
            candidates.push(gemfile.with_file_name("gems.locked"));
        } else {
            let mut name = gemfile.clone().into_os_string();
            name.push(".lock");
            candidates.push(PathBuf::from(name));
        }
    }
    candidates.push(root.join("Gemfile.lock"));
    candidates.push(root.join("gems.locked"));

    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some((path, bundler::parse(&text)));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Gem roots
// ---------------------------------------------------------------------------

/// Every gem root that exists, in priority order.
///
/// All of them are kept rather than only the first: a project can legitimately draw from two at
/// once (a vendored bundle for its dependencies plus a user install for a gem installed by
/// hand), and each gem is matched by exact `name-version`, so an extra root can only ever
/// contribute a gem that is genuinely the one the lockfile named.
fn gem_roots(
    workspace_root: &Path,
    config: &GemsConfig,
    env: &Env,
    version: Option<&str>,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Explicit configuration. The escape hatch for every layout this table gets wrong.
    for path in &config.paths {
        candidates.push(workspace_root.join(path));
    }

    // 2. The environment, when the server was launched from an activated shell.
    candidates.extend(env.gem_home.clone());
    candidates.extend(env.gem_path.iter().cloned());

    // 3. Bundler, configured or vendored. Both end in `<path>/ruby/<abi>`.
    for base in bundle_paths(workspace_root, env) {
        candidates.extend(abi_dirs(&base.join("ruby"), version));
    }
    candidates.extend(abi_dirs(
        &workspace_root.join("vendor/bundle/ruby"),
        version,
    ));

    if let Some(home) = env.home.as_deref() {
        // 4. Version managers. `<installs>/<ruby version>/lib/ruby/gems/<abi>`.
        let asdf = env
            .asdf_data_dir
            .clone()
            .unwrap_or_else(|| home.join(".asdf"));
        let mise = env
            .mise_data_dir
            .clone()
            .unwrap_or_else(|| home.join(".local/share/mise"));

        for install in ruby_installs(&asdf.join("installs/ruby"), "", version) {
            candidates.extend(abi_dirs(&install.join("lib/ruby/gems"), version));
        }
        for install in ruby_installs(&mise.join("installs/ruby"), "", version) {
            candidates.extend(abi_dirs(&install.join("lib/ruby/gems"), version));
        }
        for install in ruby_installs(&home.join(".rbenv/versions"), "", version) {
            candidates.extend(abi_dirs(&install.join("lib/ruby/gems"), version));
        }
        // chruby and ruby-install name the directory `ruby-3.4.1`.
        for install in ruby_installs(&home.join(".rubies"), "ruby-", version) {
            candidates.extend(abi_dirs(&install.join("lib/ruby/gems"), version));
        }
        // RVM is the odd one out: its gem root is not under the Ruby install at all.
        candidates.extend(ruby_installs(&home.join(".rvm/gems"), "ruby-", version));
    }

    // 5. System-wide installs. Carried on `Env` rather than written here, because they are
    //    absolute and a test has no way to point them somewhere harmless.
    for base in &env.system_roots {
        candidates.extend(abi_dirs(base, version));
    }

    if let Some(home) = env.home.as_deref() {
        // 6. `gem install --user-install`. Modern RubyGems puts this under XDG; `~/.gem` is the
        // pre-3.2 location and stays only as a fallback. Verified against `gem env` on a real
        // machine, where the XDG path is what it reports and `~/.gem` does not exist at all.
        let xdg = env
            .xdg_data_home
            .clone()
            .unwrap_or_else(|| home.join(".local/share"));
        candidates.extend(abi_dirs(&xdg.join("gem/ruby"), version));
        candidates.extend(abi_dirs(&home.join(".gem/ruby"), version));
    }

    let mut roots = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        // The marker of a gem root is `gems/`. Checking for it rejects a `GEM_HOME` pointing at
        // something that merely exists, which would otherwise cost a directory read per gem.
        if !candidate.join("gems").is_dir() {
            continue;
        }
        // Deduplicated by the canonical path, but *stored* as spelled. Two of these entries are
        // routinely symlinks to each other (`/usr/lib/ruby/gems` and the macOS system
        // framework), so the dedup is worth a syscall.
        //
        // Storing the canonical form instead would be a bug: a vendored bundle's path is built
        // by joining the workspace root, and on a machine where that root reaches the disk
        // through a symlink — every macOS temp directory, for one — canonicalising here spells
        // its files differently from every other path in the server. That forks a second
        // document for each file and hides them from the workspace checks.
        let key = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if seen.insert(key) {
            roots.push(candidate);
        }
    }
    roots
}

/// Ruby's own library directory, derived from the gem root that sits inside a Ruby install.
///
/// The shape is fixed: RubyGems installs into `<prefix>/lib/ruby/gems/<abi>`, and the
/// interpreter's own library is its sibling at `<prefix>/lib/ruby/<abi>`. So this is a walk up
/// two directories and back down one, and the whole path shape is checked rather than just the
/// `gems/` above it: RVM's root is `~/.rvm/gems/ruby-4.0.1`, whose parent is also named `gems`
/// and whose grandparent is not a Ruby installation. A `GEM_HOME` pointing anywhere, a vendored
/// bundle, and macOS's `/Library/Ruby/Gems` are excluded the same way.
///
/// # Why exactly one, when `gem_roots` returns many
///
/// A gem root is a place a gem *might* be, and searching several costs nothing because the
/// lockfile names the exact directory to look for — a Ruby 2.6 root simply does not contain
/// `rails-8.1.3`. A library directory has no such filter: taking every one of them would index
/// macOS's system Ruby 2.6 stdlib alongside the project's 4.0, and every `URI` and `JSON` in
/// the graph would have two conflicting definitions from two different decades.
///
/// A process has one interpreter, so this has one answer: the ABI has to match the project's
/// Ruby, and nothing is returned when none does. The wrong stdlib is worse than no stdlib and it
/// fails in a way no user would ever trace back to here.
///
/// **And an unknown version returns nothing at all**, which is not the timid choice it looks
/// like. macOS ships a vestigial Ruby 2.6 at `/usr/lib/ruby/gems/2.6.0` that is on every Mac
/// whether or not anyone has installed Ruby, and `gem_roots` ends with the system paths. Falling
/// back to "the first root" therefore hands a 2026 project the standard library of 2019 —
/// verified, and not in the abstract: it answered `String` with a `bigdecimal/util.rb` monkey
/// patch from Ruby 2.6, on a machine deliberately set up to have no Ruby at all. That is exactly
/// the machine this server exists for.
///
/// The cost is that a directory of scripts with no `.ruby-version`, no `.tool-versions` and no
/// lockfile gets no default gems. Naming the Ruby is one line, and being wrong about it is
/// invisible.
fn ruby_lib_dirs(roots: &[PathBuf], version: Option<&str>) -> Vec<PathBuf> {
    let Some(wanted) = version.map(abi_of) else {
        return Vec::new();
    };

    let Some(library) = roots.iter().find_map(|root| {
        // The whole shape, not just the `gems/` above it: RVM's root is `~/.rvm/gems/ruby-4.0.1`,
        // whose parent is also called `gems` and whose grandparent is not a Ruby at all.
        let (abi, gems_dir) = (root.file_name()?, root.parent()?);
        let ruby = gems_dir.parent()?;
        if gems_dir.file_name()? != "gems" || ruby.file_name()? != "ruby" {
            return None;
        }
        if ruby.parent()?.file_name()? != "lib" {
            return None;
        }
        if abi != wanted.as_str() {
            return None;
        }
        let library = ruby.join(abi);
        library.is_dir().then_some(library)
    }) else {
        return Vec::new();
    };

    // The platform directory first, so `require "rbconfig"` finds the real one.
    let mut dirs = platform_dirs(&library);
    dirs.push(library);
    dirs
}

/// The `<arch>-<os>` subdirectories of Ruby's library directory.
///
/// Globbed rather than computed from the host triple: the directory is named by whatever
/// `RbConfig::CONFIG["arch"]` was when Ruby was built (`arm64-darwin25`, `x86_64-linux`,
/// `x64-mingw-ucrt`), and reconstructing that from Rust's own target would be a second guess at
/// the same string.
fn platform_dirs(lib: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(lib) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        // An architecture directory always names an architecture, and no stdlib library has a
        // hyphen in its directory name.
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains('-'))
        })
        .collect()
}

/// `BUNDLE_PATH`, from the environment and from `.bundle/config`.
fn bundle_paths(workspace_root: &Path, env: &Env) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    paths.extend(env.bundle_path.clone());
    if let Some(configured) = bundle_config_path(&workspace_root.join(".bundle/config")) {
        paths.push(configured);
    }
    if let Some(home) = env.home.as_deref()
        && let Some(configured) = bundle_config_path(&home.join(".bundle/config"))
    {
        paths.push(configured);
    }
    paths
        .into_iter()
        .map(|path| workspace_root.join(path))
        .collect()
}

/// `.bundle/config` is YAML, but the only key we need is a flat scalar. Reading it by hand
/// avoids a YAML dependency for one line, and a file we cannot parse degrades to "no configured
/// bundle path" rather than to an error.
fn bundle_config_path(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let Some(value) = line.trim().strip_prefix("BUNDLE_PATH:") else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// Ruby installs under `parent`, newest first, with `version` hoisted to the front.
///
/// When the requested version is not installed the others are still returned: a machine that has
/// 3.4.0 but not the 3.4.1 the lockfile asks for still has the right *gems* far more often than
/// not, and the alternative is no gem intelligence at all.
fn ruby_installs(parent: &Path, prefix: &str, version: Option<&str>) -> Vec<PathBuf> {
    named_version_dirs(parent, prefix, version)
}

/// ABI directories under `parent`, newest first.
///
/// **Always globbed, never computed.** The ABI directory is the Ruby version with its patch
/// component zeroed — Ruby 4.0.1 installs into `gems/4.0.0/` — and deriving it from the version
/// string is the single easiest way to find nothing at all. Verified on a real asdf install.
fn abi_dirs(parent: &Path, version: Option<&str>) -> Vec<PathBuf> {
    named_version_dirs(parent, "", version.map(abi_of).as_deref())
}

/// `4.0.1` -> `4.0.0`: RubyGems keys its directories by the ABI, which holds the patch at zero
/// for a whole release series. Used only to *prefer* a globbed directory, never to build a path.
fn abi_of(version: &str) -> String {
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!("{major}.{minor}.0"),
        _ => version.to_owned(),
    }
}

fn named_version_dirs(parent: &Path, prefix: &str, prefer: Option<&str>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };

    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let version = name.strip_prefix(prefix)?;
            version
                .starts_with(|c: char| c.is_ascii_digit())
                .then(|| (version.to_owned(), entry.path()))
        })
        .collect();

    found.sort_by(|(a, _), (b, _)| ruby_version::compare(b, a));

    if let Some(prefer) = prefer
        && let Some(index) = found.iter().position(|(version, _)| version == prefer)
    {
        let exact = found.remove(index);
        found.insert(0, exact);
    }

    found.into_iter().map(|(_, path)| path).collect()
}

// ---------------------------------------------------------------------------
// What is actually installed
// ---------------------------------------------------------------------------

/// Directory listings of every gem root, read once.
///
/// A lockfile has hundreds of entries and a machine can have a dozen roots; probing each pair
/// with `is_dir` is thousands of syscalls. One `read_dir` per root turns the whole thing into
/// hash lookups.
struct Catalogue {
    /// `name-version[-platform]` -> unpacked gem directory.
    gems: HashMap<String, PathBuf>,
    /// `repo-shortref` -> git checkout directory.
    git: HashMap<String, PathBuf>,
    /// Gem roots, in order, for finding `specifications/`.
    roots: Vec<PathBuf>,
}

impl Catalogue {
    fn build(roots: &[PathBuf]) -> Self {
        let mut catalogue = Self {
            gems: HashMap::new(),
            git: HashMap::new(),
            roots: roots.to_vec(),
        };
        for root in roots {
            catalogue.absorb(&root.join("gems"), true);
            catalogue.absorb(&root.join("bundler/gems"), false);
        }
        catalogue
    }

    fn absorb(&mut self, dir: &Path, unpacked: bool) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let target = if unpacked {
            &mut self.gems
        } else {
            &mut self.git
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // Earlier roots win, matching the order `gem_roots` returned them in.
            target.entry(name).or_insert_with(|| entry.path());
        }
    }

    fn gem_dir(&self, full_name: &str, _name: &str) -> Option<PathBuf> {
        self.gems.get(full_name).cloned()
    }

    /// Git sources with several gemspecs check out once and hold each gem in a subdirectory, so
    /// the checkout itself may not be the gem.
    fn git_dir(&self, checkout: &str, name: &str) -> Option<PathBuf> {
        let root = self.git.get(checkout)?;
        let nested = root.join(name);
        if nested.join("lib").is_dir() {
            return Some(nested);
        }
        Some(root.clone())
    }

    /// RubyGems' own serialised gemspec for an installed gem.
    fn specification(&self, full_name: &str) -> Option<String> {
        for root in &self.roots {
            let path = root
                .join("specifications")
                .join(format!("{full_name}.gemspec"));
            if let Ok(text) = std::fs::read_to_string(&path) {
                return Some(text);
            }
        }
        None
    }
}

/// Which of a gem's directories are on the load path.
///
/// For an installed gem this comes from RubyGems' serialised gemspec, which spells
/// `require_paths` as a plain array literal — `concurrent-ruby` really does use
/// `lib/concurrent-ruby` rather than `lib`, and assuming otherwise silently misplaces every
/// `require` in it.
///
/// Git and path sources have no serialised gemspec; theirs is the gem's own source `.gemspec`,
/// which is arbitrary Ruby we refuse to execute. Those fall back to `lib`, which is what all but
/// a handful of gems use.
fn load_paths_for(
    installed: &Catalogue,
    kind: SourceKind,
    path: &Path,
    spec: &bundler::Spec,
) -> Vec<PathBuf> {
    let declared = (kind == SourceKind::Rubygems)
        .then(|| installed.specification(&spec.full_name()))
        .flatten()
        .and_then(|text| require_paths_from_gemspec(&text))
        .unwrap_or_else(|| vec!["lib".to_owned()]);

    declared
        .into_iter()
        .filter(|relative| {
            // RubyGems inserts an *absolute* require path pointing at `extensions/...` for
            // native extensions. Those hold compiled objects and no Ruby at all, so descending
            // them is pure cost. Same rule rubydex's own Ruby-side load-path code applies.
            !Path::new(relative).is_absolute()
        })
        .map(|relative| path.join(relative))
        .filter(|path| path.is_dir())
        .collect()
}

/// Pull `require_paths` out of a serialised gemspec without executing it.
///
/// `Gem::Specification#to_ruby` always writes the array on one line, as string literals with
/// `.freeze` appended. Anything else — a gem built by a tool that formats it differently, a
/// truncated file — returns `None` so the caller falls back to `lib`.
fn require_paths_from_gemspec(text: &str) -> Option<Vec<String>> {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("s.require_paths"))?;
    let open = line.find('[')?;
    let close = line[open..].find(']')? + open;

    let paths: Vec<String> = line[open + 1..close]
        .split(',')
        .map(|entry| {
            entry
                .trim()
                .trim_end_matches(".freeze")
                .trim()
                .trim_matches(['"', '\''])
                .to_owned()
        })
        .filter(|entry| !entry.is_empty())
        .collect();

    (!paths.is_empty()).then_some(paths)
}

/// Every `.rb` file under `load_paths`.
///
/// Deliberately not the `ignore` crate's default filters: a gem is not a git checkout we should
/// be second-guessing, and for a git source the repository's own `.gitignore` can hide generated
/// files that are genuinely part of the shipped gem. Hidden directories are skipped because no
/// gem puts its library code in one.
#[must_use]
pub fn ruby_files(load_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for load_path in load_paths {
        let mut walker = ignore::WalkBuilder::new(load_path);
        walker
            .standard_filters(false)
            .hidden(true)
            .follow_links(false);
        for entry in walker.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if entry.path().extension().is_some_and(|ext| ext == "rb") {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a gem root of the canonical shape: one unpacked gem plus its serialised gemspec.
    fn install(root: &Path, full_name: &str, require_paths: &[&str]) {
        for relative in require_paths {
            std::fs::create_dir_all(root.join("gems").join(full_name).join(relative)).unwrap();
            std::fs::write(
                root.join("gems")
                    .join(full_name)
                    .join(relative)
                    .join("thing.rb"),
                "class Thing; end\n",
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join("specifications")).unwrap();
        let literal = require_paths
            .iter()
            .map(|path| format!("\"{path}\".freeze"))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            root.join("specifications").join(format!("{full_name}.gemspec")),
            format!("# -*- encoding: utf-8 -*-\nGem::Specification.new do |s|\n  s.require_paths = [{literal}]\nend\n"),
        )
        .unwrap();
    }

    fn lockfile(project: &Path, body: &str) {
        std::fs::create_dir_all(project).unwrap();
        std::fs::write(project.join("Gemfile.lock"), body).unwrap();
    }

    const RAILS_LOCK: &str = "GEM\n  remote: https://rubygems.org/\n  specs:\n    rails (8.1.3)\n\nBUNDLED WITH\n   2.6.2\n";

    #[test]
    fn rubys_own_library_is_a_load_path_so_default_gems_are_not_invisible() {
        // The layout that makes this necessary, and it is not a hypothetical: a default gem's
        // directory under `gems/` exists and is *empty*, while its code sits in Ruby's own
        // library beside it. Resolution therefore succeeds and finds nothing, which is why
        // `require "json"` has always led nowhere.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        lockfile(
            &project,
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    json (2.18.0)\n",
        );
        std::fs::write(project.join(".ruby-version"), "4.0.1\n").unwrap();

        let ruby = home.join(".asdf/installs/ruby/4.0.1/lib/ruby");
        let root = ruby.join("gems/4.0.0");
        std::fs::create_dir_all(root.join("gems/json-2.18.0")).unwrap();
        std::fs::create_dir_all(root.join("specifications")).unwrap();
        std::fs::create_dir_all(ruby.join("4.0.0/json")).unwrap();
        std::fs::write(ruby.join("4.0.0/json.rb"), "module JSON\nend\n").unwrap();
        std::fs::create_dir_all(ruby.join("4.0.0/arm64-darwin25")).unwrap();
        std::fs::write(ruby.join("4.0.0/arm64-darwin25/rbconfig.rb"), "").unwrap();

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(
            gems.ruby_lib,
            vec![ruby.join("4.0.0/arm64-darwin25"), ruby.join("4.0.0")],
            "the platform directory comes first, so `require \"rbconfig\"` finds it"
        );
        assert!(
            gems.load_paths().contains(&ruby.join("4.0.0")),
            "{:?}",
            gems.load_paths()
        );
        // Ruby's own library comes after the bundle: a gem that ships a newer `json` wins.
        assert_eq!(
            gems.load_paths().last(),
            Some(&ruby.join("4.0.0")),
            "{:?}",
            gems.load_paths()
        );
    }

    #[test]
    fn a_gem_root_outside_a_ruby_install_yields_no_library() {
        // RVM keeps its gems at `~/.rvm/gems/ruby-3.4.1`, nowhere near an interpreter, and a
        // `GEM_HOME` can point anywhere at all. Walking up two directories from those would
        // name something that is not Ruby's library, and indexing it would be worse than
        // indexing nothing.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);

        let root = home.join(".rvm/gems/ruby-4.0.1");
        install(&root, "rails-8.1.3", &["lib"]);
        // The directory the derivation would name if it did not check for `gems/`.
        std::fs::create_dir_all(home.join(".rvm/ruby-4.0.1")).unwrap();

        let env = Env {
            home: Some(home.clone()),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        // Nothing under the fixture's home, which is the only thing this test controls: the
        // machine running it has its own Ruby installations and `gem_roots` finds those too.
        assert!(
            !gems.ruby_lib.iter().any(|dir| dir.starts_with(&home)),
            "{:?}",
            gems.ruby_lib
        );
    }

    #[test]
    fn an_unknown_ruby_version_takes_no_library_at_all() {
        // The machine this server exists for: no version manager, no lockfile, no
        // `.ruby-version` — and, because it is a Mac, a vestigial Ruby 2.6 under `/usr/lib`
        // that `gem_roots` finds whatever the environment says. Falling back to "the first
        // root" here handed a 2026 project the standard library of 2019, and it showed up as
        // `String` resolving into a `bigdecimal/util.rb` monkey patch.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        // A Ruby that exists but that nothing in the project points at.
        let ruby = home.join(".asdf/installs/ruby/2.6.10/lib/ruby");
        std::fs::create_dir_all(ruby.join("gems/2.6.0/gems")).unwrap();
        std::fs::create_dir_all(ruby.join("2.6.0")).unwrap();

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.ruby_version, None, "the premise of this test");
        assert!(gems.ruby_lib.is_empty(), "{:?}", gems.ruby_lib);
    }

    #[test]
    fn a_project_with_no_bundle_still_gets_rubys_library() {
        // No Gemfile.lock at all. The stdlib is not something Bundler grants a project, and a
        // script that requires `forwardable` deserves the same answer as an application does.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ruby-version"), "4.0.1\n").unwrap();

        let ruby = home.join(".asdf/installs/ruby/4.0.1/lib/ruby");
        std::fs::create_dir_all(ruby.join("gems/4.0.0/gems")).unwrap();
        std::fs::create_dir_all(ruby.join("4.0.0")).unwrap();
        std::fs::write(
            ruby.join("4.0.0/forwardable.rb"),
            "module Forwardable\nend\n",
        )
        .unwrap();

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert!(gems.gems.is_empty());
        assert_eq!(gems.ruby_lib, vec![ruby.join("4.0.0")]);
    }

    #[test]
    fn the_abi_directory_is_globbed_rather_than_computed() {
        // The correction that makes or breaks every layout below: Ruby 4.0.1 installs into
        // `gems/4.0.0/`. Computing the directory from the version string finds nothing.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);
        std::fs::write(project.join(".ruby-version"), "4.0.1\n").unwrap();

        let root = home.join(".asdf/installs/ruby/4.0.1/lib/ruby/gems/4.0.0");
        install(&root, "rails-8.1.3", &["lib"]);

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.problems, Vec::<String>::new());
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        assert_eq!(gems.gems[0].name, "rails");
        assert!(gems.gems[0].load_paths[0].ends_with("rails-8.1.3/lib"));
    }

    #[test]
    fn every_version_manager_layout_is_found() {
        // These are filesystem-shape guesses, and a guess that goes stale degrades us to "no
        // gem intelligence" with no error anywhere. Each one gets a fixture so it fails loudly.
        let layouts: &[(&str, &str)] = &[
            ("asdf", ".asdf/installs/ruby/3.4.1/lib/ruby/gems/3.4.0"),
            (
                "mise",
                ".local/share/mise/installs/ruby/3.4.1/lib/ruby/gems/3.4.0",
            ),
            ("rbenv", ".rbenv/versions/3.4.1/lib/ruby/gems/3.4.0"),
            ("chruby", ".rubies/ruby-3.4.1/lib/ruby/gems/3.4.0"),
            ("rvm", ".rvm/gems/ruby-3.4.1"),
            ("xdg user install", ".local/share/gem/ruby/3.4.0"),
            ("legacy user install", ".gem/ruby/3.4.0"),
        ];

        for (name, relative) in layouts {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let project = dir.path().join("project");
            lockfile(&project, RAILS_LOCK);
            std::fs::write(project.join(".ruby-version"), "3.4.1\n").unwrap();
            install(&home.join(relative), "rails-8.1.3", &["lib"]);

            let env = Env {
                home: Some(home),
                ..Env::default()
            };
            let gems = discover(&project, &GemsConfig::default(), &env);
            assert_eq!(gems.gems.len(), 1, "{name} layout not found: {gems:?}");
        }
    }

    #[test]
    fn the_system_gem_roots_are_searched_but_only_when_the_environment_says_so() {
        // The four system roots are absolute, so while they were written into `gem_roots` no
        // fixture could keep them out: a machine with a system Ruby answered questions the test
        // meant to ask about its own temp directory. That is a test which passes on a laptop
        // with no Ruby and fails on CI — `workspace::rbs`'s vendored-fallback tests did exactly
        // that, finding the runner's `rbs` gem and reporting `Discovered`.
        assert!(
            Env::default().system_roots.is_empty(),
            "a default Env must reach nothing outside the fixture"
        );
        assert!(
            Env::from_process()
                .system_roots
                .contains(&PathBuf::from("/usr/lib/ruby/gems")),
            "the real environment must still search the system roots"
        );

        // And they are still searched, or a machine whose only Ruby is the system one silently
        // loses every gem.
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("system/lib/ruby/gems");
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);
        std::fs::write(project.join(".ruby-version"), "3.4.1\n").unwrap();
        install(&system.join("3.4.0"), "rails-8.1.3", &["lib"]);

        let env = Env {
            system_roots: vec![system],
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
    }

    #[test]
    fn a_vendored_bundle_and_a_configured_bundle_path_are_both_found() {
        for (name, bundle_path, config) in [
            ("vendored", "vendor/bundle", None),
            (
                "configured",
                "custom/bundle",
                Some("---\nBUNDLE_PATH: \"custom/bundle\"\n"),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            lockfile(&project, RAILS_LOCK);
            if let Some(config) = config {
                std::fs::create_dir_all(project.join(".bundle")).unwrap();
                std::fs::write(project.join(".bundle/config"), config).unwrap();
            }
            install(
                &project.join(bundle_path).join("ruby/3.4.0"),
                "rails-8.1.3",
                &["lib"],
            );

            let gems = discover(&project, &GemsConfig::default(), &Env::default());
            assert_eq!(gems.gems.len(), 1, "{name} bundle not found: {gems:?}");
        }
    }

    #[test]
    fn gem_home_and_gem_path_win_when_the_shell_set_them() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(
            &project,
            "GEM\n  specs:\n    rails (8.1.3)\n    rake (13.2.1)\n",
        );

        let gem_home = dir.path().join("gem_home");
        let extra = dir.path().join("extra");
        install(&gem_home, "rails-8.1.3", &["lib"]);
        install(&extra, "rake-13.2.1", &["lib"]);

        let env = Env {
            gem_home: Some(gem_home),
            gem_path: vec![extra],
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        // Both roots contribute: a project routinely draws from a bundle plus a user install.
        let names: Vec<&str> = gems.gems.iter().map(|gem| gem.name.as_str()).collect();
        assert_eq!(names, vec!["rails", "rake"]);
    }

    #[test]
    fn a_gems_require_path_comes_from_its_gemspec_not_from_a_guess() {
        // concurrent-ruby really does this, and assuming `lib` misplaces every require in it.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(&project, "GEM\n  specs:\n    concurrent-ruby (1.3.5)\n");
        let root = dir.path().join("gem_home");
        install(&root, "concurrent-ruby-1.3.5", &["lib/concurrent-ruby"]);

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        assert!(
            gems.gems[0].load_paths[0].ends_with("lib/concurrent-ruby"),
            "{:?}",
            gems.gems[0].load_paths
        );
    }

    #[test]
    fn a_platform_specific_gem_is_found_under_its_platform_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(
            &project,
            "GEM\n  specs:\n    nokogiri (1.18.8-arm64-darwin)\n",
        );
        let root = dir.path().join("gem_home");
        install(&root, "nokogiri-1.18.8-arm64-darwin", &["lib"]);

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
    }

    #[test]
    fn a_git_source_resolves_through_its_checkout_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(
            &project,
            "GIT\n  remote: https://github.com/mprokopov/telegram-bot\n  \
             revision: b8ebc2d491016b206e5ac1c41ee71e16ec94dbee\n  specs:\n    telegram-bot (0.16.7)\n",
        );
        let root = dir.path().join("gem_home");
        std::fs::create_dir_all(root.join("gems")).unwrap();
        std::fs::create_dir_all(root.join("bundler/gems/telegram-bot-b8ebc2d49101/lib")).unwrap();
        std::fs::write(
            root.join("bundler/gems/telegram-bot-b8ebc2d49101/lib/bot.rb"),
            "class Bot; end\n",
        )
        .unwrap();

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        assert_eq!(gems.gems[0].name, "telegram-bot");
        assert_eq!(ruby_files(&gems.gems[0].load_paths).len(), 1);
    }

    #[test]
    fn a_path_source_inside_the_workspace_is_left_to_workspace_discovery() {
        // Otherwise a Rails engine gets indexed twice — and the second pass bypasses the
        // workspace's own exclude globs.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(
            &project,
            "PATH\n  remote: engines/billing\n  specs:\n    billing (0.1.0)\n",
        );
        std::fs::create_dir_all(project.join("engines/billing/lib")).unwrap();

        let gems = discover(&project, &GemsConfig::default(), &Env::default());
        assert!(gems.gems.is_empty(), "{gems:?}");
    }

    #[test]
    fn a_native_extensions_absolute_require_path_is_skipped() {
        // RubyGems inserts one pointing at `extensions/...`, which holds compiled objects and
        // no Ruby at all.
        let paths = require_paths_from_gemspec(
            "  s.require_paths = [\"lib\".freeze, \"/abs/extensions/x\".freeze]\n",
        )
        .unwrap();
        assert_eq!(paths, vec!["lib", "/abs/extensions/x"]);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        let spec = bundler::Spec {
            name: "x".to_owned(),
            version: "1.0".to_owned(),
            platform: None,
        };
        let catalogue = Catalogue::build(&[]);
        let load_paths = load_paths_for(&catalogue, SourceKind::Git, dir.path(), &spec);
        assert_eq!(load_paths, vec![dir.path().join("lib")]);
    }

    #[test]
    fn a_machine_with_no_gems_says_so_instead_of_going_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);

        let env = Env {
            home: Some(dir.path().join("empty-home")),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert!(gems.gems.is_empty());
        // Whether a system-wide Ruby happens to exist on the machine running this test is not
        // the point; resolving *none* of the lockfile is, and that is what gets reported.
        assert!(
            gems.problems.iter().any(|p| p.contains("[gems].paths")),
            "{:?}",
            gems.problems
        );
    }

    #[test]
    fn a_bundle_that_is_mostly_not_installed_is_reported() {
        // The silent-degradation case: enough gems resolve that "found nothing" would not fire,
        // but nine tenths of the bundle is missing and every jump into it will fail. Measured
        // on a real project whose Ruby was never installed: 4 of 201.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let specs: String = (0..10).map(|i| format!("    gem{i} (1.0.0)\n")).collect();
        lockfile(&project, &format!("GEM\n  specs:\n{specs}"));

        let root = dir.path().join("gem_home");
        install(&root, "gem0-1.0.0", &["lib"]);

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.gems.len(), 1);
        assert!(
            gems.problems.iter().any(|p| p.contains("bundle install")),
            "{:?}",
            gems.problems
        );
    }

    #[test]
    fn a_fully_installed_bundle_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        // One default gem missing is normal and must not trip the warning.
        lockfile(
            &project,
            "GEM\n  specs:\n    rails (8.1.3)\n    rake (13.2.1)\n    set (1.1.0)\n",
        );
        let root = dir.path().join("gem_home");
        install(&root, "rails-8.1.3", &["lib"]);
        install(&root, "rake-13.2.1", &["lib"]);

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert_eq!(gems.gems.len(), 2);
        assert_eq!(gems.unresolved, vec!["set-1.1.0"]);
        assert!(gems.problems.is_empty(), "{:?}", gems.problems);
    }

    #[test]
    fn a_project_with_no_lockfile_is_not_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let gems = discover(dir.path(), &GemsConfig::default(), &Env::default());
        assert!(gems.gems.is_empty());
        assert!(gems.problems.is_empty(), "{:?}", gems.problems);
    }

    #[test]
    fn gem_indexing_can_be_turned_off_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);
        let root = dir.path().join("gem_home");
        install(&root, "rails-8.1.3", &["lib"]);

        let config = GemsConfig {
            enabled: false,
            ..GemsConfig::default()
        };
        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        assert!(discover(&project, &config, &env).gems.is_empty());
    }

    #[test]
    fn a_configured_path_overrides_every_guess() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);
        install(&project.join("elsewhere"), "rails-8.1.3", &["lib"]);

        let config = GemsConfig {
            paths: vec![PathBuf::from("elsewhere")],
            ..GemsConfig::default()
        };
        let gems = discover(&project, &config, &Env::default());
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
    }
}
