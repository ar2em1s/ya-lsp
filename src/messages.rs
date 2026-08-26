//! Every sentence ya-lsp says to a user, in one place.
//!
//! These strings reach the user twice: as a `window/showMessage` warning, and as a
//! `tracing::warn!` line on stderr. Both are plain text — no client renders markdown in a
//! notification — so what is written here is what is read.
//!
//! They lived beside the code that raised them until v0.2.0, and seventeen sites answered the
//! same four questions independently: how to spell a setting, where to break the clause,
//! whether to name a remedy, whether to end with a period. Two of them were the *same string
//! written twice*. The unit of work here is the sentence rather than the feature, so the
//! sentences live together and the rule is written down once.
//!
//! # The rule
//!
//! 1. **A setting is its dotted TOML path**: `gems.max_files`, `index.include`,
//!    `diagnostics.rules`. Never `[gems].max_files`, never `[gems] max_files`. The dotted form
//!    is also valid TOML, so it is something the user can paste.
//! 2. **One colon.** It joins what happened to what follows from it — or, when another
//!    library's error is the explanation, it introduces that error, which is then the last
//!    thing in the message and is passed through exactly as that library wrote it.
//! 3. **A remedy whenever ya-lsp knows one**, as its own sentence, starting with a verb. "Run
//!    bundle install" is the difference between a warning and a nag.
//! 4. **A full stop at the end**, unless the message ends in a foreign error.
//! 5. **The user's words, not ours.** No term that exists only in this codebase's comments —
//!    "gem intelligence is incomplete" says less than "navigation into gems will mostly not
//!    work", and the second is what a user could have written themselves.
//!
//! `messages::tests::every_message_meets_the_rule` enumerates the whole set and checks the
//! mechanical half of that, so a new message cannot be added without meeting it.

use std::{fmt::Display, path::Path};

// ---------------------------------------------------------------------------- ya-lsp.toml

/// The client sent `initializationOptions` that do not fit the config schema.
#[must_use]
pub fn initialization_options_ignored(error: &dyn Display) -> String {
    format!(
        "the client's initializationOptions were not understood, so the defaults are in use \
         instead. Check the ya-lsp settings in your editor: {error}"
    )
}

/// `ya-lsp.toml` exists but is not valid TOML, or names a key that does not exist.
///
/// No remedy sentence: the parser's own error names the line, the column and the valid keys,
/// which is a better remedy than anything that could be written here.
#[must_use]
pub fn config_file_ignored(file_name: &str, error: &dyn Display) -> String {
    format!("{file_name} could not be parsed, so the defaults are in use instead: {error}")
}

/// `ya-lsp.toml` is there and could not be opened — a permission, not a syntax, problem.
#[must_use]
pub fn config_file_unreadable(path: &Path, error: &dyn Display) -> String {
    format!(
        "{} could not be opened, so the defaults are in use instead: {error}",
        path.display()
    )
}

/// One entry of `index.include` or `index.exclude` is not a glob.
///
/// The one string this module was worth building for on its own: it was written out twice,
/// verbatim, in two files, so a fix to either left the other.
#[must_use]
pub fn invalid_glob(field: &str, pattern: &str, error: &dyn Display) -> String {
    format!("{field} has an invalid glob {pattern:?}, which is ignored: {error}")
}

/// `index.include = []`.
#[must_use]
pub fn include_is_empty(default: &[String]) -> String {
    format!(
        "index.include is empty: nothing will be indexed. Add a glob, or remove the key to use \
         the default of {default:?}."
    )
}

/// `index.max_files = 0`.
#[must_use]
pub fn max_files_is_zero(default: usize) -> String {
    format!(
        "index.max_files is 0: nothing will be indexed. Raise it, or remove the key to use the \
         default of {default}."
    )
}

/// `rbs.path` points somewhere that is not an rbs root.
#[must_use]
pub fn rbs_path_has_no_core(path: &Path) -> String {
    format!(
        "rbs.path {} has no core directory: the signatures vendored into ya-lsp are used \
         instead, which may be a different version of Ruby's. Point rbs.path at an unpacked rbs \
         gem, or remove the key.",
        path.display()
    )
}

// ---------------------------------------------------------------------------- the file walk

