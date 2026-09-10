//! Finding a project's gems on disk, without Bundler and without Ruby.
//!
//! This is the moat. The usual answer — `Bundler.locked_gems.specs` and
//! `Gem::Specification#full_gem_path` — needs a Ruby runtime inside the project's bundle.
//! Reimplementing it means knowing the filesystem shape each version manager installs into,
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
use crate::messages;

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
    /// `BUNDLE_GEMFILE`: which Gemfile — and so which lockfile — Bundler was told to use.
    pub bundle_gemfile: Option<PathBuf>,
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

/// A `PATH`-shaped variable, split the way the platform separates one.
///
/// Its own function so it can be tested without setting a process-wide environment variable,
/// which the test harness runs threads across. Empty entries are dropped: `GEM_PATH=/a::/b` and
/// a trailing separator both produce one, and an empty path would be joined onto and searched
/// as the *current directory*.
fn split_path_list(raw: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    raw.map(|raw| {
        std::env::split_paths(&raw)
            .filter(|p| !p.as_os_str().is_empty())
            .collect()
    })
    .unwrap_or_default()
}

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
            gem_path: split_path_list(std::env::var_os("GEM_PATH")),
            bundle_path: var("BUNDLE_PATH"),
            bundle_gemfile: var("BUNDLE_GEMFILE"),
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
    /// `require "json"` leads nowhere without this. Held once rather than per gem, because one
    /// directory holds all forty of them and attaching it to each would walk it forty times.
    pub ruby_lib: Vec<PathBuf>,
    pub ruby_version: Option<Resolved>,
    /// Locked gems with no directory on disk — usually default gems that ship inside Ruby
    /// itself, or a platform-specific gem this machine never installed.
    pub unresolved: Vec<String>,
    /// `.gem_rbs_collection/` under the workspace root, when `rbs collection install` has been
    /// run. The community's curated signatures for gems that ship none of their own — including
    /// Rails — and therefore the one directory here that could type a framework with no framework
    /// knowledge in this crate at all.
    ///
    /// `None` is the common case and, on the one real application this project measures against,
    /// the only case. It is found here rather than by the workspace walk because that walk skips
    /// hidden directories, and because these are somebody else's signatures: the same reason a
    /// vendored bundle is not the user's own code.
    pub rbs_collection: Option<PathBuf>,
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

    /// Every directory of RBS the bundle already has on disk, in gem order, curated last.
    ///
    /// **Deliberately not load paths.** `require "widget"` never resolves against `sig/`, so
    /// putting these on [`Gem::load_paths`] would make go-to-definition on a `require` land on a
    /// signature — and `load_paths` is what `require` resolution reads. They are a second list
    /// because they answer a second question.
    ///
    /// `sig/` is rbs's own convention for a gem's signatures and is checked per gem rather than
    /// stored, one `is_dir` on a list this pass then spends seconds indexing. **Measured over
    /// lobsters' 180 resolved gems: 9 ship `sig/`, 52 files and 306 KiB** — a twentieth of the
    /// bundle, and none of them a gem an application writes chains through. The item is worth its
    /// walk because the walk is one stat per gem, not because the corpus flatters it.
    #[must_use]
    pub fn signature_paths(&self) -> Vec<PathBuf> {
        self.gems
            .iter()
            .map(|gem| gem.path.join(SIGNATURE_DIR))
            .filter(|path| path.is_dir())
            .chain(self.rbs_collection.clone())
            .collect()
    }

    /// Every gem's `app/`, in gem order — the half of a Rails engine that is not on its load
    /// path.
    ///
    /// **Deliberately not load paths, for exactly [`Gems::signature_paths`]' reason.**
    /// `require "active_storage"` resolves to `lib/active_storage.rb` and must never resolve to
    /// something under `app/`, and `load_paths` is the list `require` resolution reads. A third
    /// list is what keeps "where a constant is defined" and "what a `require` names" from being
    /// the same question.
    ///
    /// An engine ships its models, mailers, jobs and controllers under `app/` and declares
    /// `require_paths = ["lib"]` all the same — checked in the serialised gemspecs of
    /// `activestorage`, `actionmailbox`, `devise`, `solid_queue`, `turbo-rails` and
    /// `activeadmin` — so without this list `ActiveStorage::Blob` is not a constant this crate
    /// has ever seen, while `ActiveStorage::Service` in the file next door answers. That gap
    /// measured it: **417 files under `app/` against 24,425 under `lib/`** across the 479 gems
    /// installed for one Ruby, of which 18 are engines.
    ///
    /// `config/` is here for exactly one file, and contributes no constant at all. Across the
    /// 479 gems installed for one Ruby there are **9 distinct `.rb` files** under any `config/` —
    /// seven `routes.rb` and two `importmap.rb` — and **none of them defines a class or a
    /// module**. It is walked because an engine's `config/routes.rb` may name helpers that really
    /// are the *host application's*: four of the seven engines that ship one open with
    /// `Rails.application.routes.draw`, and three — blazer, pghero and mission_control-jobs —
    /// draw into their own `Engine.routes`, whose helpers are reached as `blazer.queries_path`
    /// after a `mount` and are not the application's at all. Telling those two apart is
    /// [`crate::workspace::rails::Whose`]'s job, not this walk's.
    ///
    /// Two `is_dir` per gem, on the same argument `sig/` gets: a stat per gem is not a cost, and
    /// the walk that follows is over the gems that really are engines.
    #[must_use]
    pub fn engine_paths(&self) -> Vec<PathBuf> {
        self.gems
            .iter()
            .flat_map(|gem| ENGINE_DIRS.map(|dir| gem.path.join(dir)))
            .filter(|path| path.is_dir())
            .collect()
    }
}

