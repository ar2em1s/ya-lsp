//! Which Ruby the project uses — decided without running `ruby`.
//!
//! Every gem root on disk is keyed by a Ruby version, so picking the wrong one means finding no
//! gems at all (or, worse, a different project's). We cannot ask the interpreter, so we read the
//! same files a version manager reads, in the order a shell would apply them.
//!
//! Nothing here touches a gem directory: this module answers "which version", and
//! [`super::gems`] answers "where does that version keep its gems". Splitting them keeps the
//! version rules testable without building a filesystem fixture for every version manager.

use std::path::Path;

use super::bundler::Lockfile;

/// Where the answer came from. Surfaced in logs, because "ya-lsp found no gems" is otherwise
/// impossible to debug: the user needs to know which file we believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `[gems].ruby_version` in `ya-lsp.toml`.
    Config,
    /// `.ruby-version` — what rbenv, chruby, and RVM read.
    RubyVersionFile,
    /// `.tool-versions` — asdf and mise.
    ToolVersions,
    /// The lockfile's own `RUBY VERSION` section.
    GemfileLock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub version: String,
    pub source: Source,
}

/// Resolve the target Ruby version, or `None` when the project says nothing about it — in which
/// case the caller falls back to the newest Ruby it can find installed.
///
/// The order is deliberate: an explicit override beats a version-manager file, a version-manager
/// file beats the lockfile. `RUBY VERSION` in `Gemfile.lock` records what the *last person to run
/// bundler* used, which is frequently not what this machine has.
#[must_use]
pub fn resolve(
    root: &Path,
    configured: Option<&str>,
    lockfile: Option<&Lockfile>,
) -> Option<Resolved> {
    if let Some(version) = configured.and_then(normalize) {
        return Some(Resolved {
            version,
            source: Source::Config,
        });
    }

    if let Some(version) = read(root, ".ruby-version")
        .as_deref()
        .and_then(from_ruby_version_file)
    {
        return Some(Resolved {
            version,
            source: Source::RubyVersionFile,
        });
    }

    if let Some(version) = read(root, ".tool-versions")
        .as_deref()
        .and_then(from_tool_versions)
    {
        return Some(Resolved {
            version,
            source: Source::ToolVersions,
        });
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

fn read(root: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(root.join(name)).ok()
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

    if candidate.is_empty() {
        return None;
    }
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

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    fn write(root: &Path, name: &str, contents: &str) {
        std::fs::write(root.join(name), contents).unwrap();
    }

    #[test]
    fn the_config_override_beats_every_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".ruby-version", "3.1.0\n");
        write(dir.path(), ".tool-versions", "ruby 3.2.0\n");

        let resolved = resolve(dir.path(), Some("3.4.1"), None).unwrap();
        assert_eq!(resolved.version, "3.4.1");
        assert_eq!(resolved.source, Source::Config);
    }

    #[test]
    fn a_ruby_version_file_beats_tool_versions_and_the_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".ruby-version", "ruby-3.3.6\n");
        write(dir.path(), ".tool-versions", "ruby 3.2.0\n");
        let lockfile = super::super::bundler::parse("RUBY VERSION\n   ruby 3.1.0p1\n");

        let resolved = resolve(dir.path(), None, Some(&lockfile)).unwrap();
        // The engine prefix comes off: no install directory is ever named `ruby-3.3.6`
        // *inside* a version manager's per-version tree.
        assert_eq!(resolved.version, "3.3.6");
        assert_eq!(resolved.source, Source::RubyVersionFile);
    }

    #[test]
    fn tool_versions_is_read_the_way_asdf_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".tool-versions",
            "# a comment\nnodejs 25.9.0\nruby 4.0.5 3.4.1\n",
        );

        let resolved = resolve(dir.path(), None, None).unwrap();
        assert_eq!(resolved.version, "4.0.5");
        assert_eq!(resolved.source, Source::ToolVersions);
    }

    #[test]
    fn a_tool_versions_entry_with_no_usable_version_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        // `system` and `ref:` name no directory we could ever find. Guessing would send the
        // whole search to the wrong place, silently.
        write(dir.path(), ".tool-versions", "ruby system\n");
        let lockfile = super::super::bundler::parse("RUBY VERSION\n   ruby 3.1.0p1\n");

        let resolved = resolve(dir.path(), None, Some(&lockfile)).unwrap();
        assert_eq!(resolved.version, "3.1.0");
        assert_eq!(resolved.source, Source::GemfileLock);
    }

    #[test]
    fn a_project_that_says_nothing_gets_no_answer_rather_than_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve(dir.path(), None, None), None);
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
}
