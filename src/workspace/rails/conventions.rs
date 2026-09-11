//! The conventions that are rules about a **path**, and nothing else.
//!
//! Being Rails-aware is a surface with no natural edge, which is why every convention in this
//! directory is bounded. What makes these different is not that they are smaller but that they
//! are checkable — `app/views/stories/show.html.erb` is rendered by `StoriesController` or by
//! nothing, and `db/queue_schema.rb` is a schema or it is not. Neither reads a byte of the file
//! it is asked about.
//!
//! # What the view convention actually is
//!
//! Rails renders `app/views/<path>/<action>.<format>.<handler>` from the controller named by
//! `<path>`: each segment camelized, joined with `::`, and `Controller` appended to the last.
//! `stories/show.html.erb` is `StoriesController`; `admin/users/index.html.erb` is
//! `Admin::UsersController`. The file name says which *action*, and is deliberately not read —
//! see the `types` module for why the whole class is the unit.
//!
//! [`mailer_of`] is the same walk without the suffix, because a mailer's views hang off the
//! mailer's own name, and [`is_helper`] is Rails' own glob for the modules every template's view
//! context includes. Both are read by `analysis::views`, and the second is the whole of what a
//! view context needs from a path: which file *is* a helper, never which helper a template gets.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::inflect::camelize;

/// The directory the convention is anchored on. Rails puts views under `app/views`, and an
/// engine under `engines/<name>/app/views`, so what is fixed is the segment and not the prefix.
const VIEWS: &str = "views";

/// The directory Rails globs for helpers, and the one it sits in.
const HELPERS: &str = "helpers";
const APP: &str = "app";

/// The three `app/` directories Rails leaves out of the autoload paths, because none of them
/// holds Ruby to autoload. Everything else under `app/` is a root.
const NOT_AUTOLOADED: [&str; 3] = ["assets", "javascript", "views"];

/// The one directory name that is a root rather than a namespace.
///
/// Rails' own glob is `app/{*,*/concerns}`, so `app/models/concerns/` is an autoload path in its
/// own right and `app/models/concerns/searchable.rb` is `Searchable` — never `Concerns::Searchable`.
const CONCERNS: &str = "concerns";

/// The file name Rails' own glob ends in: `app/helpers/**/*_helper.rb`.
const HELPER: &str = "_helper.rb";

/// The class Rails would render `path` from, as a fully qualified Ruby constant.
///
/// `None` when the path is not a view at all, when it sits directly in `views/` (a template
/// with no controller directory — `views/index.html.erb` belongs to nothing), or when a segment
/// cannot spell a Ruby constant. The last case is the only one worth stating: a constant must
/// begin with `A`–`Z`, so a directory named `123` or `спорт` names no class, and answering
/// `None` is the difference between "there is no controller" and reaching for one that has a
/// similar-looking name.
#[must_use]
pub fn controller_of(path: &Path) -> Option<String> {
    Some(namespaced(path)? + "Controller")
}

/// The class a **mailer's** own views are rendered by: the same path rule with no suffix.
///
/// `app/views/user_mailer/welcome.html.erb` is `UserMailer`, because `ActionMailer::Base`
/// derives its view path from the mailer's own name and not from a controller's. The view context needs
/// it because `AbstractController::Helpers` — and therefore `helper_method` — is in
/// `ActionMailer::Base` too: 3 of the six corpora's 60 exported names are written in a mailer.
///
/// **The caller must gate this and [`controller_of`] does not have to be gated.** A directory
/// named `stories` can only produce `StoriesController`, which is a name nothing but a
/// controller is called; this one produces whatever the directory happens to spell, and
/// `app/views/shared/` spells `Shared`. `analysis::views` gates it on the classes the
/// application defines that [`super::is_mailer`] recognises, which is the same superclass table
/// the mailer and job reader uses.
#[must_use]
pub fn mailer_of(path: &Path) -> Option<String> {
    namespaced(path)
}

/// Whether `path` is a file Rails puts in every template's view context.
///
/// `ActionController::Base.all_helpers_from_path` globs `**/*_helper.rb` under each
/// `app/helpers` directory, so **both halves of this are Rails' own** and neither is a
/// convention this crate chose: a file under `app/helpers` that is not named `*_helper.rb` is
/// not a default helper at all. Solidus writes seven of them —
/// `core/app/helpers/spree/core/controller_helpers/auth.rb` — and reaches them by `include`ing
/// them into its controllers, which is a different mechanism and one `analysis::views` answers
/// through the ancestor walk rather than through this list.
///
/// The anchor is a `helpers` segment directly under an `app`, which is what an engine keeps
/// too. It is doing real work: `spec/helpers/` is a directory every one of the six corpora has.
#[must_use]
pub fn is_helper(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.ends_with(HELPER) || name == HELPER {
        return false;
    }
    path.parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .any(|directory| {
            directory.file_name() == Some(OsStr::new(HELPERS))
                && directory.parent().and_then(Path::file_name) == Some(OsStr::new(APP))
        })
}