/// Where a Rails engine keeps the Ruby that is not on its load path.
///
/// `app/` for the models, mailers, jobs and controllers; `config/` for the routes file, and for
/// nothing else — see [`Gems::engine_paths`].
const ENGINE_DIRS: [&str; 2] = ["app", "config"];

/// Where a gem keeps the signatures it ships, by rbs's own convention.
const SIGNATURE_DIR: &str = "sig";

/// Where `rbs collection install` writes the curated signatures for a project's gems.
///
/// The path is configurable in `rbs_collection.yaml`, which is YAML this crate has no parser for
/// and no dependency to gain one. The default is honoured and a moved collection is not, which is
/// the same trade `[gems] paths` exists to unstick.
const RBS_COLLECTION_DIR: &str = ".gem_rbs_collection";

/// The gem roots on this machine, without cataloguing what is inside them.
///
/// `discover` needs the roots *and* the catalogue, which is the expensive half. `rbs::discover`
/// needs only the roots, and needs them even when `[gems] enabled = false` — where Ruby is
/// installed and whether the project's bundle should be indexed are different questions.
#[must_use]
pub fn roots(workspace_root: &Path, config: &GemsConfig, env: &Env) -> Vec<PathBuf> {
    let lockfile = read_lockfile(workspace_root, env);
    let version = ruby_version::resolve(
        workspace_root,
        config.ruby_version.as_deref(),
        lockfile.as_ref().map(|(_, lockfile)| lockfile),
        env.home.as_deref(),
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

    let lockfile = read_lockfile(workspace_root, env);

    // The same four arguments as `roots`, and they have to stay the same four: these two
    // resolve independently, and a disagreement between them means `rbs::discover` searches one
    // Ruby's tree while the gem index searches another's.
    gems.ruby_version = ruby_version::resolve(
        workspace_root,
        config.ruby_version.as_deref(),
        lockfile.as_ref().map(|(_, lockfile)| lockfile),
        env.home.as_deref(),
    );

    let version = gems.ruby_version.as_ref().map(|it| it.version.as_str());
    gems.roots = gem_roots(workspace_root, config, env, version);
    // Before the lockfile check below, and cheap either way: one stat. A project can have run
    // `rbs collection install` and this costs nothing at all when it has not.
    gems.rbs_collection =
        Some(workspace_root.join(RBS_COLLECTION_DIR)).filter(|path| path.is_dir());
    if config.default_gems {
        gems.ruby_lib = ruby_lib_dirs(&gems.roots, version);
        // The silent cliff. `ruby_lib_dirs` refuses to guess a Ruby, and it is right to —
        // guessing once put Apple's vestigial 2.6 stdlib into the graph and answered
        // `"hello".u` with `unspace`. But refusing costs the whole of Ruby's own library, 727
        // files on a 3.4 install, and doing so in silence is worse: `require "json"`
        // answered `null`, `JSON.parse` hovered as nothing, and the only trace anywhere was a
        // `DEBUG` line about the *bundle*, which is not what went missing.
        if gems.ruby_lib.is_empty() {
            gems.problems.push(match version {
                None => messages::no_ruby_version(),
                Some(version) => messages::ruby_library_missing(version),
            });
        }
    }

    // Ruby's own library is found before this point deliberately. A project with no bundle
    // still calls `JSON.parse` and still writes `require "forwardable"`, and the stdlib is not
    // something Bundler grants it.
    let Some((lockfile_path, lockfile)) = lockfile else {
        // Not a problem worth showing the user: plenty of Ruby projects have no bundle. It
        // deliberately no longer claims Ruby's own library got indexed — whether it did is the
        // question above, and answering it here is what hid finding F for a whole release.
        tracing::debug!(
            "no Gemfile.lock under {}: no bundle to index; {} Ruby library path(s)",
            workspace_root.display(),
            gems.ruby_lib.len()
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
        gems.problems.push(messages::bundle_mostly_missing(
            &lockfile_path,
            found,
            expected,
            gems.roots.len(),
            gems.ruby_version.as_ref().map(|it| it.version.as_str()),
        ));
    }

    tracing::info!(
        "resolved {}/{} locked gems against {} gem root(s); ruby {}",
        gems.gems.len(),
        seen.len() + gems.unresolved.len(),
        gems.roots.len(),
        gems.ruby_version.as_ref().map_or_else(
            || "unknown".to_owned(),
            |it| format!("{} (from {})", it.version, it.source)
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
fn read_lockfile(root: &Path, env: &Env) -> Option<(PathBuf, Lockfile)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(gemfile) = env.bundle_gemfile.as_deref() {
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

/// Every `.rb` and `.rbs` file under `paths`.
///
/// One walk for both lists, because a signature directory and a load path differ in where they
/// are and not in how they are read: `analysis::Analysis::index_edited_signatures` sorts the two
/// extensions apart on the batch, and it is the only place that rule may live — a file indexed
/// differently depending on which walk found it is worse than one that is not filtered at all.
/// `.rbs` is taken under a load path too, for the handful of gems that put signatures beside the
/// code rather than in `sig/`. Measured over lobsters' bundle: zero, and it costs a comparison.
///
/// Deliberately not the `ignore` crate's default filters: a gem is not a git checkout we should
/// be second-guessing, and for a git source the repository's own `.gitignore` can hide generated
/// files that are genuinely part of the shipped gem. Hidden directories are skipped because no
/// gem puts its library code in one — `.gem_rbs_collection/` is hidden and is reached as a path
/// in its own right, never by descending into it.
#[must_use]
pub fn source_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        let mut walker = ignore::WalkBuilder::new(path);
        walker
            .standard_filters(false)
            .hidden(true)
            .follow_links(false);
        for entry in walker.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "rb" || ext == "rbs")
            {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

#[cfg_attr(coverage_nightly, coverage(off))]
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
        // library beside it. Resolution therefore succeeds and finds nothing, so without this
        // `require "json"` leads nowhere.
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
    fn bundle_gemfile_names_the_lockfile_beside_it() {
        // Bundler appends `.lock` to whatever `BUNDLE_GEMFILE` names — except `gems.rb`, whose
        // lockfile is `gems.locked`. Both come through `Env` rather than `std::env`, which is
        // the only reason either can be tested without mutating the process.
        for (gemfile, lock) in [
            ("Gemfile.ci", "Gemfile.ci.lock"),
            ("gems.rb", "gems.locked"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            let root = dir.path().join("root");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(project.join(lock), RAILS_LOCK).unwrap();
            // The default name is present and holds something else, so a pass would be an
            // accident if `BUNDLE_GEMFILE` were ignored.
            std::fs::write(
                project.join("Gemfile.lock"),
                "GEM\n  remote: https://rubygems.org/\n  specs:\n    sinatra (4.0.0)\n",
            )
            .unwrap();
            install(&root, "rails-8.1.3", &["lib"]);

            let env = Env {
                gem_home: Some(root.clone()),
                bundle_gemfile: Some(PathBuf::from(gemfile)),
                ..Env::default()
            };
            let gems = discover(&project, &GemsConfig::default(), &env);
            let names: Vec<&str> = gems.gems.iter().map(|gem| gem.name.as_str()).collect();
            assert_eq!(names, vec!["rails"], "BUNDLE_GEMFILE={gemfile}");
        }
    }

    #[test]
    fn a_lockfile_that_names_a_gem_twice_resolves_it_once() {
        // A lockfile resolved for several platforms lists one spec per platform, and a `PATH`
        // source can override a `GEM` entry. Bundler applies the first that works; so do we.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let root = dir.path().join("root");
        lockfile(
            &project,
            "GEM\n  remote: https://rubygems.org/\n  specs:\n\
             \x20   nokogiri (1.18.8-arm64-darwin)\n\
             \x20   nokogiri (1.18.8-x86_64-linux)\n",
        );
        install(&root, "nokogiri-1.18.8-arm64-darwin", &["lib"]);

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        // The one that is not installed is not reported: it is a platform this machine will
        // never have, and listing it would bury the cases that matter.
        assert!(gems.unresolved.is_empty(), "{:?}", gems.unresolved);
    }

    #[test]
    fn a_path_source_inside_the_workspace_is_left_to_workspace_discovery() {
        // A `PATH` gem is the user's own code. Indexing it here would bypass the workspace's
        // own exclude globs — and index it twice.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("engines/billing/lib")).unwrap();
        std::fs::write(
            project.join("engines/billing/lib/billing.rb"),
            "module Billing; end\n",
        )
        .unwrap();
        // A sibling of the workspace, which discovery will never reach on its own.
        let outside = dir.path().join("outside/vendor/tooling");
        std::fs::create_dir_all(outside.join("lib")).unwrap();
        std::fs::write(outside.join("lib/tooling.rb"), "module Tooling; end\n").unwrap();

        lockfile(
            &project,
            &format!(
                "PATH\n  remote: engines/billing\n  specs:\n    billing (0.1.0)\n\n\
                 PATH\n  remote: {}\n  specs:\n    tooling (0.1.0)\n\n\
                 PATH\n  specs:\n    nowhere (0.1.0)\n\n\
                 PATH\n  remote: engines/absent\n  specs:\n    absent (0.1.0)\n",
                outside.display()
            ),
        );

        let gems = discover(&project, &GemsConfig::default(), &Env::default());
        let names: Vec<&str> = gems.gems.iter().map(|gem| gem.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["tooling"],
            "only the path source outside the workspace is ours to index"
        );
        // `nowhere` has no remote at all and `absent` names a directory that is not there.
        // Neither is a gem we failed to find: one is unparseable and the other is not installed.
        assert_eq!(gems.unresolved, vec!["absent-0.1.0".to_owned()], "{gems:?}");
    }

    #[test]
    fn a_plugin_source_is_never_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let root = dir.path().join("root");
        lockfile(
            &project,
            "PLUGIN SOURCE\n  remote: https://rubygems.org/\n  specs:\n    plug (1.0.0)\n",
        );
        install(&root, "plug-1.0.0", &["lib"]);

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert!(gems.gems.is_empty(), "{gems:?}");
        assert!(gems.unresolved.is_empty(), "{gems:?}");
    }

    #[test]
    fn a_git_source_with_several_gemspecs_resolves_to_the_gem_not_the_checkout() {
        // One checkout can hold several gems, each in a subdirectory. The checkout itself is
        // the answer only when there is no such subdirectory.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let root = dir.path().join("root");
        let sha = "b8ebc2d491016b206e5ac1c41ee71e16ec94dbee";
        lockfile(
            &project,
            &format!(
                "GIT\n  remote: https://github.com/org/monorepo\n  revision: {sha}\n  specs:\n\
                 \x20   inner (1.0.0)\n\
                 \x20   outer (1.0.0)\n"
            ),
        );
        // `gems/` is the marker of a gem root; a checkout-only root would be rejected before
        // `bundler/gems` was ever read.
        std::fs::create_dir_all(root.join("gems")).unwrap();
        let checkout = root.join("bundler/gems/monorepo-b8ebc2d49101");
        std::fs::create_dir_all(checkout.join("inner/lib")).unwrap();
        std::fs::write(checkout.join("inner/lib/inner.rb"), "module Inner; end\n").unwrap();
        std::fs::create_dir_all(checkout.join("lib")).unwrap();
        std::fs::write(checkout.join("lib/outer.rb"), "module Outer; end\n").unwrap();
        // A plain file beside the checkouts: `absorb` has to step over it rather than record it.
        std::fs::write(root.join("bundler/gems/README"), "not a gem\n").unwrap();

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        let paths: Vec<(&str, &Path)> = gems
            .gems
            .iter()
            .map(|gem| (gem.name.as_str(), gem.path.as_path()))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("inner", checkout.join("inner").as_path()),
                ("outer", checkout.as_path()),
            ]
        );
    }

    #[test]
    fn a_gem_home_that_is_not_a_gem_root_is_ignored() {
        // The marker of a gem root is `gems/`. Without the check, a `GEM_HOME` pointing at
        // something that merely exists costs a directory read per gem in the lockfile.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let empty = dir.path().join("not-a-gem-root");
        lockfile(&project, RAILS_LOCK);
        std::fs::create_dir_all(&empty).unwrap();

        let env = Env {
            gem_home: Some(empty),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert!(gems.roots.is_empty(), "{:?}", gems.roots);
        assert_eq!(gems.unresolved, vec!["rails-8.1.3".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn two_spellings_of_one_gem_root_are_indexed_once_and_spelled_as_written() {
        // `/usr/lib/ruby/gems` and the macOS system framework are routinely symlinks to each
        // other. Deduplicating by the canonical path is what stops the second from doubling
        // every gem — but the *stored* path stays as spelled, because canonicalising here would
        // spell a vendored bundle's files differently from every other path in the server.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let real = dir.path().join("real-root");
        let link = dir.path().join("linked-root");
        lockfile(&project, RAILS_LOCK);
        install(&real, "rails-8.1.3", &["lib"]);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let env = Env {
            gem_home: Some(link.clone()),
            gem_path: vec![real.clone()],
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert_eq!(gems.roots, vec![link], "as spelled, and only once");
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
    }

    #[test]
    fn a_gem_root_only_reaches_rubys_library_through_the_exact_install_shape() {
        // `<prefix>/lib/ruby/gems/<abi>` beside `<prefix>/lib/ruby/<abi>` is the whole rule.
        // Each of these breaks one part of it and must yield nothing rather than a directory
        // that happens to be there.
        let dir = tempfile::tempdir().unwrap();
        for (label, gems_root) in [
            // `ruby/` is not under `lib/`.
            ("no lib", "opt/ruby/gems/4.0.0"),
            // The ABI directory does not match the resolved version.
            ("wrong abi", "opt/lib/ruby/gems/3.1.0"),
        ] {
            let project = dir.path().join(label.replace(' ', "-"));
            lockfile(&project, RAILS_LOCK);
            std::fs::write(project.join(".ruby-version"), "4.0.1\n").unwrap();

            let home = dir.path().join(label.replace(' ', "_"));
            let root = home.join(gems_root);
            std::fs::create_dir_all(root.join("gems")).unwrap();
            // The library directory the shape would name, so its absence is not what fails.
            std::fs::create_dir_all(root.parent().unwrap().parent().unwrap().join("4.0.0"))
                .unwrap();

            let env = Env {
                gem_home: Some(root),
                ..Env::default()
            };
            let gems = discover(&project, &GemsConfig::default(), &env);
            assert!(gems.ruby_lib.is_empty(), "{label}: {:?}", gems.ruby_lib);
        }
    }

    #[test]
    fn a_bundle_path_comes_from_the_environment_or_from_either_bundle_config() {
        // Three sources, and the two files are read by hand rather than with a YAML parser:
        // a `.bundle/config` we cannot make sense of has to degrade to "no configured path".
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        lockfile(&project, RAILS_LOCK);

        std::fs::create_dir_all(project.join(".bundle")).unwrap();
        std::fs::write(
            project.join(".bundle/config"),
            // A key we do not read, a `BUNDLE_PATH` with nothing after it, and then the real
            // one — in that order, so an implementation that takes the first line fails.
            "---\nBUNDLE_JOBS: \"4\"\nBUNDLE_PATH: \"\"\nBUNDLE_PATH: \"vendor/bundle\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".bundle")).unwrap();
        std::fs::write(
            home.join(".bundle/config"),
            "---\nBUNDLE_PATH: 'global-bundle'\n",
        )
        .unwrap();

        // Bundler installs into `<path>/ruby/<abi>`, so that — not `<path>` — is the gem root.
        std::fs::write(project.join(".ruby-version"), "4.0.1\n").unwrap();
        install(
            &project.join("vendor/bundle/ruby/4.0.0"),
            "rails-8.1.3",
            &["lib"],
        );
        install(
            &project.join("global-bundle/ruby/4.0.0"),
            "rack-3.1.0",
            &["lib"],
        );

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert!(
            gems.roots
                .contains(&project.join("vendor/bundle/ruby/4.0.0")),
            "the project's own .bundle/config: {:?}",
            gems.roots
        );
        assert!(
            gems.roots
                .contains(&project.join("global-bundle/ruby/4.0.0")),
            "the home .bundle/config: {:?}",
            gems.roots
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
        let project = home.join("project");
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
        let project = home.join("project");
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
        // And it says so. Refusing to guess is right; refusing in silence cost the whole of
        // Ruby's own library for a release, with `require "json"` answering null and nothing
        // anywhere connecting that to a missing `.ruby-version`.
        assert_eq!(gems.problems, vec![messages::no_ruby_version()]);
    }

    #[test]
    fn a_ruby_that_is_not_installed_here_is_named_rather_than_dropped() {
        // The other half of the cliff: the project does say which Ruby it wants, and that Ruby
        // is not on this machine. The loss is identical — no stdlib — but the remedy is not, so
        // the message is not either.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ruby-version"), "3.4.1\n").unwrap();

        let gems = discover(&project, &GemsConfig::default(), &Env::default());

        assert!(gems.ruby_lib.is_empty(), "{:?}", gems.ruby_lib);
        assert_eq!(gems.problems, vec![messages::ruby_library_missing("3.4.1")]);
    }

    #[test]
    fn a_project_that_asked_for_no_default_gems_is_not_told_it_has_none() {
        // A warning nobody can act on is a nag. Turning `gems.default_gems` off *is* the
        // action, so the message the other two tests pin must not survive it — which is also
        // what the messages themselves offer as the way to silence them.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let config = GemsConfig {
            default_gems: false,
            ..GemsConfig::default()
        };
        let gems = discover(&project, &config, &Env::default());

        assert!(gems.problems.is_empty(), "{:?}", gems.problems);
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
        // Found, so nothing is said. The message only exists for the case where it is not.
        assert!(gems.problems.is_empty(), "{:?}", gems.problems);
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

        // The bundle resolved, which is what this test is about. The fixture installs no
        // `lib/ruby/4.0.0` beside the gems, so the stdlib is reported missing and that is the
        // only thing reported.
        assert_eq!(gems.problems, vec![messages::ruby_library_missing("4.0.1")]);
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        assert_eq!(gems.gems[0].name, "rails");
        assert!(gems.gems[0].load_paths[0].ends_with("rails-8.1.3/lib"));
    }

    #[test]
    fn a_ruby_whose_abi_directory_is_missing_still_finds_the_gems_that_are_there() {
        // The stated fallback, which had no fixture: the lockfile asks for 3.4.1, the machine
        // has 3.3.0, and the *gems* under it are still overwhelmingly the right ones. Preferring
        // an ABI directory that is not installed must reorder nothing rather than find nothing.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);
        std::fs::write(project.join(".ruby-version"), "3.4.1\n").unwrap();

        // The interpreter directory matches; the ABI directory below it does not.
        let root = home.join(".asdf/installs/ruby/3.4.1/lib/ruby/gems/3.3.0");
        install(&root, "rails-8.1.3", &["lib"]);

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.problems, vec![messages::ruby_library_missing("3.4.1")]);
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        assert!(gems.gems[0].load_paths[0].ends_with("rails-8.1.3/lib"));
    }

    #[test]
    fn a_gems_own_signatures_are_found_and_are_not_a_load_path() {
        // Two halves, and only both together find anything: `sig/` has to be admitted by
        // extension *and* by directory, since the walk runs over `require_paths` and no gem
        // puts `sig` in one.
        //
        // The second assertion is the one with teeth. `load_paths` is what `require "..."`
        // resolves against, so a `sig/` that leaked into it would make go-to-definition on
        // `require "thing"` land on a signature rather than on the code.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        let project = dir.path().join("project");
        install(&root, "thing-1.0.0", &["lib"]);
        std::fs::create_dir_all(root.join("gems/thing-1.0.0/sig")).unwrap();
        std::fs::write(
            root.join("gems/thing-1.0.0/sig/thing.rbs"),
            "class Thing
  def shout: () -> String
end
",
        )
        .unwrap();
        lockfile(
            &project,
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    thing (1.0.0)\n",
        );

        let env = Env {
            gem_home: Some(root.clone()),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(
            gems.signature_paths(),
            vec![root.join("gems/thing-1.0.0/sig")]
        );
        assert_eq!(gems.load_paths(), vec![root.join("gems/thing-1.0.0/lib")]);
        assert_eq!(
            source_files(&gems.signature_paths()),
            vec![root.join("gems/thing-1.0.0/sig/thing.rbs")]
        );
    }

    #[test]
    fn an_engines_own_directories_are_found_and_are_not_load_paths() {
        // The same two halves as `sig/` above. An engine declares
        // `require_paths = ["lib"]` — every one of the six checked does — so its models are
        // outside the walk, and the fix is a third list rather than a fourth require path.
        //
        // The second assertion is again the one with teeth, and here it is sharper than it was
        // for `sig/`: this gem ships `app/thing.rb` *and* `lib/thing.rb`, so an `app/` that
        // leaked into `load_paths` would silently change what `require "thing"` means rather
        // than merely pointing it somewhere odd.
        //
        // `config/` is on the list for exactly one file — the routes file — and for no constant:
        // nothing under any installed gem's `config/` defines a class or a module.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        let project = dir.path().join("project");
        install(&root, "thing-1.0.0", &["lib"]);
        std::fs::create_dir_all(root.join("gems/thing-1.0.0/app/models/thing")).unwrap();
        std::fs::write(
            root.join("gems/thing-1.0.0/app/thing.rb"),
            "class Thing; end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("gems/thing-1.0.0/app/models/thing/blob.rb"),
            "class Thing::Blob; end\n",
        )
        .unwrap();
        // The directory that is deliberately not walked: an engine's routes are the engine's.
        std::fs::create_dir_all(root.join("gems/thing-1.0.0/config")).unwrap();
        std::fs::write(
            root.join("gems/thing-1.0.0/config/routes.rb"),
            "Rails.application.routes.draw do\n  resources :blobs\nend\n",
        )
        .unwrap();
        lockfile(
            &project,
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    thing (1.0.0)\n",
        );

        let env = Env {
            gem_home: Some(root.clone()),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(
            gems.engine_paths(),
            vec![
                root.join("gems/thing-1.0.0/app"),
                root.join("gems/thing-1.0.0/config"),
            ]
        );
        assert_eq!(gems.load_paths(), vec![root.join("gems/thing-1.0.0/lib")]);
        assert_eq!(
            source_files(&gems.engine_paths()),
            vec![
                root.join("gems/thing-1.0.0/app/models/thing/blob.rb"),
                root.join("gems/thing-1.0.0/app/thing.rb"),
                root.join("gems/thing-1.0.0/config/routes.rb"),
            ]
        );
        // And `config/` is on the engine list rather than the load path, for the same reason
        // `app/` is: `require "thing"` must not start meaning `config/thing.rb` either.
        assert!(
            !gems
                .load_paths()
                .iter()
                .any(|path| path.ends_with("config") || path.ends_with("app")),
            "neither engine directory is a load path"
        );
    }

    #[test]
    fn a_gem_that_is_not_an_engine_contributes_no_engine_path() {
        // 18 of 479 installed gems are engines, so this is the common case by an order of
        // magnitude and it has to cost the one stat `signature_paths` costs, not a walk.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        let project = dir.path().join("project");
        install(&root, "thing-1.0.0", &["lib"]);
        lockfile(
            &project,
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    thing (1.0.0)\n",
        );

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert!(gems.engine_paths().is_empty(), "{gems:?}");
    }

    #[test]
    fn a_gem_without_signatures_contributes_no_signature_path() {
        // Nine gems in one hundred and eighty, so this is the common case and it has to cost a
        // stat rather than a walk of a directory that is not there.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        let project = dir.path().join("project");
        install(&root, "thing-1.0.0", &["lib"]);
        lockfile(
            &project,
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    thing (1.0.0)\n",
        );

        let env = Env {
            gem_home: Some(root),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        assert!(gems.signature_paths().is_empty(), "{gems:?}");
    }

    #[test]
    fn the_curated_collection_is_found_when_present_and_is_nothing_when_absent() {
        // lobsters has none, and one application is not a survey — so what is
        // pinned here is that the directory is read when it is there and costs one stat when it
        // is not, and no claim at all about how often that is.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        lockfile(&project, "GEM\n  specs:\n\nBUNDLED WITH\n   2.6.2\n");

        let absent = discover(&project, &GemsConfig::default(), &Env::default());
        assert_eq!(absent.rbs_collection, None);
        assert!(absent.signature_paths().is_empty());

        let collection = project.join(".gem_rbs_collection/activesupport/8.0");
        std::fs::create_dir_all(&collection).unwrap();
        std::fs::write(
            collection.join("activesupport.rbs"),
            "class Object
  def blank?: () -> bool
end
",
        )
        .unwrap();

        let present = discover(&project, &GemsConfig::default(), &Env::default());
        assert_eq!(
            present.rbs_collection,
            Some(project.join(".gem_rbs_collection"))
        );
        // Last in the list, after every gem's own — a curated signature is the fallback for a
        // gem that ships none, not a replacement for one that does.
        assert_eq!(
            present.signature_paths(),
            vec![project.join(".gem_rbs_collection")]
        );
        assert_eq!(
            source_files(&present.signature_paths()),
            vec![collection.join("activesupport.rbs")],
            "the walk reaches into a hidden directory it was handed, and never descends into one"
        );
    }

    #[test]
    fn a_walked_directory_yields_ruby_and_signatures_and_nothing_else() {
        // A gem ships its README, its licence and often a compiled `.bundle` beside its code.
        // Handing any of them to the indexer is a parse error per file and no declarations.
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(lib.join("thing")).unwrap();
        std::fs::write(lib.join("thing.rb"), "class Thing; end\n").unwrap();
        std::fs::write(lib.join("thing/version.rb"), "").unwrap();
        // A gem that puts its signatures beside the code rather than in `sig/`. Zero of
        // lobsters' 180, and taking them costs one comparison.
        std::fs::write(lib.join("thing.rbs"), "class Thing\nend\n").unwrap();
        std::fs::write(lib.join("README.md"), "# thing\n").unwrap();
        std::fs::write(lib.join("thing.bundle"), "").unwrap();
        std::fs::write(lib.join("noextension"), "").unwrap();

        let files = source_files(std::slice::from_ref(&lib));

        assert_eq!(
            files,
            // Sorted as paths, so the `thing/` directory comes before `thing.rb` itself.
            vec![
                lib.join("thing/version.rb"),
                lib.join("thing.rb"),
                lib.join("thing.rbs"),
            ],
            "only `.rb` and `.rbs`"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_ruby_library_that_cannot_be_listed_costs_only_its_platform_directory() {
        // The library directory is checked with `is_dir` and then listed, and the two are
        // different questions: a Homebrew or system Ruby installed under another user leaves a
        // directory that exists and cannot be read. The platform directory is the half that
        // needs the listing — the library itself is a path we already have — so losing the
        // listing must not lose `require "json"` as well.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        lockfile(&project, RAILS_LOCK);
        std::fs::write(project.join(".ruby-version"), "4.0.1\n").unwrap();

        let ruby = home.join(".asdf/installs/ruby/4.0.1/lib/ruby");
        install(&ruby.join("gems/4.0.0"), "rails-8.1.3", &["lib"]);
        let library = ruby.join("4.0.0");
        std::fs::create_dir_all(library.join("arm64-darwin25")).unwrap();
        std::fs::set_permissions(&library, std::fs::Permissions::from_mode(0o000)).unwrap();

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);

        // Restored before any assertion, so a failure does not leave an unremovable tempdir.
        std::fs::set_permissions(&library, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            gems.ruby_lib,
            vec![library],
            "the library itself is still a load path; only its platform directory is lost"
        );
        assert_eq!(gems.gems.len(), 1, "{gems:?}");
    }

    #[test]
    fn a_path_list_is_split_the_way_the_platform_separates_one() {
        use std::path::MAIN_SEPARATOR;

        // `GEM_PATH` is the one multi-value variable read here, and an empty entry in it is not
        // "no path" — it is the *current directory*, which would put whatever the editor was
        // launched from on the gem search path.
        let sep = if cfg!(windows) { ";" } else { ":" };
        let joined = format!("{}a{sep}{sep}{}b{sep}", MAIN_SEPARATOR, MAIN_SEPARATOR);

        assert_eq!(
            split_path_list(Some(joined.into())),
            vec![
                PathBuf::from(format!("{MAIN_SEPARATOR}a")),
                PathBuf::from(format!("{MAIN_SEPARATOR}b")),
            ]
        );
        assert!(split_path_list(None).is_empty(), "unset is no paths");
        assert!(split_path_list(Some("".into())).is_empty(), "set but empty");
    }

    #[test]
    fn an_abi_is_the_version_with_its_patch_zeroed_and_nothing_else() {
        // Used only to *prefer* a globbed directory, so a version it cannot take apart has to
        // come back unchanged rather than as a guess: preferring `head.0` over `head` would
        // reorder the installs on a machine running a development build of Ruby.
        assert_eq!(abi_of("4.0.1"), "4.0.0");
        assert_eq!(abi_of("3.4"), "3.4.0");
        assert_eq!(abi_of("head"), "head", "nothing to zero");
        assert_eq!(abi_of(""), "");
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
        assert_eq!(source_files(&gems.gems[0].load_paths).len(), 1);
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
        let home = dir.path().join("empty-home");
        let project = home.join("project");
        lockfile(&project, RAILS_LOCK);

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let gems = discover(&project, &GemsConfig::default(), &env);
        assert!(gems.gems.is_empty());
        // Whether a system-wide Ruby happens to exist on the machine running this test is not
        // the point; resolving *none* of the lockfile is, and that is what gets reported.
        assert!(
            gems.problems.iter().any(|p| p.contains("gems.paths")),
            "{:?}",
            gems.problems
        );
    }

    #[test]
    fn the_resolution_summary_says_what_was_found_and_which_ruby_it_used() {
        // Two lines nobody had asserted, and between them they are the whole answer to "why
        // does go-to-definition not work in gems?". The `info!` says how much of the lockfile
        // resolved and against which Ruby; the `debug!` below it names the gems that did not.
        // A user reads these before they read anything else, and a summary that quietly stops
        // being true is worse than no summary.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        lockfile(
            &project,
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    rails (8.1.3)\n    \
             nokogiri (1.18.0)\n",
        );
        std::fs::write(project.join(".ruby-version"), "3.4.1\n").unwrap();
        install(&home.join(".gem/ruby/3.4.0"), "rails-8.1.3", &["lib"]);

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let (gems, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            discover(&project, &GemsConfig::default(), &env)
        });

        assert_eq!(gems.gems.len(), 1, "{gems:?}");
        assert!(
            logged.contains("resolved 1/2 locked gems against 1 gem root(s)"),
            "how much of the lockfile resolved: {logged}"
        );
        assert!(
            logged.contains(&format!(
                "ruby 3.4.1 (from {})",
                project.join(".ruby-version").display()
            )),
            "which Ruby, and which file said so — after the walk the kind of file no longer \
             names one: {logged}"
        );
        // And which gem is missing, by name — the difference between "run bundle install" and
        // "this gem is not installed for this platform".
        assert!(
            logged.contains("1 locked gems are not installed here: nokogiri-1.18.0"),
            "which gems are missing: {logged}"
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
        // Nothing about the *bundle*. The fixture points at no Ruby at all, which is a
        // separate loss with a separate message.
        assert_eq!(gems.problems, vec![messages::no_ruby_version()]);
    }

    #[test]
    fn a_project_with_no_lockfile_is_not_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let gems = discover(dir.path(), &GemsConfig::default(), &Env::default());
        assert!(gems.gems.is_empty());
        // Having no bundle is ordinary and says nothing. Having no Ruby is not the same thing
        // and is the only entry here.
        assert_eq!(gems.problems, vec![messages::no_ruby_version()]);
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