/// A directory under the workspace root could not be read.
#[must_use]
pub fn workspace_scan_failed(error: &dyn Display) -> String {
    format!("part of the workspace could not be scanned, so some files are not indexed: {error}")
}

/// The walk finished and matched nothing at all.
#[must_use]
pub fn nothing_matched(include: &[String], root: &Path) -> String {
    format!(
        "no file under {} matched index.include {include:?}: nothing will be indexed. Widen \
         index.include, or check that index.exclude and .gitignore are not excluding everything.",
        root.display()
    )
}

/// The walk stopped at `index.max_files`.
#[must_use]
pub fn index_truncated(max_files: usize) -> String {
    format!(
        "stopped at index.max_files ({max_files}): the rest of the workspace is not indexed, so \
         navigation and completion will miss it. Raise index.max_files, or narrow index.include."
    )
}

// ---------------------------------------------------------------------------- signatures

/// The rbs root that answered has an empty (or absent) `core/`.
#[must_use]
pub fn no_core_signatures(path: &Path) -> String {
    format!(
        "no signatures under {}: String, Array, Hash and the other built-in classes will be \
         missing. Point rbs.path at an unpacked rbs gem.",
        path.display()
    )
}

/// Nowhere to unpack the vendored signatures to, because the platform's cache directory
/// depends on environment variables that are not set.
#[must_use]
pub fn no_cache_directory() -> String {
    "there is no cache directory to unpack ya-lsp's own copy of Ruby's signatures into: String, \
     Array, Hash and the other built-in classes will be missing. Set HOME, or set \
     XDG_CACHE_HOME."
        .to_owned()
}

/// The vendored signatures could not be written out.
#[must_use]
pub fn signatures_not_unpacked(path: &Path, error: &dyn Display) -> String {
    format!(
        "ya-lsp's own copy of Ruby's signatures could not be unpacked into {}, so String, Array, \
         Hash and the other built-in classes will be missing: {error}",
        path.display()
    )
}

// ---------------------------------------------------------------------------- gems and Ruby

/// Most of the lockfile resolved to nothing on disk.
///
/// The threshold is a majority rather than "any": default gems live inside Ruby itself, and a
/// lockfile resolved for seven platforms names six directories that will never exist here.
#[must_use]
pub fn bundle_mostly_missing(
    lockfile: &Path,
    found: usize,
    expected: usize,
    roots: usize,
    ruby: Option<&str>,
) -> String {
    let searched = match (roots, ruby) {
        (0, _) => " (no gem directory was found at all)".to_owned(),
        (roots, Some(ruby)) => format!(" ({roots} gem directories searched, for ruby {ruby})"),
        (roots, None) => format!(" ({roots} gem directories searched)"),
    };
    format!(
        "only {found} of the {expected} gems in {} are installed anywhere ya-lsp looked\
         {searched}: navigation into gems will mostly not work. Run bundle install, or set \
         gems.paths in ya-lsp.toml to the output of gem env gemdir, or set gems.enabled = false \
         to silence this.",
        lockfile.display()
    )
}

/// No `.ruby-version` and no `.tool-versions` anywhere from the workspace root up to `$HOME`,
/// no `RUBY VERSION` in the lockfile, and no `gems.ruby_version`.
///
/// ya-lsp refuses to guess a Ruby, because guessing once put Apple's vestigial 2.6 stdlib into
/// the graph and answered `"hello".u` with `unspace`. Refusing is right; refusing *silently*
/// costs the whole of Ruby's own library — 727 files on a 3.4 install — and the only trace of
/// it was a `DEBUG` line about the bundle, which is not what went missing.
///
/// It says *where* it looked as well as what for, because after the walk those are different
/// questions: a user who has already written one of these files in a parent directory would
/// otherwise be told to go and write it again.
#[must_use]
pub fn no_ruby_version() -> String {
    "no .ruby-version or .tool-versions in this project or any directory above it, and no RUBY \
     VERSION in the lockfile, says which Ruby to use: Ruby's own library is not indexed, so \
     json, uri, forwardable and the rest of the standard library will be missing. Add a \
     .ruby-version file to the project, set gems.ruby_version in ya-lsp.toml, or set \
     gems.default_gems = false to silence this."
        .to_owned()
}

