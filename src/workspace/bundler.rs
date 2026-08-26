//! `Gemfile.lock` parsing — no Ruby, no Bundler.
//!
//! The lockfile is the only thing that says which gems a project actually uses, and Bundler
//! reads it with Ruby. We cannot: the whole point of this server is that it runs without a Ruby
//! runtime. So the format is parsed directly.
//!
//! The grammar is stable and indentation-significant. Section headers sit at column 0; inside a
//! source section two spaces mean a key, four mean a spec, six or more mean one of that spec's
//! dependencies (which we ignore — the lockfile already lists every gem in the graph at four
//! spaces, so walking the dependency tree would only re-derive what is already flat).
//!
//! Anything unrecognised is skipped rather than rejected. A newer Bundler adding a section must
//! not turn into "no gem intelligence at all"; the cost of ignoring it is at worst the gems we
//! would have missed anyway.

/// Where a locked gem comes from. The three that matter live in three different places on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// `GEM` — installed under `<gem root>/gems/<full name>`.
    Rubygems,
    /// `PATH` — a directory, relative to the lockfile itself.
    Path,
    /// `GIT` — checked out under `<gem root>/bundler/gems/<repo>-<revision[..12]>`.
    Git,
    /// `PLUGIN SOURCE` — Bundler's own plugins. Recognised only so their specs are not mistaken
    /// for project dependencies.
    Plugin,
}

/// One gem, as the lockfile spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub version: String,
    /// `None` for a pure-Ruby gem; `Some("arm64-darwin")` for a platform-specific build, which
    /// RubyGems unpacks into a directory whose name carries the platform too.
    pub platform: Option<String>,
}

