//! Which bodies of knowledge apply to this project, decided once per configuration.
//!
//! Every switch here turns off **what an answer is made of**, never whether the question may be
//! asked. `textDocument/hover` is answered with Rails off, and answered with less. The advertised
//! capabilities are not in scope: a request method is the wire contract, fixed at `initialize`
//! and never renegotiated, which is why `server::capabilities` takes only the encoding.
//!
//! # Why a Rails switch is about answers and not about speed
//!
//! "Turn Rails off to make it fast" is what a user will reach for and it is the wrong reason. The
//! generator lists are **reference-filtered before anything is read**: a project with no
//! `belongs_to`, no `schema.rb` and no `routes.rb` puts nothing on five of the seven lists and
//! the generators open no files.
//!
//! What a non-Rails project really pays is narrower and real. The **projection walk**, which runs
//! whichever lists are wanted — 105 ms of a keystroke on discourse when the gate makes it run.
//! The **path conventions**, which are asked of filenames rather than of calls, so any project
//! with an `app/views/` is having `rails::controller_of` applied to it whether or not it is
//! Rails: a Sinatra or Hanami application with one gets a *Derived* card citing a controller that
//! does not exist. And the **view context**, which claims `app/helpers` for every bare word in a
//! template.

use std::path::Path;

use crate::workspace::{
    bundler,
    config::{Config, Switch, Word},
};

/// The gem whose presence in a lockfile says this is Rails.
///
/// `railties` and not `rails`: the `rails` gem is a metapackage a project may leave out while
/// depending on the pieces, and every Rails application has railties whether or not it names it.
const RAILS_GEM: &str = "railties";

/// The file a Rails **application** has and nothing else does.
const RAILS_ENTRY: &str = "config/application.rb";

/// What each body of knowledge is resolved to, after `auto` has been decided.
///
/// Copied rather than borrowed: it is eight flags, it is read on the hot path by
/// `analysis::types`, and holding a reference would put a borrow of the workspace's config on
/// every rung that reads one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    /// The umbrella, after `auto`. Every Rails flag below is already `and`ed with it, so a reader
    /// asks the specific one and never both.
    pub rails: bool,
    /// `db/*schema.rb`, `db/*structure.sql`, and the macros that rename a table.
    pub schema: bool,
    /// Associations, `enum`, `attribute`, `delegate`, the 17 tail macros, and the query interface
    /// a model's relations are written against.
    pub models: bool,
    /// `config/routes.rb` and the helper module.
    pub routes: bool,
    /// Mailers, jobs and Sidekiq workers.
    pub entrypoints: bool,
    /// The view context, and the rung that types a template's `@story` from its controller.
    pub views: bool,
    /// `Struct.new` and `Data.define`. Not Rails, which is why it is `[types] structs`.
    pub structs: bool,
    /// A Sorbet `sig` and a YARD `@return`. Not Rails, for `structs`' reason.
    pub annotations: bool,
}

impl Features {
    /// What `config` asks for, with `rails.enabled = "auto"` decided against `root`.
    ///
    /// **The detection is only run for `auto`**, which is what keeps a filesystem test and a
    /// lockfile read off the path of a project that has already answered.
    #[must_use]
    pub fn resolve(root: &Path, config: &Config) -> (Self, String) {
        // Asked once, and only where the answer can matter. `Switch::decide` is still the one
        // place the three words become a boolean — handing it `false` where nothing was detected
        // is safe precisely because it only looks at that argument for `auto`.
        let detected =
            matches!(config.rails.enabled, Switch::Word(Word::Auto)).then(|| detect(root));
        let rails = config
            .rails
            .enabled
            .decide(detected.as_ref().is_some_and(|(yes, _)| *yes));
        // **Returned rather than logged here**, and the ordering is the reason: this runs inside
        // `Workspace::load`, which is *before* `[log]` has been read and therefore before the
        // file sink exists. A line written here reaches stderr and never the file a user is
        // about to attach to a bug report — which is the one place the detection is most worth
        // having. `Workspace::say_which_way_rails_went` is where it is said, and both callers
        // say it immediately after the log has been re-pointed.
        let why = match &detected {
            Some((_, why)) => format!("rails knowledge {}, detected: {why}", on_or_off(rails)),
            None => format!("rails knowledge {} by rails.enabled", on_or_off(rails)),
        };
        let features = Self {
            rails,
            schema: rails && config.rails.schema,
            models: rails && config.rails.models,
            routes: rails && config.rails.routes,
            entrypoints: rails && config.rails.entrypoints,
            views: rails && config.rails.views,
            structs: config.types.structs,
            annotations: config.types.annotations,
        };
        (features, why)
    }
}