/// The Ruby version is known and its library directory is nowhere ya-lsp looked.
///
/// Distinct from [`no_ruby_version`] because the remedies are: there, ya-lsp does not know
/// which Ruby to look for; here it looked for the right one and the machine has not got it.
#[must_use]
pub fn ruby_library_missing(version: &str) -> String {
    format!(
        "no library directory for ruby {version} was found anywhere ya-lsp looked: Ruby's own \
         library is not indexed, so json, uri, forwardable and the rest of the standard library \
         will be missing. Install ruby {version}, set gems.paths in ya-lsp.toml to the output of \
         gem env gemdir, or set gems.default_gems = false to silence this."
    )
}

/// Gem indexing stopped at `gems.max_files`.
#[must_use]
pub fn gem_index_truncated(max_files: usize) -> String {
    format!(
        "stopped indexing gems at gems.max_files ({max_files}): the rest of the bundle is not \
         indexed, so navigation into those gems will not work. Raise gems.max_files, or set \
         gems.enabled = false."
    )
}

// ---------------------------------------------------------------------------- answers

/// A key in `diagnostics.rules` that is not a rule name.
#[must_use]
pub fn unknown_diagnostic_rule(name: &str, known: &str) -> String {
    format!(
        "unknown diagnostic rule {name:?} in diagnostics.rules: it is ignored, so the severity \
         set for it has no effect. The rules are {known}."
    )
}