impl Spec {
    /// The directory name RubyGems gives this gem — `Gem::Specification#full_name`.
    #[must_use]
    pub fn full_name(&self) -> String {
        match &self.platform {
            Some(platform) => format!("{}-{}-{}", self.name, self.version, platform),
            None => format!("{}-{}", self.name, self.version),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub kind: SourceKind,
    /// The last `remote:` line. `GEM` sections may list several mirrors; only `PATH` and `GIT`
    /// use this to find files, and those have exactly one.
    pub remote: Option<String>,
    /// `GIT` only: the resolved commit. The checkout directory is named after its first twelve
    /// characters, so a lockfile without one is unusable.
    pub revision: Option<String>,
    pub specs: Vec<Spec>,
}

impl Source {
    fn new(kind: SourceKind) -> Self {
        Self {
            kind,
            remote: None,
            revision: None,
            specs: Vec::new(),
        }
    }

    /// The name of the directory Bundler checks a git source out into.
    ///
    /// Bundler builds it from the *remote URL's* basename, not the gem name — `git@host:org/foo.git`
    /// and `https://host/org/foo` both become `foo`, and the revision is truncated to twelve
    /// characters.
    #[must_use]
    pub fn git_checkout_name(&self) -> Option<String> {
        let remote = self.remote.as_deref()?;
        let revision = self.revision.as_deref()?;
        if revision.len() < 12 {
            return None;
        }
        let base = remote
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()?
            .trim_end_matches(".git");
        if base.is_empty() {
            return None;
        }
        Some(format!("{base}-{}", &revision[..12]))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lockfile {
    /// In file order, which is the order Bundler wrote them and the order we search them.
    pub sources: Vec<Source>,
    /// From the `RUBY VERSION` section: the Ruby the lockfile was resolved against.
    pub ruby_version: Option<String>,
    pub bundled_with: Option<String>,
}

impl Lockfile {
    /// Every spec across every source, paired with the source that owns it.
    pub fn specs(&self) -> impl Iterator<Item = (&Source, &Spec)> {
        self.sources
            .iter()
            .filter(|source| source.kind != SourceKind::Plugin)
            .flat_map(|source| source.specs.iter().map(move |spec| (source, spec)))
    }

    #[must_use]
    pub fn spec_count(&self) -> usize {
        self.specs().count()
    }
}

/// Which top-level section the parser is inside, and — for a source — the source being filled in.
///
/// The source lives *in* the variant rather than beside it in an `Option`. Held separately, the
/// two encoded one state twice: `Section::Source` was true exactly when the option was `Some`,
/// which the compiler could not know, so the body needed a `let ... else { continue }` for a
/// case the header match had already made impossible. One variable, and the impossible case
/// cannot be written down.
#[derive(Debug)]
enum Section {
    /// `GEM` / `PATH` / `GIT` / `PLUGIN SOURCE` — the only ones that carry specs.
    Source(Source),
    RubyVersion,
    BundledWith,
    /// `PLATFORMS`, `DEPENDENCIES`, `CHECKSUMS`, and anything a future Bundler invents.
    Other,
}

/// Parse a `Gemfile.lock`. Never fails: a malformed file yields whatever was still legible.
#[must_use]
pub fn parse(text: &str) -> Lockfile {
    let mut lockfile = Lockfile::default();
    let mut section = Section::Other;
    let mut in_specs = false;

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }

        // A header is the only thing that starts at column 0.
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Section::Source(source) = std::mem::replace(&mut section, Section::Other) {
                lockfile.sources.push(source);
            }
            in_specs = false;
            section = match line.trim() {
                "GEM" => Section::Source(Source::new(SourceKind::Rubygems)),
                "PATH" => Section::Source(Source::new(SourceKind::Path)),
                "GIT" => Section::Source(Source::new(SourceKind::Git)),
                "PLUGIN SOURCE" => Section::Source(Source::new(SourceKind::Plugin)),
                "RUBY VERSION" => Section::RubyVersion,
                "BUNDLED WITH" => Section::BundledWith,
                _ => Section::Other,
            };
            continue;
        }

        match &mut section {
            Section::Source(source) => {
                let indent = line.len() - line.trim_start().len();
                let body = line.trim();

                if indent <= 2 {
                    // `specs:` opens the list; every other key is `name: value`.
                    if body == "specs:" {
                        in_specs = true;
                    } else if let Some((key, value)) = body.split_once(':') {
                        in_specs = false;
                        let value = value.trim().to_owned();
                        match key.trim() {
                            "remote" => source.remote = Some(value),
                            "revision" => source.revision = Some(value),
                            _ => {}
                        }
                    }
                } else if in_specs
                    && indent == 4
                    && let Some(spec) = parse_spec(body)
                {
                    source.specs.push(spec);
                }
                // indent >= 6 is a dependency of the spec above it; every gem it could name is
                // already listed at indent 4, so there is nothing here we do not have.
            }
            Section::RubyVersion => {
                if lockfile.ruby_version.is_none() {
                    lockfile.ruby_version = parse_ruby_version(line.trim());
                }
            }
            Section::BundledWith => {
                if lockfile.bundled_with.is_none() {
                    lockfile.bundled_with = Some(line.trim().to_owned());
                }
            }
            Section::Other => {}
        }
    }

    if let Section::Source(source) = section {
        lockfile.sources.push(source);
    }
    lockfile
}

/// `name (version)` or `name (version-platform)`.
///
/// The version half never contains a hyphen — Bundler spells prereleases `1.0.0.rc1` — so the
/// first hyphen inside the parentheses always starts the platform. This is the same split
/// Bundler's own `LockfileParser` makes.
fn parse_spec(line: &str) -> Option<Spec> {
    // A `!` marks a pinned dependency. It only appears in `DEPENDENCIES`, but stripping it here
    // costs nothing and keeps a hand-edited lockfile from producing a gem named `rails!`.
    let line = line.trim().trim_end_matches('!');
    let open = line.rfind(" (")?;
    let (name, rest) = line.split_at(open);
    let inside = rest.trim().strip_prefix('(')?.strip_suffix(')')?;

    // No emptiness check on the name: the line was trimmed above, so the `" ("` that `rfind`
    // found is never at index 0 and never leaves nothing in front of it.
    let name = name.trim();
    if inside.is_empty() {
        return None;
    }

    let (version, platform) = match inside.split_once('-') {
        Some((version, platform)) => (version, Some(platform.to_owned())),
        None => (inside, None),
    };
    if version.is_empty() {
        return None;
    }

    Some(Spec {
        name: name.to_owned(),
        version: version.to_owned(),
        platform,
    })
}

/// `ruby 3.2.1p123` → `3.2.1`.
///
/// The patchlevel suffix is part of the `RUBY VERSION` spelling and is never part of a directory
/// name, so it has to come off. An engine prefix (`truffleruby`, `jruby`) is dropped with it:
/// we only use the number to pick a directory.
fn parse_ruby_version(line: &str) -> Option<String> {
    let token = line
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let version = match token.split_once('p') {
        Some((version, patchlevel)) if patchlevel.chars().all(|c| c.is_ascii_digit()) => version,
        _ => token,
    };
    (!version.is_empty()).then(|| version.to_owned())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "\
GIT
  remote: https://github.com/mprokopov/telegram-bot
  revision: b8ebc2d491016b206e5ac1c41ee71e16ec94dbee
  ref: b8ebc2d491016b206e5ac1c41ee71e16ec94dbee
  specs:
    telegram-bot (0.16.7)
      actionpack (>= 4.0)
      activesupport (>= 4.0)

PATH
  remote: engines/billing
  specs:
    billing (0.1.0)
      rails (>= 7)

GEM
  remote: https://rubygems.org/
  specs:
    actionpack (8.1.3)
      actionview (= 8.1.3)
      rack (~> 3.1)
    nokogiri (1.18.8-arm64-darwin)
      racc (~> 1.4)
    zeitwerk (2.7.3)

PLATFORMS
  arm64-darwin-24
  ruby

DEPENDENCIES
  nokogiri
  telegram-bot!

RUBY VERSION
   ruby 3.4.1p0

BUNDLED WITH
   2.6.2
";

    /// A lockfile nobody would write, made of every shape the parser has to survive.
    ///
    /// `parse` never fails — a malformed file yields whatever was still legible — so the only
    /// way to test that promise is to hand it something malformed and say what "legible" meant.
    const MANGLED: &str = "\
GEM
\tremote: https://rubygems.org/
  a line with no colon in it
    indented like a spec before the list opens
  specs:
    ok-gem (1.0.0)
    (2.0.0)
    nameless ()
    dashed (-linux)
    not a spec at all
  trailing: ignored

RUBY VERSION
   ruby 3.4.1pXY
   ruby 9.9.9p0

BUNDLED WITH
   2.6.2
   9.9.9
";

    #[test]
    fn a_malformed_lockfile_yields_whatever_was_still_legible() {
        let lockfile = parse(MANGLED);

        // A tab is indentation too, so `\tremote:` is a key of the GEM source rather than a
        // header that would have closed it.
        let source = &lockfile.sources[0];
        assert_eq!(source.kind, SourceKind::Rubygems);
        assert_eq!(source.remote.as_deref(), Some("https://rubygems.org/"));

        // Of the five lines under `specs:`, only the first is a spec: `(2.0.0)` has no name,
        // `nameless ()` no version, `dashed (-linux)` an empty version before the platform, and
        // `not a spec at all` no parentheses.
        let named: Vec<&str> = source.specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(named, vec!["ok-gem"]);

        // The first line of a single-valued section wins; a second is not an overwrite.
        // `3.4.1pXY` keeps its suffix because `XY` is not a patchlevel.
        assert_eq!(lockfile.ruby_version.as_deref(), Some("3.4.1pXY"));
        assert_eq!(lockfile.bundled_with.as_deref(), Some("2.6.2"));
    }

    #[test]
    fn a_git_checkout_name_needs_a_remote_and_a_full_revision() {
        let full = |remote: &str, revision: &str| {
            let mut source = Source::new(SourceKind::Git);
            source.remote = Some(remote.to_owned());
            source.revision = Some(revision.to_owned());
            source.git_checkout_name()
        };
        let sha = "b8ebc2d491016b206e5ac1c41ee71e16ec94dbee";

        // Bundler builds the directory from the *remote URL's* basename, not the gem name.
        assert_eq!(
            full("git@host:org/foo.git", sha).as_deref(),
            Some("foo-b8ebc2d49101")
        );
        assert_eq!(
            full("https://host/org/foo/", sha).as_deref(),
            Some("foo-b8ebc2d49101")
        );

        // An abbreviated revision cannot be truncated to twelve characters, and a remote with
        // no basename left in it names no directory. Both are `None` rather than a guess.
        assert_eq!(full("https://host/org/foo", "b8ebc2d"), None);
        assert_eq!(full("/", sha), None);

        // Neither half is optional.
        let mut bare = Source::new(SourceKind::Git);
        assert_eq!(bare.git_checkout_name(), None);
        bare.remote = Some("https://host/org/foo".to_owned());
        assert_eq!(
            bare.git_checkout_name(),
            None,
            "a remote alone is not enough"
        );
    }

    #[test]
    fn every_section_of_a_real_lockfile_is_read() {
        let lockfile = parse(REAL);

        assert_eq!(lockfile.ruby_version.as_deref(), Some("3.4.1"));
        assert_eq!(lockfile.bundled_with.as_deref(), Some("2.6.2"));

        let kinds: Vec<SourceKind> = lockfile.sources.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![SourceKind::Git, SourceKind::Path, SourceKind::Rubygems]
        );

        // Four spaces is a gem; six is one of its dependencies and must not become a gem of its
        // own. `actionview` and `rack` appear only as dependencies here.
        let names: Vec<&str> = lockfile
            .specs()
            .map(|(_, spec)| spec.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "telegram-bot",
                "billing",
                "actionpack",
                "nokogiri",
                "zeitwerk"
            ]
        );
    }

    #[test]
    fn a_platform_specific_gem_keeps_the_platform_in_its_directory_name() {
        let lockfile = parse(REAL);
        let nokogiri = lockfile
            .specs()
            .find(|(_, spec)| spec.name == "nokogiri")
            .expect("nokogiri is in the fixture")
            .1;

        assert_eq!(nokogiri.version, "1.18.8");
        assert_eq!(nokogiri.platform.as_deref(), Some("arm64-darwin"));
        // RubyGems unpacks it under this exact name; guessing `nokogiri-1.18.8` finds nothing.
        assert_eq!(nokogiri.full_name(), "nokogiri-1.18.8-arm64-darwin");
    }

    #[test]
    fn a_git_source_names_its_checkout_the_way_bundler_does() {
        let lockfile = parse(REAL);
        let git = &lockfile.sources[0];
        // Repository basename plus twelve characters of the revision — verified against a real
        // `bundler/gems` directory.
        assert_eq!(
            git.git_checkout_name().as_deref(),
            Some("telegram-bot-b8ebc2d49101")
        );
    }

    #[test]
    fn ssh_and_dot_git_remotes_produce_the_same_checkout_name() {
        for remote in [
            "git@github.com:mprokopov/telegram-bot.git",
            "https://github.com/mprokopov/telegram-bot.git",
            "https://github.com/mprokopov/telegram-bot/",
        ] {
            let source = Source {
                kind: SourceKind::Git,
                remote: Some(remote.to_owned()),
                revision: Some("b8ebc2d491016b206e5ac1c41ee71e16ec94dbee".to_owned()),
                specs: Vec::new(),
            };
            assert_eq!(
                source.git_checkout_name().as_deref(),
                Some("telegram-bot-b8ebc2d49101"),
                "{remote}"
            );
        }
    }

    #[test]
    fn a_git_source_with_no_revision_has_no_checkout_to_point_at() {
        let source = Source {
            kind: SourceKind::Git,
            remote: Some("https://github.com/a/b".to_owned()),
            revision: None,
            specs: Vec::new(),
        };
        assert_eq!(source.git_checkout_name(), None);
    }

    #[test]
    fn prerelease_versions_are_not_mistaken_for_platforms() {
        // Bundler spells prereleases with dots, never hyphens, which is what makes the
        // first-hyphen split safe.
        let lockfile = parse("GEM\n  specs:\n    activeadmin (4.0.0.beta18)\n");
        let spec = &lockfile.sources[0].specs[0];
        assert_eq!(spec.version, "4.0.0.beta18");
        assert_eq!(spec.platform, None);
        assert_eq!(spec.full_name(), "activeadmin-4.0.0.beta18");
    }

    #[test]
    fn a_plugin_source_is_not_a_project_dependency() {
        let lockfile = parse("PLUGIN SOURCE\n  remote: x\n  specs:\n    some-plugin (1.0)\n");
        assert_eq!(lockfile.sources.len(), 1);
        assert_eq!(lockfile.spec_count(), 0);
    }

    #[test]
    fn an_unknown_future_section_is_skipped_rather_than_derailing_the_parse() {
        let lockfile =
            parse("CHECKSUMS\n  rails (8.0.0) sha256=abc\n\nGEM\n  specs:\n    rails (8.0.0)\n");
        assert_eq!(lockfile.spec_count(), 1);
        assert_eq!(lockfile.specs().next().unwrap().1.name, "rails");
    }

    #[test]
    fn an_empty_or_truncated_lockfile_is_survivable() {
        assert_eq!(parse("").spec_count(), 0);
        // Cut off mid-section: the source still closes, with whatever it had.
        assert_eq!(
            parse("GEM\n  remote: https://rubygems.org/\n").spec_count(),
            0
        );
        assert_eq!(parse("GEM\n  specs:\n    rails (").spec_count(), 0);
    }

    #[test]
    fn ruby_version_survives_a_patchlevel_and_an_engine() {
        assert_eq!(
            parse_ruby_version("ruby 3.2.1p123").as_deref(),
            Some("3.2.1")
        );
        assert_eq!(parse_ruby_version("ruby 3.4.1").as_deref(), Some("3.4.1"));
        assert_eq!(
            parse_ruby_version("truffleruby 22.3.0").as_deref(),
            Some("22.3.0")
        );
        assert_eq!(parse_ruby_version("ruby").as_deref(), None);
    }
}