/// Everything between the last `views/` and the file name, camelized and joined.
///
/// The half [`controller_of`] and [`mailer_of`] share, which is all of the path reading: what
/// separates them is one suffix, and a second copy of this walk is a second copy that would one
/// day disagree about `app/views/admin/user_sessions/`.
fn namespaced(path: &Path) -> Option<String> {
    let segments: Vec<&str> = path
        .iter()
        .map(|segment| segment.to_str().unwrap_or_default())
        .collect();
    // The *last* `views`, so that a path that happens to contain the word twice is read the way
    // Rails reads it: the nearest one to the template is the one the convention hangs off.
    let views = segments.iter().rposition(|segment| *segment == VIEWS)?;
    // Everything between `views/` and the file name. The file name is the action, which this
    // rule does not read.
    let directories = segments.get(views + 1..segments.len().saturating_sub(1))?;
    if directories.is_empty() {
        return None;
    }

    let mut name = String::new();
    for directory in directories {
        let camelized = camelize(directory)?;
        if !name.is_empty() {
            name.push_str("::");
        }
        name.push_str(&camelized);
    }
    Some(name)
}

/// Every namespace Zeitwerk defines because a **directory** spells it, outermost first.
///
/// `class A::B::C` where `A::B` is undefined raises `NameError` in plain Ruby. Rails runs it
/// because Zeitwerk walks the autoload paths and, for a directory with no matching `.rb` beside
/// it, **defines a module named after the directory** — an *implicit namespace*. So
/// `app/services/user/policy/not_already_silenced.rb` needs a `User::Policy` no file writes, and
/// the directory `app/services/user/policy/` is the whole of what declares it.
///
/// Asked of the **file**, and answers the chain of directories between the autoload root and it:
/// `app/services/chat/thread/policy/message_existence.rb` gives `Chat`, `Chat::Thread` and
/// `Chat::Thread::Policy`, each with the directory that spells it. Whether any of them is
/// *already* declared is not this rule's question — a directory beside a `user.rb` conjures
/// nothing, and the caller, which holds every name the workspace and its bundle declare, is the
/// only place that can tell.
///
/// **Three bounds, and each is Rails' own rather than this crate's.** The anchor is a segment
/// literally named `app`, which an engine and a discourse plugin keep too. [`NOT_AUTOLOADED`] is
/// the list Rails leaves out. And a `concerns` directly under a root is a root itself
/// ([`CONCERNS`]), so nothing named `Concerns` is ever conjured.
///
/// What it cannot see is Zeitwerk's acronym table: an application that registers `API` gets
/// `Api` here. That is `database.yml`'s kind of escape — configuration this crate does not read —
/// and it costs a namespace that is not declared rather than a wrong one, because the name the
/// files themselves write will not match it and the caller drops what nothing else confirms.
#[must_use]
pub fn autoloaded_namespaces(path: &Path) -> Vec<(PathBuf, String)> {
    let segments: Vec<&str> = path
        .iter()
        .map(|segment| segment.to_str().unwrap_or_default())
        .collect();
    // The *last* `app`, for [`namespaced`]'s reason: a path holding the word twice hangs off
    // the one nearest the file.
    let Some(app) = segments.iter().rposition(|segment| *segment == APP) else {
        return Vec::new();
    };
    let Some(root) = segments.get(app + 1) else {
        return Vec::new();
    };
    if NOT_AUTOLOADED.contains(root) {
        return Vec::new();
    }
    // Past the root, and past a `concerns` that is a second root. The file name is the last
    // segment and names a constant rather than a namespace, so it is never walked.
    let mut from = app + 2;
    if segments.get(from) == Some(&CONCERNS) {
        from += 1;
    }
    let Some(directories) = segments.get(from..segments.len().saturating_sub(1)) else {
        return Vec::new();
    };

    // In bounds because `directories` came back `Some`: the slice above could only be taken
    // when `from` is at most the index of the file name.
    let mut here: PathBuf = segments[..from].iter().copied().collect();
    let mut name = String::new();
    let mut conjured = Vec::with_capacity(directories.len());
    for directory in directories {
        here.push(directory);
        // A directory that cannot spell a constant is one Zeitwerk cannot descend either, and
        // everything below it goes with it — so the chain stops rather than skipping a segment.
        let Some(camelized) = camelize(directory) else {
            return conjured;
        };
        if !name.is_empty() {
            name.push_str("::");
        }
        name.push_str(&camelized);
        conjured.push((here.clone(), name.clone()));
    }
    conjured
}

