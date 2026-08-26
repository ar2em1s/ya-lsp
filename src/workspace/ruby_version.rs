//! Which Ruby the project uses — decided without running `ruby`.
//!
//! Every gem root on disk is keyed by a Ruby version, so picking the wrong one means finding no
//! gems at all (or, worse, a different project's). We cannot ask the interpreter, so we read the
//! same files a version manager reads, in the order a shell would apply them — and in the same
//! directories: from the workspace root upwards. Every one of these tools walks the ancestors,
//! so a version file in a parent directory is not an oversight, it is the documented way to say
//! "everything under here uses this Ruby".
//!
//! Nothing here touches a gem directory: this module answers "which version", and
//! [`super::gems`] answers "where does that version keep its gems". Splitting them keeps the
//! version rules testable without building a filesystem fixture for every version manager.

use std::path::{Path, PathBuf};

use super::bundler::Lockfile;

/// What rbenv, chruby and RVM read.
const RUBY_VERSION_FILE: &str = ".ruby-version";
/// What asdf and mise read.
const TOOL_VERSIONS_FILE: &str = ".tool-versions";

/// Where the answer came from. Surfaced in logs, because "ya-lsp found no gems" is otherwise
/// impossible to debug: the user needs to know which file we believed.
///
/// The file variants carry the path and not only the kind. Naming the kind answered the question
/// only while there was one directory the file could be in; over the ancestor chain there are as
/// many candidates as the chain is deep, and an answer that surprises someone is debuggable only
/// if they are told which file won. asdf's own `current` prints the full path for that reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `gems.ruby_version` in `ya-lsp.toml`.
    Config,
    /// A `.ruby-version`, in the workspace root or above it.
    RubyVersionFile(PathBuf),
    /// A `.tool-versions`, in the workspace root or above it.
    ToolVersions(PathBuf),
    /// The lockfile's own `RUBY VERSION` section.
    GemfileLock,
}