/// `textDocument/references` found more than it will send.
///
/// Said out loud rather than only logged: a truncated find-all-references is a wrong answer
/// wearing the shape of a right one, and the user is the only one who can decide what to do
/// about it.
#[must_use]
pub fn references_truncated(found: usize, shown: usize) -> String {
    format!("{found} references found: only the first {shown} are shown.")
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// Every setting `ya-lsp.toml` has, spelled the one way a message may spell it.
    ///
    /// A message that names a key not on this list names a key that does not exist, which is a
    /// worse failure than saying nothing: the user goes and adds it.
    const SETTINGS: &[&str] = &[
        "index.include",
        "index.exclude",
        "index.load_paths",
        "index.max_files",
        "index.respect_gitignore",
        "gems.enabled",
        "gems.default_gems",
        "gems.ruby_version",
        "gems.paths",
        "gems.max_files",
        "rbs.enabled",
        "rbs.stdlib",
        "rbs.path",
        "diagnostics.enabled",
        "diagnostics.rules",
    ];

    /// Words that exist only inside this codebase. A user cannot search for any of them.
    const COINAGES: &[&str] = &[
        "gem intelligence",
        "rubydex",
        "declaration",
        "DocUri",
        "graph",
        "workspace prefix",
    ];

    /// One entry per message the server can send, with a sample of every argument.
    ///
    /// `true` marks a message that ends in another library's error, which is passed through
    /// exactly as that library wrote it and therefore does not end in a full stop. Adding a
    /// message without adding it here leaves it ungoverned, which is what this list is for.
    fn every_message() -> Vec<(&'static str, String, bool)> {
        let error = "no such file or directory (os error 2)";
        let path = Path::new("/w/ya-lsp.toml");
        let include = vec!["**/*.rb".to_owned()];
        vec![
            (
                "initialization_options_ignored",
                initialization_options_ignored(&error),
                true,
            ),
            (
                "config_file_ignored",
                config_file_ignored("ya-lsp.toml", &error),
                true,
            ),
            (
                "config_file_unreadable",
                config_file_unreadable(path, &error),
                true,
            ),
            (
                "invalid_glob",
                invalid_glob("index.exclude", "a[b", &error),
                true,
            ),
            ("include_is_empty", include_is_empty(&include), false),
            ("max_files_is_zero", max_files_is_zero(50_000), false),
            (
                "rbs_path_has_no_core",
                rbs_path_has_no_core(Path::new("/w/sig")),
                false,
            ),
            ("workspace_scan_failed", workspace_scan_failed(&error), true),
            (
                "nothing_matched",
                nothing_matched(&include, Path::new("/w")),
                false,
            ),
            ("index_truncated", index_truncated(50_000), false),
            (
                "no_core_signatures",
                no_core_signatures(Path::new("/w/sig/core")),
                false,
            ),
            ("no_cache_directory", no_cache_directory(), false),
            (
                "signatures_not_unpacked",
                signatures_not_unpacked(Path::new("/c/rbs-4.0.0"), &error),
                true,
            ),
            (
                "bundle_mostly_missing/no roots",
                bundle_mostly_missing(path, 4, 201, 0, None),
                false,
            ),
            (
                "bundle_mostly_missing/no ruby",
                bundle_mostly_missing(path, 4, 201, 2, None),
                false,
            ),
            (
                "bundle_mostly_missing",
                bundle_mostly_missing(path, 4, 201, 2, Some("3.4.1")),
                false,
            ),
            ("no_ruby_version", no_ruby_version(), false),
            ("ruby_library_missing", ruby_library_missing("3.4.1"), false),
            ("gem_index_truncated", gem_index_truncated(300_000), false),
            (
                "unknown_diagnostic_rule",
                unknown_diagnostic_rule("parse-erro", "parse-error, undefined-method"),
                false,
            ),
            (
                "references_truncated",
                references_truncated(35_733, 200),
                false,
            ),
        ]
    }

    #[test]
    fn every_message_meets_the_rule() {
        for (name, message, ends_in_a_foreign_error) in every_message() {
            let first = message.chars().next().expect("a message is not empty");
            assert!(
                !first.is_ascii_uppercase(),
                "{name}: a message opens lowercase — the client supplies the chrome, and a \
                 sentence of ours never starts one: {message}"
            );
            assert!(
                !message.contains('`'),
                "{name}: showMessage is plain text, so markup is characters the user reads: \
                 {message}"
            );
            assert!(
                !message.contains('\n'),
                "{name}: clients collapse a notification onto one line: {message}"
            );
            for bracket in ["[index", "[gems", "[rbs", "[diagnostics"] {
                assert!(
                    !message.contains(bracket),
                    "{name}: a setting is its dotted path, not its section header: {message}"
                );
            }
            for coinage in COINAGES {
                assert!(
                    !message.to_ascii_lowercase().contains(coinage),
                    "{name}: {coinage:?} is a word only this codebase uses: {message}"
                );
            }
            if ends_in_a_foreign_error {
                assert!(
                    !message.ends_with('.'),
                    "{name}: a foreign error is passed through as written, and ends the message: \
                     {message}"
                );
            } else {
                assert!(
                    message.ends_with('.'),
                    "{name}: a message ya-lsp wrote all of ends in a full stop: {message}"
                );
            }
            for named in settings_named(&message) {
                assert!(
                    SETTINGS.contains(&named.as_str()),
                    "{name}: {named:?} is not a setting ya-lsp.toml has: {message}"
                );
            }
        }
    }

    #[test]
    fn the_enumeration_covers_every_message_there_is() {
        // The list above is what makes the rule enforceable, and it is only as good as its
        // completeness. `messages.rs` is one `pub fn` per message, so the source itself is the
        // inventory to check against — a new message that skips the list fails here rather
        // than shipping ungoverned.
        let source = include_str!("messages.rs");
        let declared: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub fn "))
            .filter_map(|rest| rest.split(['(', '<']).next())
            .collect();
        let enumerated: Vec<&str> = every_message()
            .iter()
            .map(|(name, _, _)| name.split('/').next().expect("a name"))
            .collect();
        for name in &declared {
            assert!(
                enumerated.contains(name),
                "{name} is a message with no entry in every_message(), so nothing checks it"
            );
        }
        for name in &enumerated {
            assert!(
                declared.contains(name),
                "every_message() names {name}, which is not a message this module has"
            );
        }
    }

    /// Every `<section>.<key>` a message names, for the four sections `ya-lsp.toml` has.
    fn settings_named(message: &str) -> Vec<String> {
        let mut named = Vec::new();
        for section in ["index", "gems", "rbs", "diagnostics"] {
            let mut rest = message;
            while let Some(at) = rest.find(section) {
                let tail = &rest[at + section.len()..];
                rest = tail;
                let Some(key) = tail.strip_prefix('.') else {
                    continue;
                };
                let end = key
                    .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .unwrap_or(key.len());
                if end > 0 {
                    named.push(format!("{section}.{}", &key[..end]));
                }
            }
        }
        named
    }
}