/// How the file itself spells the chain a directory proposed, or `None` if it spells another.
///
/// **The path proposes and the file confirms — the spelling as well as the existence.**
/// [`autoloaded_namespaces`] camelizes each directory, which is Zeitwerk's default inflector and
/// not Zeitwerk: an application registering `REST` as an acronym autoloads `app/serializers/rest/`
/// as `REST`, and a chain proposed as `Rest` agrees with nothing that file writes. Six such names
/// on mastodon — `ActivityPub`, `REST`, `OAuth`, `OStatus`, `RSS` and `SEO` — behind 237 of the
/// 1,932 openers six corpora write.
///
/// The acronym table is `config/initializers/inflections.rb`, which is Ruby that only runs. What
/// is on disk instead is the *answer*: the file writes `REST::AccountSerializer` under a
/// directory named `rest`, and a segment is the directory's own name whatever case an inflector
/// put it in. So the match is the directory with its underscores dropped against the segment,
/// ASCII-case-insensitively — `not_already_silenced` confirms `NotAlreadySilenced` exactly as it
/// did, and `rest` now confirms `REST` as well as `Rest`.
///
/// **It cannot conjure a name the directory does not spell**, which is the bound that matters: a
/// file under `foo/` declaring `Bar::Baz` fails the comparison at the first segment, and one
/// whose constant has a different number of segments from the chain fails before that. What it
/// gives up is the ability to say a project's inflector is *wrong* — and this crate never knew
/// the inflector, so there was nothing to give up.
#[must_use]
pub fn confirmed_spelling(
    conjured: &[(PathBuf, String)],
    declared: &str,
) -> Option<Vec<(PathBuf, String)>> {
    let segments: Vec<&str> = declared.split("::").collect();
    if segments.len() != conjured.len() {
        return None;
    }
    let mut name = String::new();
    let mut spelled = Vec::with_capacity(conjured.len());
    for ((directory, _), segment) in conjured.iter().zip(&segments) {
        let spells = directory
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|last| last.replace('_', "").eq_ignore_ascii_case(segment));
        if !spells {
            return None;
        }
        if !name.is_empty() {
            name.push_str("::");
        }
        name.push_str(segment);
        spelled.push((directory.clone(), name.clone()));
    }
    Some(spelled)
}

/// Whether `path` is a schema `rails db:migrate` writes.
///
/// A path convention, like the view rule above, and checkable the same way — but **not one
/// file**. Rails has supported more than one database since 6.0, and `schema_dump_path` names
/// the primary one `db/schema.rb` and every other one `db/<database>_schema.rb`. A new Rails 8
/// application ships three of the second kind before anyone writes a line of it, for
/// solid_queue, solid_cache and solid_cable; lobsters has `db/queue_schema.rb`,
/// `db/cache_schema.rb` and `db/rack_attack_schema.rb` beside its own.
///
/// The file has to sit **directly** in a directory called `db`, because that is what Rails
/// joins the name onto. What this cannot see is the two escapes from the convention:
/// `schema_dump:` in `database.yml` renames the file outright and `ENV["SCHEMA"]` overrides it,
/// and both are out of reach of a path rule — reading `database.yml` is a new file format and a
/// new surface. The other half of the same convention is [`is_structure`].
#[must_use]
pub fn is_schema(path: &Path) -> bool {
    is_dump(path, "schema.rb")
}

/// Whether `path` is a schema `rails db:migrate` writes when the format is `:sql`.
///
/// The same rule as [`is_schema`] against the other name, because it is the same method in
/// Rails: `schema_dump` names the primary database's dump `structure.sql` and every other one
/// `<database>_structure.sql`, exactly as it names the Ruby one `schema.rb` and
/// `<database>_schema.rb`. An application has one format or the other, so in practice one of
/// these two answers for a given `db/` — but nothing enforces that, and a repository that
/// switched formats and did not delete the old file has both. What happens then is
/// [`Schema::table_names`]' rule and not this one's: a table two schema sources declare is
/// declared by neither.
///
/// **Reading it needs no DDL parser.** A dump's `CREATE TABLE` grammar is shared across the
/// three databases Rails supports, and what is *not* shared is one keyword.
/// [`structure`](super::structure) is the reader and names it.
///
/// [`Schema::table_names`]: super::Schema::table_names
#[must_use]
pub fn is_structure(path: &Path) -> bool {
    is_dump(path, "structure.sql")
}

