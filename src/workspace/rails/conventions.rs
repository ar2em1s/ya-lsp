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
use std::path::Path;

use super::inflect::camelize;

/// The directory the convention is anchored on. Rails puts views under `app/views`, and an
/// engine under `engines/<name>/app/views`, so what is fixed is the segment and not the prefix.
const VIEWS: &str = "views";

/// The directory Rails globs for helpers, and the one it sits in.
const HELPERS: &str = "helpers";
const APP: &str = "app";

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
}