impl std::fmt::Display for Source {
    /// What the startup line says after `from`: a path wherever there is one, which is the whole
    /// reason a source is recorded at all.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config => f.write_str("gems.ruby_version in ya-lsp.toml"),
            Self::RubyVersionFile(path) | Self::ToolVersions(path) => {
                write!(f, "{}", path.display())
            }
            Self::GemfileLock => f.write_str("RUBY VERSION in Gemfile.lock"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub version: String,
    pub source: Source,
}

/// Resolve the target Ruby version, or `None` when neither the project nor any directory above
/// it says which one it is.
///
/// The order is deliberate, and it is the order a shell applies:
///
/// 1. `gems.ruby_version` in `ya-lsp.toml` — an explicit override beats everything.
/// 2. A version-manager file, nearest directory first, `.ruby-version` before `.tool-versions`
///    within one directory.
/// 3. `RUBY VERSION` in `Gemfile.lock`.
///
/// The lockfile ranks below the *whole* chain rather than below the workspace root alone. The
/// question here is which interpreter will run this code, not which project this is: a
/// version-manager file is the live answer to that and `RUBY VERSION` is a historical one — it
/// records what the last person to run bundler used, which is frequently not what this machine
/// has. ya-lsp never runs `ruby`, so the one thing it cannot afford is to disagree with the
/// `ruby` the user's own shell resolves in that directory.
///
/// `ceiling` is the last directory the walk reads — `$HOME` in production, and read rather than
/// merely stopped at, because `~/.ruby-version` is an ordinary thing to write. It is a parameter
/// and not an environment lookup for the same reason [`super::gems::Env`] is one: a fixture has
/// to be able to bound the walk to its own temp directory. `None` bounds it to `root` alone —
/// with no ceiling to stop at, an unbounded walk would read files that belong to nobody in
/// particular, and finding no version is a reported outcome while finding a wrong one is silent.
///
/// `None` is not "use the default". Without a version [`super::gems`] indexes no part of Ruby's
/// own library — that is what [`crate::messages::no_ruby_version`] reports. Gem *roots* do keep
/// looking and take the newest version installed; the stdlib does not, and no comment here
/// should imply otherwise again.
#[must_use]
pub fn resolve(
    root: &Path,
    configured: Option<&str>,
    lockfile: Option<&Lockfile>,
    ceiling: Option<&Path>,
) -> Option<Resolved> {
    if let Some(version) = configured.and_then(normalize) {
        return Some(Resolved {
            version,
            source: Source::Config,
        });
    }

    for directory in chain(root, ceiling) {
        if let Some(resolved) = from_directory(directory) {
            return Some(resolved);
        }
    }

    if let Some(version) = lockfile
        .and_then(|lockfile| lockfile.ruby_version.as_deref())
        .and_then(normalize)
    {
        return Some(Resolved {
            version,
            source: Source::GemfileLock,
        });
    }

    None
}

/// Every directory the version files are read from, nearest first: `root`, then each ancestor,
/// stopping *after* `ceiling` or at the filesystem root — whichever it reaches first.
///
/// Nothing is canonicalised. Two spellings of one directory make the walk miss the ceiling and
/// run on to the filesystem root, which reads a few directories nobody wrote a version file in;
/// canonicalising the workspace root instead would fork every URI on a machine whose temp
/// directory is a symlink, which is `workspace::uri`'s whole subject.
fn chain<'a>(root: &'a Path, ceiling: Option<&'a Path>) -> impl Iterator<Item = &'a Path> {
    let mut reached = false;
    root.ancestors().take_while(move |directory| {
        let keep_going = !reached;
        reached = ceiling.is_none_or(|ceiling| ceiling == *directory);
        keep_going
    })
}

/// The version one directory names, if it names one.
///
/// `.ruby-version` before `.tool-versions` is a tie-break *within* a directory and nothing more:
/// across the chain the nearer directory wins whichever kind of file it holds. Kind-major would
/// let a `~/.ruby-version` the user forgot about outrank a `.tool-versions` they wrote in the
/// project, and every version manager orders it the other way.
fn from_directory(directory: &Path) -> Option<Resolved> {
    let path = directory.join(RUBY_VERSION_FILE);
    if let Some(version) = read(&path).as_deref().and_then(from_ruby_version_file) {
        return Some(Resolved {
            version,
            source: Source::RubyVersionFile(path),
        });
    }

    let path = directory.join(TOOL_VERSIONS_FILE);
    if let Some(version) = read(&path).as_deref().and_then(from_tool_versions) {
        return Some(Resolved {
            version,
            source: Source::ToolVersions(path),
        });
    }

    None
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// `.ruby-version` holds one version, sometimes with an engine prefix (`ruby-3.2.1`,
/// `jruby-9.4.0.0`). Some tools also allow a comment line.
///
/// A non-numeric value (`system`, `ref:master`) is rejected rather than guessed at: there is no
/// directory name we could build from it, and pretending otherwise would silently search the
/// wrong place.
fn from_ruby_version_file(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .and_then(normalize)
}

/// `.tool-versions` lists one tool per line. asdf allows several fallback versions on one line
/// (`ruby 3.3.0 3.2.1`); the first is the one in effect.
fn from_tool_versions(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        if words.next() != Some("ruby") {
            continue;
        }
        // Take the first value that is actually a version. `system`, `ref:master`, and
        // `path:/opt/ruby` are all legal here and none of them names a directory we can find.
        if let Some(version) = words.find_map(normalize) {
            return Some(version);
        }
    }
    None
}

/// Strip an engine prefix and a patchlevel suffix, and reject anything that is not a version.
fn normalize(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // `ruby-3.2.1` / `jruby-9.4.0.0`: drop everything up to the last hyphen that is followed by
    // a digit. Using the *last* one keeps `truffleruby-head-23.0.0` intact.
    let candidate = match raw.rfind('-') {
        Some(index) if raw[index + 1..].starts_with(|c: char| c.is_ascii_digit()) => {
            &raw[index + 1..]
        }
        _ => raw,
    };

    if !candidate.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }

    // `3.2.1p123` — a patchlevel is never part of an install directory's name.
    let candidate = match candidate.split_once('p') {
        Some((version, patchlevel))
            if !patchlevel.is_empty() && patchlevel.chars().all(|c| c.is_ascii_digit()) =>
        {
            version
        }
        _ => candidate,
    };

    // No emptiness check: the digit test above is what guarantees one, and `split_once('p')`
    // cannot take the digit away — the `p` it splits on is never at index 0.
    Some(candidate.to_owned())
}