fn on_or_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// Whether this looks like a Rails project, and the sentence that says why.
///
/// **Two halves, and both are needed.** An engine has no `config/application.rb` — it is a gem
/// with an `app/` — and a fresh clone has no `Gemfile.lock`, so either test alone answers no for
/// a project that is plainly Rails.
///
/// **The lockfile is read here rather than through `Workspace::gems()`**, and that is the one
/// coupling this function exists to avoid: `gems::discover` returns early when `gems.enabled` is
/// false and never reads the lockfile at all, so detection routed through it would mean a user
/// who turned gems off had silently also turned Rails off.
fn detect(root: &Path) -> (bool, String) {
    if root.join(RAILS_ENTRY).is_file() {
        return (true, format!("{RAILS_ENTRY} is there"));
    }
    let lockfile = root.join(bundler::LOCKFILE_NAME);
    match std::fs::read_to_string(&lockfile) {
        Ok(text) if bundler::locks(&text, RAILS_GEM) => (
            true,
            format!("{RAILS_GEM} is in {}", bundler::LOCKFILE_NAME),
        ),
        Ok(_) => (
            false,
            format!(
                "no {RAILS_ENTRY} and no {RAILS_GEM} in {}",
                bundler::LOCKFILE_NAME
            ),
        ),
        Err(_) => (
            false,
            format!("no {RAILS_ENTRY} and no {}", bundler::LOCKFILE_NAME),
        ),
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::config::PartialConfig;

    fn config(toml_text: &str) -> Config {
        let mut config = Config::default();
        config.apply(toml::from_str::<PartialConfig>(toml_text).expect("valid config"));
        config
    }

    #[test]
    fn an_application_is_found_by_its_entry_point() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join(RAILS_ENTRY), "module App\nend\n").unwrap();

        let (detected, why) = detect(dir.path());
        assert!(detected);
        assert!(why.contains(RAILS_ENTRY), "{why}");
    }

    #[test]
    fn an_engine_is_found_by_its_lockfile_because_it_has_no_entry_point() {
        // An engine is a gem with an `app/`: there is no `config/application.rb` anywhere in it,
        // and half the point of the switch is that solidus' five engines still get Rails.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "GEM\n  specs:\n    railties (7.2.3.1)\n    rake (13.0.6)\n",
        )
        .unwrap();

        let (detected, why) = detect(dir.path());
        assert!(detected, "{why}");
        assert!(why.contains("railties"), "{why}");
    }

    #[test]
    fn a_plain_gem_is_not_rails_and_says_which_of_the_two_tests_it_failed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "GEM\n  specs:\n    rake (13.0.6)\n",
        )
        .unwrap();
        let (detected, why) = detect(dir.path());
        assert!(!detected);
        assert!(why.contains("no railties"), "{why}");

        // And a fresh clone, with nothing installed and no lockfile written yet.
        let empty = tempfile::tempdir().unwrap();
        let (detected, why) = detect(empty.path());
        assert!(!detected);
        assert!(why.contains("Gemfile.lock"), "{why}");
    }

    #[test]
    fn auto_is_the_only_answer_that_looks_at_the_filesystem() {
        // A project that has said yes or no is not asked about, which is what keeps a
        // `read_to_string` and a `stat` off the path of every configuration but one.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::default().rails.enabled, Switch::Word(Word::Auto));

        let (off, _) = Features::resolve(dir.path(), &config("[rails]\nenabled = false\n"));
        assert!(!off.rails);
        let (on, _) = Features::resolve(dir.path(), &config("[rails]\nenabled = true\n"));
        assert!(on.rails, "there is no application.rb and no lockfile here");
        let (on, _) = Features::resolve(dir.path(), &config("[rails]\nenabled = \"on\"\n"));
        assert!(on.rails);
        let (off, _) = Features::resolve(dir.path(), &config("[rails]\nenabled = \"off\"\n"));
        assert!(!off.rails);

        // And auto, on the same directory, answers no.
        assert!(!Features::resolve(dir.path(), &Config::default()).0.rails);
    }

    #[test]
    fn whichever_way_it_lands_it_says_which() {
        // A detection nobody can read is a detection nobody can argue with, and
        // `workspace/gems.rs`'s `ruby_lib` comment is the standing example of what that costs.
        // The sentence is handed back rather than logged here — see `resolve`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join(RAILS_ENTRY), "module App\nend\n").unwrap();

        let (_, why) = Features::resolve(dir.path(), &Config::default());
        assert!(why.contains("rails knowledge on"), "{why}");
        assert!(why.contains(RAILS_ENTRY), "{why}");

        let (_, why) = Features::resolve(dir.path(), &config("[rails]\nenabled = false\n"));
        assert_eq!(why, "rails knowledge off by rails.enabled");
    }

    #[test]
    fn the_umbrella_is_folded_in_so_no_reader_has_to_ask_twice() {
        // Every Rails flag is already `and`ed with `rails`, which is what stops the next rung
        // being written as `features.rails && features.views` in one place and `features.views`
        // in another.
        let dir = tempfile::tempdir().unwrap();
        let (features, _) = Features::resolve(dir.path(), &config("[rails]\nenabled = false\n"));
        for on in [
            features.schema,
            features.models,
            features.routes,
            features.entrypoints,
            features.views,
        ] {
            assert!(!on);
        }
        // The two that are not Rails are untouched by it.
        assert!(features.structs);
        assert!(features.annotations);

        // And one family off leaves the others alone.
        let (features, _) = Features::resolve(
            dir.path(),
            &config("[rails]\nenabled = true\nschema = false\n\n[types]\nstructs = false\n"),
        );
        assert!(!features.schema);
        assert!(features.models);
        assert!(!features.structs);
        assert!(features.annotations);
    }
}