/// The half both dump conventions share.
///
/// The file has to sit **directly** in a directory called `db`, because that is the directory
/// Rails joins the name onto. The secondary form is `_` and then the primary name, which is
/// deliberately stricter than a bare suffix test: `myschema.rb` is not a dump and
/// `my_schema.rb` is what Rails would have written for a database called `my`.
fn is_dump(path: &Path, primary: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    (name == primary
        || name
            .strip_suffix(primary)
            .is_some_and(|database| database.ends_with('_')))
        && path.parent().and_then(Path::file_name) == Some(OsStr::new("db"))
}

/// Whether `path` is a file the router draws.
///
/// Two shapes and both are `config/`-anchored, which is the whole of the rule: `config/routes.rb`
/// is what `Rails.application.routes.draw` is written in, and `config/routes/<name>.rb` is what
/// `draw :name` reads — a Rails 6 feature every large application in the corpus uses, and where
/// **440 of mastodon's 460 helpers** are. An engine keeps both under its own root, so the parent
/// segment is fixed and the prefix is not: `api/config/routes.rb` and
/// `plugins/chat/config/routes.rb` are routes files exactly as the application's own is.
///
/// The anchor is doing real work rather than tidying. Two files in the corpus are named
/// `routes.rb`, hold the same DSL, and are **not** an application's routes:
/// `core/lib/spree/testing_support/dummy_app/routes.rb` is a fixture and lobsters' own
/// `extras/routes.rb` is not the DSL at all. Neither sits in a `config/`.
#[must_use]
pub fn is_routes(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.file_name() == Some(OsStr::new("config")) {
        return path.file_name() == Some(OsStr::new("routes.rb"));
    }
    parent.file_name() == Some(OsStr::new("routes"))
        && parent.parent().and_then(Path::file_name) == Some(OsStr::new("config"))
        && path.extension() == Some(OsStr::new("rb"))
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::testing::*;
    use std::path::PathBuf;

    use super::*;

    /// Every shape of path a routes file can and cannot take.
    ///
    /// The `config/` anchor is what the two negative rows are about, and both are real files in
    /// the corpus: solidus keeps a routes fixture at
    /// `core/lib/spree/testing_support/dummy_app/routes.rb`, and lobsters' `extras/routes.rb` is
    /// not the DSL at all. A rule keyed on the name alone would read both.
    #[test]
    fn which_files_the_router_draws() {
        for path in [
            "config/routes.rb",
            "api/config/routes.rb",
            "plugins/chat/config/routes.rb",
            "config/routes/admin.rb",
            "engines/blog/config/routes/api.rb",
        ] {
            assert!(is_routes(&PathBuf::from(path)), "{path}");
        }
        for path in [
            "extras/routes.rb",
            "core/lib/spree/testing_support/dummy_app/routes.rb",
            "config/routes.rbs",
            "config/routes/admin.yml",
            "routes/admin.rb",
            "config/application.rb",
            "routes.rb",
            "",
        ] {
            assert!(!is_routes(&PathBuf::from(path)), "{path}");
        }
    }

    /// Every directory shape the autoloader is asked about, and where the chain stops.
    ///
    /// A table rather than a test each, because what has to be legible is the **chain**: a file
    /// three directories down conjures three namespaces and not one, and every empty row is a
    /// different reason for being empty.
    #[test]
    fn the_namespaces_a_directory_conjures() {
        let rows: [(&str, &[&str]); 11] = [
            // The shape the whole rule is for: two directories under a root, two namespaces.
            (
                "app/services/user/policy/not_already_silenced.rb",
                &["User", "User::Policy"],
            ),
            // An engine and a plugin keep the `app` anchor, and so the rule reaches them.
            (
                "plugins/chat/app/services/chat/thread/policy/message_existence.rb",
                &["Chat", "Chat::Thread", "Chat::Thread::Policy"],
            ),
            // Underscores camelize, which is Zeitwerk's default inflector and `camelize`'s job.
            (
                "app/services/custom_emoji/action/apply_import_rows.rb",
                &["CustomEmoji", "CustomEmoji::Action"],
            ),
            // A file directly in the root names a top-level constant and conjures nothing.
            ("app/models/user.rb", &[]),
            // `app/{*,*/concerns}` is Rails' own glob, so `concerns` is a root and never a
            // namespace — `Searchable`, never `Concerns::Searchable`.
            ("app/models/concerns/searchable.rb", &[]),
            // One level down from a `concerns` root is a namespace again.
            ("app/models/concerns/reports/searchable.rb", &["Reports"]),
            // The three Rails leaves out of the autoload paths.
            ("app/views/stories/show.html.erb", &[]),
            ("app/assets/config/manifest.js", &[]),
            // No `app` anchor at all: `lib/` is not an autoload path in a Rails application,
            // and a file there really does need the namespace written down.
            ("lib/reports/story.rb", &[]),
            // An `app` with nothing after it names no root, and a root with nothing after it
            // holds no file — neither is a path any walk hands over, and both are the shape a
            // `..` or a truncated argument would arrive as.
            ("app", &[]),
            ("app/models", &[]),
        ];
        for (path, expected) in rows {
            let conjured: Vec<String> = autoloaded_namespaces(&PathBuf::from(path))
                .into_iter()
                .map(|(_, name)| name)
                .collect();
            assert_eq!(conjured, expected, "{path}");
        }
    }

    /// The directory each name is answered with, which is what keys the generated document.
    #[test]
    fn each_conjured_namespace_carries_the_directory_that_spells_it() {
        let conjured = autoloaded_namespaces(&PathBuf::from(
            "/src/app/services/user/policy/not_already_silenced.rb",
        ));
        assert_eq!(
            conjured,
            vec![
                (PathBuf::from("/src/app/services/user"), "User".to_owned()),
                (
                    PathBuf::from("/src/app/services/user/policy"),
                    "User::Policy".to_owned()
                ),
            ]
        );
    }

    /// The file's own spelling is the one taken, and the directory only has to be the word.
    ///
    /// Zeitwerk's default inflector camelizes, and a project that registers an acronym does not.
    /// Every row here is a real directory and a real constant out of the six corpora except the
    /// last two, which are what the rule must refuse.
    #[test]
    fn the_file_spells_the_namespace_and_the_directory_only_has_to_be_the_word() {
        let rows: [(&str, &str, Option<&[&str]>); 8] = [
            // Zeitwerk's own inflector, unchanged.
            (
                "app/services/user/policy/x.rb",
                "User::Policy",
                Some(&["User", "User::Policy"]),
            ),
            // An acronym the project registered, which is all six of mastodon's.
            ("app/serializers/rest/x.rb", "REST", Some(&["REST"])),
            (
                "app/controllers/activitypub/x.rb",
                "ActivityPub",
                Some(&["ActivityPub"]),
            ),
            ("app/controllers/oauth/x.rb", "OAuth", Some(&["OAuth"])),
            ("app/lib/ostatus/x.rb", "OStatus", Some(&["OStatus"])),
            // Underscores are the directory's and never the constant's.
            (
                "app/services/auto_assignment/x.rb",
                "AutoAssignment",
                Some(&["AutoAssignment"]),
            ),
            // A constant that is not the directory at all.
            ("app/services/reports/x.rb", "Invoices", None),
            // The right words, one segment too few — a file that opens a shallower namespace
            // than its path, which Zeitwerk would not have loaded from there.
            ("app/services/chat/thread/x.rb", "Chat", None),
        ];
        for (path, declared, expected) in rows {
            let proposed = autoloaded_namespaces(&PathBuf::from(path));
            let spelled = confirmed_spelling(&proposed, declared);
            let names: Option<Vec<String>> =
                spelled.map(|chain| chain.into_iter().map(|(_, name)| name).collect());
            assert_eq!(
                names
                    .as_deref()
                    .map(|names| names.iter().map(String::as_str).collect::<Vec<_>>()),
                expected.map(<[&str]>::to_vec),
                "{path} declaring {declared}"
            );
        }
    }

    /// The directory a confirmed name carries is still the directory, whatever the spelling.
    ///
    /// The re-spelling replaces the *name* and may not touch the path beside it: that path is
    /// what `autoloaded_declarations` used to key the generated document and what
    /// `Context::autoloaded` still sorts by.
    #[test]
    fn a_re_spelled_name_keeps_the_directory_it_came_with() {
        let proposed = autoloaded_namespaces(&PathBuf::from("/src/app/serializers/rest/tag.rb"));
        assert_eq!(
            confirmed_spelling(&proposed, "REST"),
            Some(vec![(
                PathBuf::from("/src/app/serializers/rest"),
                "REST".to_owned()
            )])
        );
    }

    /// A directory that cannot spell a constant stops the chain, and takes what is under it.
    ///
    /// Zeitwerk cannot descend into `123/` either, so answering `Reports` for the directory
    /// above it and nothing for the one below is the honest shape — not a chain with a hole.
    #[test]
    fn a_directory_that_names_no_constant_ends_the_chain() {
        let conjured: Vec<String> =
            autoloaded_namespaces(&PathBuf::from("app/services/reports/123/thing.rb"))
                .into_iter()
                .map(|(_, name)| name)
                .collect();
        assert_eq!(conjured, ["Reports"]);
    }

    /// Every shape of path the convention is asked about, side by side.
    ///
    /// A table rather than a test each, because what has to be legible is *where the convention
    /// stops*: three of these rows are `None`, and each one is a different reason.
    #[test]
    fn the_controller_a_path_names() {
        let rows = [
            ("app/views/stories/show.html.erb", Some("StoriesController")),
            // The action is not read, and neither is the format or the handler.
            (
                "app/views/stories/index.html.erb",
                Some("StoriesController"),
            ),
            (
                "app/views/stories/_form.html.erb",
                Some("StoriesController"),
            ),
            (
                "app/views/stories/show.json.jbuilder",
                Some("StoriesController"),
            ),
            // Camelized per segment, joined with `::`, `Controller` on the last one only.
            (
                "app/views/admin/user_sessions/new.html.erb",
                Some("Admin::UserSessionsController"),
            ),
            // An engine: what is fixed is the `views` segment, not the `app/` above it.
            (
                "engines/blog/app/views/posts/show.html.erb",
                Some("PostsController"),
            ),
            // Directly in `views/`: there is no controller directory, so there is no controller.
            ("app/views/index.html.erb", None),
            // Not a view at all.
            ("app/models/story.rb", None),
            // A segment that cannot spell a constant. `None`, not `123Controller`.
            ("app/views/123/show.html.erb", None),
        ];
        let answers: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(path, _)| (*path, controller_of(&PathBuf::from(path))))
            .collect();
        let expected: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(path, name)| (*path, name.map(str::to_owned)))
            .collect();
        assert_eq!(answers, expected);
    }

    /// The same walk without the suffix, and the row that says why the caller has to gate it.
    ///
    /// `app/views/shared/_header.html.erb` names `Shared`, which is a perfectly good answer to
    /// "what constant does this directory spell" and no answer at all to "what renders this".
    /// [`controller_of`] cannot produce that shape and this one can, which is the whole reason
    /// the two are separate functions rather than one with a flag.
    #[test]
    fn the_mailer_a_path_names() {
        let rows = [
            ("app/views/user_mailer/welcome.html.erb", Some("UserMailer")),
            (
                "app/views/admin/report_mailer/daily.text.erb",
                Some("Admin::ReportMailer"),
            ),
            // Not gated here, deliberately: a directory that names nothing in particular still
            // spells a constant, and `analysis::views` is what asks whether it is a mailer.
            ("app/views/shared/_header.html.erb", Some("Shared")),
            ("app/views/index.html.erb", None),
            ("app/models/story.rb", None),
            ("app/views/123/show.html.erb", None),
        ];
        let answers: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(path, _)| (*path, mailer_of(&PathBuf::from(path))))
            .collect();
        let expected: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(path, name)| (*path, name.map(str::to_owned)))
            .collect();
        assert_eq!(answers, expected);
    }

    /// Rails' own glob, both halves of it.
    ///
    /// The `_helper.rb` half is what keeps solidus'
    /// `core/app/helpers/spree/core/controller_helpers/auth.rb` off the list — it really is not
    /// in any view context by default — and the `app/helpers` half is what keeps `spec/helpers`
    /// off it. Each of the four false rows fails exactly one of the two.
    #[test]
    fn the_files_rails_puts_in_every_view_context() {
        let rows = [
            ("app/helpers/application_helper.rb", true),
            ("app/helpers/admin/stories_helper.rb", true),
            ("core/app/helpers/spree/base_helper.rb", true),
            ("/srv/app/helpers/application_helper.rb", true),
            // Under `app/helpers` and not named the way the glob names them.
            ("app/helpers/spree/core/controller_helpers/auth.rb", false),
            ("app/helpers/_helper.rb", false),
            // Named the way the glob names them and not under `app/helpers`.
            ("spec/helpers/application_helper.rb", false),
            ("lib/helpers/application_helper.rb", false),
            ("app/models/application_helper.rb", false),
            ("app/helpers/application_helper.rbs", false),
            ("", false),
        ];
        let answers: Vec<(&str, bool)> = rows
            .iter()
            .map(|(path, _)| (*path, is_helper(&PathBuf::from(path))))
            .collect();
        assert_eq!(answers, rows.to_vec());
    }

    #[test]
    fn the_nearest_views_directory_is_the_one_the_convention_hangs_off() {
        assert_eq!(
            controller_of(&PathBuf::from(
                "app/views/admin/views/stories/show.html.erb"
            )),
            Some("StoriesController".to_owned())
        );
    }

    #[test]
    fn a_leading_underscore_is_not_a_segment_of_its_own() {
        // `split('_')` on `_stories` yields an empty part first, and an empty part must not eat
        // the `?` out of `chars().next()` and answer `None` for a directory Rails accepts.
        assert_eq!(
            controller_of(&PathBuf::from("app/views/_stories/show.html.erb")),
            Some("StoriesController".to_owned())
        );
    }

    #[test]
    fn a_directory_of_underscores_names_nothing() {
        assert_eq!(
            controller_of(&PathBuf::from("app/views/_/show.html.erb")),
            None
        );
    }

    #[test]
    fn an_absolute_path_reads_the_same_as_a_relative_one() {
        assert_eq!(
            controller_of(&PathBuf::from("/srv/app/views/stories/show.html.erb")),
            Some("StoriesController".to_owned())
        );
    }

    /// Which files Rails would have dumped a schema into, and which it would not.
    ///
    /// A table, because what has to be legible is that there is **more than one** — Rails names
    /// the primary database's dump `db/schema.rb` and every other one `db/<database>_schema.rb`,
    /// and a new Rails 8 application ships three of the second kind. Three rows are `false` and
    /// each is a different reason.
    #[test]
    fn the_files_rails_dumps_a_schema_into() {
        let rows = [
            ("db/schema.rb", true),
            ("db/animals_schema.rb", true),
            ("db/queue_schema.rb", true),
            ("/srv/app/db/cache_schema.rb", true),
            // Directly in `db/`, because that is the directory Rails joins the name onto.
            ("db/old/schema.rb", false),
            ("app/models/schema.rb", false),
            // A schema-shaped name that is not a dump.
            ("db/schema.rbs", false),
            ("db/schemas.rb", false),
            ("db/seeds.rb", false),
            // A path with no file name at the end of it, which a URI ending in `/` produces.
            ("db/..", false),
            ("", false),
        ];
        let answers: Vec<(&str, bool)> = rows
            .iter()
            .map(|(path, _)| (*path, is_schema(&PathBuf::from(path))))
            .collect();
        assert_eq!(answers, rows.to_vec());
    }

    /// The same table against the other format, which is the same method in Rails.
    ///
    /// `schema_dump` names the primary database's dump and every other one the same way for
    /// both formats, so the two predicates are one rule with two names — and the rows that are
    /// `false` are the ones worth having: `structure.sql` is a name a repository puts on things
    /// that are not a Rails dump, so the `db/` anchor is doing the work here that it does
    /// there. The last row is the reason the secondary form is `_` and then the name rather
    /// than a bare suffix.
    #[test]
    fn the_files_rails_dumps_a_structure_into() {
        let rows = [
            ("db/structure.sql", true),
            ("db/animals_structure.sql", true),
            ("/srv/app/db/cache_structure.sql", true),
            ("db/old/structure.sql", false),
            ("spec/fixtures/structure.sql", false),
            ("db/structure.rb", false),
            ("db/structures.sql", false),
            ("db/seeds.sql", false),
            ("db/mystructure.sql", false),
            ("db/..", false),
            ("", false),
        ];
        let answers: Vec<(&str, bool)> = rows
            .iter()
            .map(|(path, _)| (*path, is_structure(&PathBuf::from(path))))
            .collect();
        assert_eq!(answers, rows.to_vec());
        // And neither predicate ever answers for the other's file, which is what lets the two
        // sources be one list from `schema_declarations` down.
        for path in ["db/schema.rb", "db/animals_schema.rb"] {
            assert!(!is_structure(&PathBuf::from(path)), "{path}");
        }
        for path in ["db/structure.sql", "db/animals_structure.sql"] {
            assert!(!is_schema(&PathBuf::from(path)), "{path}");
        }
    }

    #[test]
    fn a_templates_instance_variable_is_typed_by_the_controller_its_path_names() {
        // The view↔renderer convention, end to end. A template has no enclosing class, so
        // the instance-variable machinery has nothing in the file to walk — the assignment is in another file that the template
        // never names, and what connects the two is a path.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        // The provenance, which is what makes a convention shippable: the class and the line,
        // both in a file the card is not drawn over.
        assert!(
            markdown.contains(
                "Type taken from `StoriesController`, line 3 — the controller Rails renders \
                 this template from."
            ),
            "{markdown}"
        );
        // And it is a derived answer rather than a guessed one, which the same card has to say
        // by *not* saying the other thing.
        assert!(
            !markdown.contains("guessed from the name"),
            "a convention that names a file is not a guess: {markdown}"
        );

        // The same rung asked about the variable itself rather than about a call on it. A
        // template has no assignment of its own to walk to, so `@story` used to answer nothing
        // at all — and the convention that types `@story.title` is the one that types this.
        let bare = card(&mut harness, &view, source, "@story");
        assert!(bare.contains("class Story"), "{bare}");
        assert!(
            bare.contains("Type taken from `StoriesController`, line 3"),
            "{bare}"
        );

        // A variable the controller never assigns is not this controller's to answer for, and
        // the convention says so by finding no assignment rather than by finding a wrong one.
        let missing = "<%= @missing.title %>\n";
        let other = harness.write("app/views/stories/edit.html.erb", missing);
        harness.index();
        let markdown = card(&mut harness, &other, missing, "title");
        assert!(!markdown.contains("StoriesController"), "{markdown}");

        // Completion asks the same question and must get the same answer.
        let offered = harness.declarations_at(&view, "<h1><%= @story.~ %></h1>\n");
        assert!(offered.contains(&"title".to_owned()), "{offered:?}");
        assert!(
            !offered.contains(&"show".to_owned()),
            "the controller's own methods are not the template's: {offered:?}"
        );
    }

    #[test]
    fn a_namespaced_view_reaches_the_namespaced_controller_or_nothing() {
        // `app/views/admin/stories/` is `Admin::StoriesController`, and the failure that
        // matters is not missing it — it is reaching the *top-level* `StoriesController`
        // instead, which is a different controller assigning a different variable.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/models/draft.rb",
            "class Draft\n  def slug\n  end\nend\n",
        );
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @thing = Story.new\n  end\nend\n",
        );
        harness.write(
            "app/controllers/admin/stories_controller.rb",
            "module Admin\n  class StoriesController\n    def show\n      @thing = Draft.new\n    end\n  end\nend\n",
        );
        let source = "<%= @thing.slug %>\n";
        let view = harness.write("app/views/admin/stories/show.html.erb", source);
        harness.index();

        let markdown = card(&mut harness, &view, source, "slug");
        assert!(markdown.contains("Draft#slug"), "{markdown}");
        assert!(
            markdown.contains("`Admin::StoriesController`"),
            "{markdown}"
        );
    }

    #[test]
    fn a_template_whose_controller_does_not_exist_reaches_for_no_other_one() {
        // The convention is the name and nothing like it. `app/views/comments/` names
        // `CommentsController`, which this application does not have — and the one it does have
        // assigns exactly the variable the template reads, so a lookup that fell back to
        // "something similar" would produce a confident, wrong, checkable-looking answer.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write("app/controllers/stories_controller.rb", CONTROLLER);
        let source = "<%= @story.title %>\n";
        let view = harness.write("app/views/comments/show.html.erb", source);
        harness.index();

        let markdown = card(&mut harness, &view, source, "title");
        assert!(
            !markdown.contains("StoriesController"),
            "no controller means no controller: {markdown}"
        );
        // What answers instead is the rung below, wearing its label. The two are separable and
        // this is where that shows: same file, same variable, a different tier.
        assert!(
            markdown.contains("Type guessed from the name `@story` alone"),
            "{markdown}"
        );
    }

    #[test]
    fn a_controller_edited_but_not_saved_types_the_template_it_renders() {
        // The reason the controller's text is read through the buffer accessor rather than
        // straight off disk. rubydex re-indexes an open buffer on every keystroke, so the graph
        // is already ahead of the file; reading the assignment from disk would make the one
        // answer that crosses a file boundary the one answer that lags behind it.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/models/draft.rb",
            "class Draft\n  def slug\n  end\nend\n",
        );
        let controller = harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @thing = Story.new\n  end\nend\n",
        );
        let source = "<%= @thing.slug %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        harness.open(
            &controller,
            "class StoriesController\n  def show\n    @thing = Draft.new\n  end\nend\n",
        );
        let markdown = card(&mut harness, &view, source, "slug");
        assert!(
            markdown.contains("Draft#slug"),
            "the unsaved buffer is what the controller says: {markdown}"
        );
    }

    #[test]
    fn a_controller_the_graph_holds_and_the_disk_does_not_answers_nothing() {
        // The graph and the filesystem can disagree for as long as it takes a change to reach
        // the walk, and this rung is the one place a request reads a file the cursor is not in.
        // A controller deleted since the index was built has to answer nothing rather than
        // panic or produce a stale type off a path that no longer resolves.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();
        std::fs::remove_file(
            harness
                .root
                .path()
                .join("app/controllers/stories_controller.rb"),
        )
        .unwrap();

        let source = "<h1><%= @story.title %></h1>\n";
        let markdown = card(&mut harness, &view, source, "title");
        assert!(!markdown.contains("StoriesController"), "{markdown}");
        // The rung below still answers, which is what makes this a missing file rather than a
        // broken request.
        assert!(
            markdown.contains("Type guessed from the name `@story` alone"),
            "{markdown}"
        );
    }

    #[test]
    fn a_definition_jumps_to_the_same_place_the_card_names() {
        // Both new rungs go through `resolve_typed`, which hover and go-to-definition share
        // for one reason: a card and a jump that disagreed about what
        // `@story.` is would be worse than either being absent.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let jumped = harness.definition_at(&view, source, "title");
        let target = jumped[0]["targetUri"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(target.ends_with("story.rb"), "{jumped}");
    }
}