/// Compare two version strings the way `Gem::Version` orders them, for "newest installed".
///
/// Segments are compared numerically where both sides are numeric and lexically otherwise, so
/// `3.10.0` sorts above `3.9.0` — which a plain string comparison gets backwards — and a
/// prerelease (`3.4.0.rc1`) sorts below its release.
#[must_use]
pub fn compare(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut left = left.split(['.', '-']);
    let mut right = right.split(['.', '-']);

    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            // A shorter version is the release; a longer one continues into a prerelease tag.
            // `3.4.0` beats `3.4.0.rc1`, but `3.4` loses to `3.4.1`.
            (None, Some(segment)) => {
                return if segment.chars().all(|c| c.is_ascii_digit()) {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (Some(segment), None) => {
                return if segment.chars().all(|c| c.is_ascii_digit()) {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (Some(a), Some(b)) => {
                let ordering = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    // A numeric segment outranks an alphabetic one: `1.0` is newer than `1.rc1`.
                    (Ok(_), Err(_)) => Ordering::Greater,
                    (Err(_), Ok(_)) => Ordering::Less,
                    (Err(_), Err(_)) => a.cmp(b),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    fn write(root: &Path, name: &str, contents: &str) {
        std::fs::write(root.join(name), contents).unwrap();
    }

    /// A project three directories under a fixture `$HOME`, with nothing written anywhere.
    ///
    /// The temp directory itself is *above* the ceiling by construction, so anything written
    /// there is a file the walk must never read.
    fn nested() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = home.join("work/team/project");
        std::fs::create_dir_all(&project).unwrap();
        (dir, home, project)
    }

    #[test]
    fn the_config_override_beats_every_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".ruby-version", "3.1.0\n");
        write(dir.path(), ".tool-versions", "ruby 3.2.0\n");

        let resolved = resolve(dir.path(), Some("3.4.1"), None, Some(dir.path())).unwrap();
        assert_eq!(resolved.version, "3.4.1");
        assert_eq!(resolved.source, Source::Config);
    }

    #[test]
    fn a_ruby_version_file_beats_tool_versions_and_the_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".ruby-version", "ruby-3.3.6\n");
        write(dir.path(), ".tool-versions", "ruby 3.2.0\n");
        let lockfile = super::super::bundler::parse("RUBY VERSION\n   ruby 3.1.0p1\n");

        let resolved = resolve(dir.path(), None, Some(&lockfile), Some(dir.path())).unwrap();
        // The engine prefix comes off: no install directory is ever named `ruby-3.3.6`
        // *inside* a version manager's per-version tree.
        assert_eq!(resolved.version, "3.3.6");
        assert_eq!(
            resolved.source,
            Source::RubyVersionFile(dir.path().join(".ruby-version"))
        );
    }

    #[test]
    fn tool_versions_is_read_the_way_asdf_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".tool-versions",
            "# a comment\nnodejs 25.9.0\nruby 4.0.5 3.4.1\n",
        );

        let resolved = resolve(dir.path(), None, None, Some(dir.path())).unwrap();
        assert_eq!(resolved.version, "4.0.5");
        assert_eq!(
            resolved.source,
            Source::ToolVersions(dir.path().join(".tool-versions"))
        );
    }

    #[test]
    fn a_tool_versions_entry_with_no_usable_version_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        // `system` and `ref:` name no directory we could ever find. Guessing would send the
        // whole search to the wrong place, silently.
        write(dir.path(), ".tool-versions", "ruby system\n");
        let lockfile = super::super::bundler::parse("RUBY VERSION\n   ruby 3.1.0p1\n");

        let resolved = resolve(dir.path(), None, Some(&lockfile), Some(dir.path())).unwrap();
        assert_eq!(resolved.version, "3.1.0");
        assert_eq!(resolved.source, Source::GemfileLock);
    }

    #[test]
    fn a_project_that_says_nothing_gets_no_answer_rather_than_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve(dir.path(), None, None, Some(dir.path())), None);
    }

    #[test]
    fn versions_compare_numerically_not_lexically() {
        // The one every naive implementation gets wrong.
        assert_eq!(compare("3.10.0", "3.9.0"), Ordering::Greater);
        assert_eq!(compare("3.4.1", "3.4.1"), Ordering::Equal);
        assert_eq!(compare("3.4", "3.4.1"), Ordering::Less);
        // A prerelease is older than the release it leads to.
        assert_eq!(compare("3.4.0.rc1", "3.4.0"), Ordering::Less);
        assert_eq!(compare("4.0.0", "3.4.0.rc1"), Ordering::Greater);
    }

    #[test]
    fn a_longer_version_is_newer_only_when_it_continues_with_a_number() {
        // Both sides of the same rule, which is what tells `3.4` < `3.4.1` apart from
        // `3.4` > `3.4.rc1`. Every comparison here is also asserted the other way round,
        // because an ordering that is not antisymmetric will sort differently depending on
        // which version the directory listing happened to yield first.
        for (newer, older) in [
            ("3.4.1", "3.4"),
            ("3.4", "3.4.rc1"),
            ("3.4.0", "3.4.0.pre"),
            ("1.0", "1.rc1"),
        ] {
            assert_eq!(
                compare(newer, older),
                Ordering::Greater,
                "{newer} > {older}"
            );
            assert_eq!(compare(older, newer), Ordering::Less, "{older} < {newer}");
        }
    }

    #[test]
    fn two_prereleases_of_one_version_are_ordered_by_their_tags() {
        // Both segments alphabetic, which is the case that decides between two prereleases of
        // the same release — `rbs-4.1.0.pre1` against `rbs-4.1.0.rc1`, where `workspace::rbs`
        // has to pick one and picking the older one replaces `String` with an earlier draft of
        // it. Lexical is not right in general (`rc` after `pre` only by luck of the alphabet)
        // but it is what RubyGems does, and it is at least stable.
        assert_eq!(compare("4.1.0.rc1", "4.1.0.pre1"), Ordering::Greater);
        assert_eq!(compare("4.1.0.pre1", "4.1.0.rc1"), Ordering::Less);
        assert_eq!(compare("4.1.0.rc1", "4.1.0.rc1"), Ordering::Equal);
    }

    #[test]
    fn a_ruby_version_file_may_open_with_blanks_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        // rbenv writes the bare version; other tools allow a comment above it.
        write(
            dir.path(),
            ".ruby-version",
            "\n# set by mise\n  3.3.6  \n3.0.0\n",
        );

        let resolved = resolve(dir.path(), None, None, Some(dir.path())).unwrap();
        assert_eq!(resolved.version, "3.3.6", "the first real line wins");
        assert_eq!(
            resolved.source,
            Source::RubyVersionFile(dir.path().join(".ruby-version"))
        );
    }

    #[test]
    fn a_ruby_version_that_names_no_directory_is_rejected_rather_than_guessed_at() {
        // `ruby-head` is a perfectly ordinary thing to write and there is no `lib/ruby/gems/head`
        // to search. Falling through to the lockfile is the honest answer; inventing a directory
        // name would send the whole gem search somewhere silently wrong.
        let lockfile = super::super::bundler::parse("RUBY VERSION\n   ruby 3.1.0p1\n");
        for unusable in ["ruby-head\n", "system\n", "ref:master\n", "\n\n"] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), ".ruby-version", unusable);
            let resolved = resolve(dir.path(), None, Some(&lockfile), Some(dir.path())).unwrap();
            assert_eq!(
                resolved.source,
                Source::GemfileLock,
                "{unusable:?} should not have resolved"
            );
        }
    }

    #[test]
    fn a_patchlevel_comes_off_only_when_it_is_one() {
        // `3.2.1p123` is how `ruby -v` spells it and no install directory carries the suffix.
        // `3.2.1pXY` is not a patchlevel at all, and `truffleruby-head-23.0.0` keeps its
        // *last* hyphen — dropping at the first would leave `head-23.0.0`.
        for (written, expected) in [
            ("3.2.1p123", "3.2.1"),
            ("3.2.1pXY", "3.2.1pXY"),
            ("3.2.1p", "3.2.1p"),
            ("truffleruby-head-23.0.0", "23.0.0"),
            ("jruby-9.4.0.0", "9.4.0.0"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), ".ruby-version", written);
            let resolved = resolve(dir.path(), None, None, Some(dir.path()))
                .unwrap_or_else(|| panic!("{written} resolved to nothing"));
            assert_eq!(resolved.version, expected, "from {written:?}");
        }
    }

    #[test]
    fn normalize_rejects_what_it_cannot_turn_into_a_directory_name() {
        // Reached through the files above for everything a file can hold; asserted here for
        // the empty string, which every caller filters out before it could arrive.
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("   "), None);
    }

    #[test]
    fn tool_versions_skips_blank_lines_and_comments_before_ruby() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".tool-versions",
            "\n# managed by mise\n\nnodejs 25.9.0\nruby system 3.4.1\n",
        );

        let resolved = resolve(dir.path(), None, None, Some(dir.path())).unwrap();
        // `system` is legal here and names no directory, so the *next* value is taken.
        assert_eq!(resolved.version, "3.4.1");
        assert_eq!(
            resolved.source,
            Source::ToolVersions(dir.path().join(".tool-versions"))
        );
    }

    #[test]
    fn the_walk_stops_at_the_ceiling_or_at_the_filesystem_root() {
        // Both stops, as path arithmetic. No file is read, so the answer cannot depend on what
        // this machine happens to keep in `/` or in the real `$HOME`.
        let walk = |root: &str, ceiling: Option<&str>| {
            let (root, ceiling) = (PathBuf::from(root), ceiling.map(PathBuf::from));
            chain(&root, ceiling.as_deref())
                .map(|directory| directory.display().to_string())
                .collect::<Vec<_>>()
        };

        // The ceiling is read and *then* the walk stops. `~/.ruby-version` is how every one of
        // these tools spells "my default Ruby", so stopping short of it would miss the file
        // most likely to be on the chain at all.
        assert_eq!(
            walk("/home/x/work/app", Some("/home/x")),
            ["/home/x/work/app", "/home/x/work", "/home/x"]
        );
        // A workspace that is not under the ceiling never meets it, and stops at the
        // filesystem root instead — `/srv/app` and `/workspace` are ordinary places to work.
        assert_eq!(walk("/srv/app", Some("/home/x")), ["/srv/app", "/srv", "/"]);
        // No ceiling, no walk. `Env::default()` carries no `$HOME`, so a fixture that has not
        // asked for the walk reads its own directory and nothing else — the same rule that
        // keeps `Env::system_roots` empty by default.
        assert_eq!(walk("/srv/app", None), ["/srv/app"]);
    }

    #[test]
    fn the_nearest_directory_wins_whichever_kind_of_file_it_holds() {
        // Kind-major ordering — every `.ruby-version` above every `.tool-versions` — is right
        // within one directory and wrong across a chain: it would let a file the user forgot
        // about in `~` outrank one they wrote next door. asdf, rbenv and chruby all take the
        // nearest file, and the general rule is the same one: the more specific statement wins.
        let (_dir, home, project) = nested();
        let team = project.parent().unwrap();
        write(&home, ".ruby-version", "3.1.0\n");
        write(team, ".tool-versions", "ruby 3.2.0\n");

        let resolved = resolve(&project, None, None, Some(&home)).unwrap();
        assert_eq!(resolved.version, "3.2.0");
        assert_eq!(
            resolved.source,
            Source::ToolVersions(team.join(".tool-versions"))
        );
    }

    #[test]
    fn an_ancestor_file_outranks_the_lockfile() {
        // The one answer the walk changes rather than adds, so it is pinned on its own rather
        // than left to follow from the order of some `if`s. This project resolved 3.1.0 from
        // its lockfile before the walk existed; the `.tool-versions` two directories up is what
        // `ruby` resolves in that directory today, and `RUBY VERSION` records what the last
        // person to run bundler had.
        let (_dir, home, project) = nested();
        let team = project.parent().unwrap();
        write(team, ".tool-versions", "ruby 3.3.6\n");
        let lockfile = super::super::bundler::parse("RUBY VERSION\n   ruby 3.1.0p1\n");

        let resolved = resolve(&project, None, Some(&lockfile), Some(&home)).unwrap();
        assert_eq!(resolved.version, "3.3.6");
        assert_eq!(
            resolved.source,
            Source::ToolVersions(team.join(".tool-versions"))
        );
    }

    #[test]
    fn the_ceiling_is_read_and_nothing_above_it_is() {
        let (dir, home, project) = nested();
        write(dir.path(), ".ruby-version", "9.9.9\n");

        // Above `$HOME` is nobody's setting for this project — on a real machine that is
        // `/Users`, `/home` and `/`, which no version manager would have written.
        assert_eq!(resolve(&project, None, None, Some(&home)), None);

        write(&home, ".tool-versions", "ruby 4.0.1\n");
        let resolved = resolve(&project, None, None, Some(&home)).unwrap();
        assert_eq!(resolved.version, "4.0.1");
        assert_eq!(
            resolved.source,
            Source::ToolVersions(home.join(".tool-versions"))
        );
    }

    #[test]
    fn a_source_names_the_file_it_believed() {
        // The startup line is where a surprising Ruby gets debugged from, and after the walk
        // "from a .tool-versions" names one of as many files as the chain is deep.
        assert_eq!(
            Source::RubyVersionFile(PathBuf::from("/home/x/.ruby-version")).to_string(),
            "/home/x/.ruby-version"
        );
        assert_eq!(
            Source::ToolVersions(PathBuf::from("/home/x/work/.tool-versions")).to_string(),
            "/home/x/work/.tool-versions"
        );
        // The two that are not a path still say where to go and look.
        assert_eq!(
            Source::Config.to_string(),
            "gems.ruby_version in ya-lsp.toml"
        );
        assert_eq!(
            Source::GemfileLock.to_string(),
            "RUBY VERSION in Gemfile.lock"
        );
    }
}
