//! The conventions that are rules about a **path**, and nothing else.
//!
//! Rails awareness has no natural edge, so every convention in this directory is bounded. These
//! differ not by being smaller but by being checkable: `app/views/stories/show.html.erb` is
//! rendered by `StoriesController` or by nothing, and `db/queue_schema.rb` is a schema or it is
//! not. None reads a byte of the file it is asked about.
//!
//! # What the view convention actually is
//!
//! Rails renders `app/views/<path>/<action>.<format>.<handler>` from the controller `<path>` names:
//! each segment camelized, joined with `::`, with `Controller` appended to the last.
//! `stories/show.html.erb` is `StoriesController`; `admin/users/index.html.erb` is
//! `Admin::UsersController`. The file name gives the *action* and is deliberately not read; the
//! `types` module explains why the whole class is the unit.
//!
//! [`mailer_of`] is the same walk without the suffix, because a mailer's views hang off the
//! mailer's own name, and [`is_helper`] is Rails' own glob for the modules every template's view
//! context includes. `analysis::views` reads both, and the second is all a view context needs from
//! a path: which file *is* a helper, never which helper a template gets.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::inflect::camelize;

/// The directory the convention is anchored on. Rails puts views under `app/views`, and an engine
/// under `engines/<name>/app/views`, so the segment is fixed, not the prefix.
const VIEWS: &str = "views";

/// The directory Rails globs for helpers, and the one it sits in.
const HELPERS: &str = "helpers";
const APP: &str = "app";

/// The three `app/` directories Rails leaves out of the autoload paths, because none holds Ruby to
/// autoload. Everything else under `app/` is a root.
const NOT_AUTOLOADED: [&str; 3] = ["assets", "javascript", "views"];

/// The one directory name that is a root, not a namespace.
///
/// Rails' glob is `app/{*,*/concerns}`, so `app/models/concerns/` is an autoload path of its own
/// and `app/models/concerns/searchable.rb` is `Searchable`, never `Concerns::Searchable`.
const CONCERNS: &str = "concerns";

/// The file name Rails' own glob ends in: `app/helpers/**/*_helper.rb`.
const HELPER: &str = "_helper.rb";

/// What a file Zeitwerk loads is named: the one extension it walks.
const RUBY: &str = ".rb";

/// The class Rails would render `path` from, as a fully qualified Ruby constant.
///
/// `None` when the path is not a view, when it sits directly in `views/` (`views/index.html.erb`
/// has no controller directory, so belongs to nothing), or when a segment cannot spell a Ruby
/// constant. The last case matters: a constant must begin with `A`–`Z`, so a directory named `123`
/// or `спорт` names no class, and `None` is the difference between "there is no controller" and
/// reaching for a similar-looking one.
#[must_use]
pub fn controller_of(path: &Path) -> Option<String> {
    Some(namespaced(path)? + "Controller")
}

/// The class a **mailer's** own views are rendered by: the same path rule without the suffix.
///
/// `app/views/user_mailer/welcome.html.erb` is `UserMailer`, because `ActionMailer::Base` derives
/// its view path from the mailer's own name, not a controller's. The view context needs it because
/// `AbstractController::Helpers` (and so `helper_method`) is in `ActionMailer::Base` too.
///
/// **The caller must gate this; [`controller_of`] needs no gate.** A directory named `stories` can
/// only produce `StoriesController`, a name only a controller has; this one produces whatever the
/// directory spells, and `app/views/shared/` spells `Shared`. `analysis::views` gates it on the
/// application's classes that [`super::is_mailer`] recognises: the same superclass table the mailer
/// and job reader uses.
#[must_use]
pub fn mailer_of(path: &Path) -> Option<String> {
    namespaced(path)
}

/// The template a view path is, as a render call names it: `directory/name`, every extension left
/// off, and whether it is a partial (its file name starts with `_`, which the name drops).
///
/// `app/views/stories/show.html.erb` is `("stories/show", false)` and
/// `app/views/stories/_story.html.erb` is `("stories/story", true)`. `None` outside a `views`
/// directory.
#[must_use]
pub fn template_of(path: &Path) -> Option<(String, bool)> {
    let segments = segments(path);
    let views = segments.iter().rposition(|segment| *segment == VIEWS)?;
    let (file, directories) = segments.get(views + 1..)?.split_last()?;
    let name = file.split('.').next().unwrap_or_default();
    let partial = name.starts_with('_');
    let name = name.trim_start_matches('_');
    if name.is_empty() {
        return None;
    }
    let mut logical = directories.join("/");
    if !logical.is_empty() {
        logical.push('/');
    }
    logical.push_str(name);
    Some((logical, partial))
}

/// Whether `path` is a jbuilder view: `app/views/stories/show.json.jbuilder`, which the
/// view conventions read as they read an ERB template, though it is indexed whole, as Ruby.
#[must_use]
pub fn is_jbuilder(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jbuilder")
        && template_of(path).is_some()
}

/// Whether a receiverless `render` in `path` hands a template the variables of the object it runs
/// on: a controller, a mailer, a helper or a view, each under an `app` directory.
///
/// A component (`app/components`), a service or a library object that calls `render` renders
/// something of its own, with its own variables, and names no template of the application's.
#[must_use]
pub fn renders_its_own_variables(path: &Path) -> bool {
    let segments = segments(path);
    segments
        .windows(2)
        .any(|pair| pair[0] == APP && ["controllers", "mailers", HELPERS, VIEWS].contains(&pair[1]))
}

/// Whether `path` is a file Rails puts in every template's view context.
///
/// `ActionController::Base.all_helpers_from_path` globs `**/*_helper.rb` under each `app/helpers`
/// directory, so **both halves of this are Rails' own**, not a convention this crate chose: a file
/// under `app/helpers` not named `*_helper.rb` is not a default helper. An engine may have several
/// (`core/app/helpers/spree/core/controller_helpers/auth.rb`) and reaches them by `include`ing them
/// into controllers, a different mechanism that `analysis::views` answers through the ancestor
/// walk, not this list.
///
/// The anchor is a `helpers` segment directly under an `app`, which engines keep too. It matters:
/// nearly every project has a `spec/helpers/`.
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
/// The part [`controller_of`] and [`mailer_of`] share, which is all the path reading: they differ
/// by one suffix, and a second copy of this walk would one day disagree about
/// `app/views/admin/user_sessions/`.
fn namespaced(path: &Path) -> Option<String> {
    let segments: Vec<&str> = path
        .iter()
        .map(|segment| segment.to_str().unwrap_or_default())
        .collect();
    // The *last* `views`, so a path containing the word twice is read as Rails reads it: the one
    // nearest the template is what the convention hangs off.
    let views = segments.iter().rposition(|segment| *segment == VIEWS)?;
    // Everything between `views/` and the file name. The file name is the action, which this rule
    // does not read.
    let directories = segments.get(views + 1..segments.len().saturating_sub(1))?;
    constant_of(directories)
}

/// The mailer a view directory names where a `default template_path:` moved the mailer's views:
/// `mailers/notify_mailer` is `NotifyMailer` between `mailers/` and nothing.
///
/// The inverse of the setting's own rule, `"#{before}#{mailer.class.name.underscore}#{after}"`,
/// with [`mailer_of`]'s camelizing: `None` where the directory is not spelled that way, or the name
/// between is empty or cannot spell a constant. Which class that is, and whether it is a mailer
/// that setting applies to, is `analysis::views`' question.
#[must_use]
pub fn mailer_in(directory: &str, before: &str, after: &str) -> Option<String> {
    let between = directory.strip_prefix(before)?.strip_suffix(after)?;
    constant_of(&between.split('/').collect::<Vec<_>>())
}

/// Directory names as the constant Rails camelizes them into: `admin/users` is `Admin::Users`.
/// `None` for no directory, or one that cannot spell a constant (`""` included).
fn constant_of(directories: &[&str]) -> Option<String> {
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

/// A path as the segments both autoload rules read, with anything unspellable blanked.
fn segments(path: &Path) -> Vec<&str> {
    path.iter()
        .map(|segment| segment.to_str().unwrap_or_default())
        .collect()
}

/// Where a path's namespace directories begin, or `None` if it is under no autoload root.
///
/// **The three bounds both autoload rules share, in one place.** The anchor is a segment literally
/// named `app`, which engines and plugins keep too; [`NOT_AUTOLOADED`] is the list Rails
/// leaves out; and a `concerns` directly under a root is itself a root ([`CONCERNS`]). A second
/// copy would one day disagree about `app/models/concerns/`.
///
/// The answer indexes the segments *before the file name*, so it is `None` for a path with no file
/// name after the root: `app/models` and `app/models/concerns` are directories and declare nothing.
fn autoloaded_from(segments: &[&str]) -> Option<usize> {
    // The *last* `app`, for [`namespaced`]'s reason: a path containing the word twice hangs off the
    // one nearest the file.
    let app = segments.iter().rposition(|segment| *segment == APP)?;
    let root = segments.get(app + 1)?;
    if NOT_AUTOLOADED.contains(root) {
        return None;
    }
    let mut from = app + 2;
    if segments.get(from) == Some(&CONCERNS) {
        from += 1;
    }
    (from < segments.len()).then_some(from)
}

/// Every namespace Zeitwerk defines because a **directory** spells it, outermost first.
///
/// `class A::B::C` with `A::B` undefined raises `NameError` in plain Ruby. Rails runs it because
/// Zeitwerk walks the autoload paths and, for a directory with no matching `.rb` beside it,
/// **defines a module named after the directory**: an *implicit namespace*. So
/// `app/services/user/policy/not_already_silenced.rb` needs a `User::Policy` no file writes, and
/// the directory `app/services/user/policy/` is all that declares it.
///
/// Asked of the **file**, it answers the chain of directories between the autoload root and the
/// file: `app/services/chat/thread/policy/message_existence.rb` gives `Chat`, `Chat::Thread` and
/// `Chat::Thread::Policy`, each with the directory spelling it. Whether any is *already* declared
/// is not this rule's question: a directory beside a `user.rb` conjures nothing, and only the
/// caller, which holds every name the workspace and bundle declare, can tell.
///
/// **Three bounds, each Rails' own, not this crate's:** the anchor is a segment literally named
/// `app` (engines and plugins keep it too); [`NOT_AUTOLOADED`] is the list Rails leaves
/// out; and a `concerns` directly under a root is itself a root ([`CONCERNS`]), so nothing named
/// `Concerns` is ever conjured.
///
/// It cannot see Zeitwerk's acronym table: an application registering `API` gets `Api` here. That
/// is `database.yml`'s kind of escape (configuration this crate does not read), and it costs an
/// undeclared namespace, not a wrong one, because the name the files write will not match and the
/// caller drops what nothing confirms.
#[must_use]
pub fn autoloaded_namespaces(path: &Path) -> Vec<(PathBuf, String)> {
    let segments = segments(path);
    let Some(from) = autoloaded_from(&segments) else {
        return Vec::new();
    };
    // Everything between the root and the file name. The file name is the last segment and names a
    // constant, not a namespace, so it is never walked here.
    let directories = &segments[from..segments.len() - 1];

    // In bounds because [`autoloaded_from`] only answers when `from` is at most the file name's
    // index.
    let mut here: PathBuf = segments[..from].iter().copied().collect();
    let mut name = String::new();
    let mut conjured = Vec::with_capacity(directories.len());
    for directory in directories {
        here.push(directory);
        // A directory that cannot spell a constant is one Zeitwerk cannot descend either, and
        // everything below goes with it, so the chain stops instead of skipping a segment.
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

/// The constant Zeitwerk would expect the file at `path` to declare, fully qualified.
///
/// [`autoloaded_namespaces`] answers the chain of *directories* between an autoload root and a
/// file; this is that chain plus the file's own name, the whole constant:
/// `app/services/chat/thread/policy/message_existence.rb` is
/// `Chat::Thread::Policy::MessageExistence`, and `app/models/concerns/searchable.rb` is
/// `Searchable`. `None` for a path under no autoload root, one not named `*.rb`, and any segment
/// that cannot spell a Ruby constant.
///
/// **This is what the path *proposes*, and a caller about to write the answer into somebody's files
/// must confirm it against the file.** The inflector here is Zeitwerk's default, not Zeitwerk: an
/// application registering the acronym `API` declares `APIKey` in `api_key.rb`, and this says
/// `ApiKey`. [`confirmed_spelling`] resolves that for a *namespace*, where any spelling the
/// directory allows is as good as another, since nothing is written down. It cannot here: a rename
/// must **spell** the new name, and when a project's own spelling cannot be reproduced, the only
/// safe answer is not to write.
#[must_use]
pub fn autoloaded_constant(path: &Path) -> Option<String> {
    let segments = segments(path);
    let from = autoloaded_from(&segments)?;
    // In bounds because `autoloaded_from` only answers when a file name follows the root.
    let directories = &segments[from..segments.len() - 1];
    let own = named_constant(path)?;

    let mut name = String::new();
    for directory in directories {
        name.push_str(&camelize(directory)?);
        name.push_str("::");
    }
    name.push_str(&own);
    Some(name)
}

/// The constant a file's **own name** spells, with no autoload root required.
///
/// The last segment of [`autoloaded_constant`], and worth asking on its own: a file named after the
/// class it holds has something at stake when it moves, wherever it sits, and one that is not (a
/// `.rake` task, a spec, an initializer) has nothing, whatever its directory. That separates a move
/// worth a word from one worth silence.
#[must_use]
pub fn named_constant(path: &Path) -> Option<String> {
    camelize(path.file_name()?.to_str()?.strip_suffix(RUBY)?)
}

/// Whether two spellings of a constant are the same constant to two different inflectors.
///
/// [`confirmed_spelling`]'s comparison (underscores dropped, ASCII case ignored) applied to a whole
/// name instead of a chain of directories, for the same reason: `APIKey` and `ApiKey` are one class
/// in an application registering the acronym, and the acronym table is Ruby that only runs.
///
/// It is deliberately *not* how a caller decides to write a name. It answers "is this the constant
/// that file is named after", which separates a file with something at stake in a move from one
/// without; the exact comparison decides whether ya-lsp may spell the new name.
#[must_use]
pub fn same_constant(left: &str, right: &str) -> bool {
    let mut left = left.split("::");
    let mut right = right.split("::");
    loop {
        let (left, right) = (left.next(), right.next());
        match (left, right) {
            (None, None) => return true,
            (Some(left), Some(right))
                if left
                    .replace('_', "")
                    .eq_ignore_ascii_case(&right.replace('_', "")) => {}
            _ => return false,
        }
    }
}

/// How the file itself spells the chain a directory proposed, or `None` if it spells another.
///
/// **The path proposes and the file confirms: the spelling as well as the existence.**
/// [`autoloaded_namespaces`] camelizes each directory, which is Zeitwerk's default inflector, not
/// Zeitwerk: an application registering `REST` as an acronym autoloads `app/serializers/rest/` as
/// `REST`, and a chain proposed as `Rest` matches nothing that file writes. Real applications
/// register such acronyms (`ActivityPub`, `REST`, `OAuth`, `OStatus`, `RSS`, `SEO`).
///
/// The acronym table is `config/initializers/inflections.rb`, Ruby that only runs. What is on disk
/// instead is the *answer*: the file writes `REST::AccountSerializer` under a directory named
/// `rest`, and a segment is the directory's own name whatever case an inflector gave it. So the
/// match compares the directory, underscores dropped, against the segment, ignoring ASCII case:
/// `not_already_silenced` confirms `NotAlreadySilenced`, and `rest` confirms both `REST` and
/// `Rest`.
///
/// **It cannot conjure a name the directory does not spell**, the bound that matters: a file under
/// `foo/` declaring `Bar::Baz` fails at the first segment, and a constant with a different segment
/// count from the chain fails before that. What it gives up is saying a project's inflector is
/// *wrong*, and this crate never knew the inflector anyway.
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
/// A path convention like the view rule above, checkable the same way, but **not one file**. Rails
/// has supported multiple databases since 6.0, and `schema_dump_path` names the primary one
/// `db/schema.rb` and every other `db/<database>_schema.rb`. A new Rails 8 application ships three
/// of the second kind before anyone writes a line (for solid_queue, solid_cache and solid_cable),
/// e.g. `db/queue_schema.rb` and `db/cache_schema.rb`.
///
/// The file must sit **directly** in a directory called `db`, because that is what Rails joins the
/// name onto. This cannot see the convention's two escapes: `schema_dump:` in `database.yml`
/// renames the file outright, and `ENV["SCHEMA"]` overrides it; both are out of reach of a path
/// rule (reading `database.yml` would be a new file format and a new surface). The convention's
/// other half is [`is_structure`].
#[must_use]
pub fn is_schema(path: &Path) -> bool {
    is_dump(path, "schema.rb")
}

/// Whether `path` is a schema `rails db:migrate` writes when the format is `:sql`.
///
/// The same rule as [`is_schema`] with the other name, because it is the same method in Rails:
/// `schema_dump` names the primary database's dump `structure.sql` and every other
/// `<database>_structure.sql`, exactly as it names the Ruby ones `schema.rb` and
/// `<database>_schema.rb`. An application uses one format, so in practice only one of these answers
/// for a given `db/`, but nothing enforces that, and a repository that switched formats without
/// deleting the old file has both. Then [`Schema::table_names`]' rule applies, not this one's: a
/// table two schema sources declare is declared by neither.
///
/// **Reading it needs no DDL parser.** A dump's `CREATE TABLE` grammar is shared across the three
/// databases Rails supports; what differs is one keyword. [`structure`](super::structure) is the
/// reader, and names it.
///
/// [`Schema::table_names`]: super::Schema::table_names
#[must_use]
pub fn is_structure(path: &Path) -> bool {
    is_dump(path, "structure.sql")
}

/// The half both dump conventions share.
///
/// The file must sit **directly** in a directory called `db`, the directory Rails joins the name
/// onto. The secondary form is `_` plus the primary name, deliberately stricter than a bare suffix
/// test: `myschema.rb` is not a dump, while `my_schema.rb` is what Rails would write for a database
/// called `my`.
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
/// Two shapes, both anchored on `config/`, which is the whole rule: `config/routes.rb` holds
/// `Rails.application.routes.draw`, and `config/routes/<name>.rb` is what `draw :name` reads (a
/// Rails 6 feature large applications use heavily). An engine keeps both under its own root, so the
/// parent segment is fixed and the prefix is not: `api/config/routes.rb` and
/// `plugins/chat/config/routes.rb` are routes files just like the application's.
///
/// The anchor matters. Real files named `routes.rb` exist that are **not** an application's routes:
/// `core/lib/spree/testing_support/dummy_app/routes.rb` is a fixture, and some applications keep an
/// `extras/routes.rb` that is not the DSL at all. Neither sits in a `config/`.
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

    #[test]
    fn only_a_controller_a_mailer_a_helper_or_a_view_renders_with_its_own_variables() {
        for path in [
            "app/controllers/stories_controller.rb",
            "app/controllers/concerns/reshows.rb",
            "engines/blog/app/mailers/digest_mailer.rb",
            "app/helpers/stories_helper.rb",
            "app/views/stories/show.html.erb",
        ] {
            assert!(renders_its_own_variables(&PathBuf::from(path)), "{path}");
        }
        for path in [
            "app/components/card/component.rb",
            "app/services/exporter.rb",
            "lib/renderer.rb",
            "controllers/stray.rb",
        ] {
            assert!(!renders_its_own_variables(&PathBuf::from(path)), "{path}");
        }
    }

    #[test]
    fn a_jbuilder_view_is_one_under_a_views_directory() {
        for path in [
            "app/views/stories/show.json.jbuilder",
            "app/views/stories/_story.json.jbuilder",
        ] {
            assert!(is_jbuilder(&PathBuf::from(path)), "{path}");
        }
        for path in [
            "app/views/stories/show.html.erb",
            "lib/builders/story.jbuilder",
            "app/views/stories/show.rb",
        ] {
            assert!(!is_jbuilder(&PathBuf::from(path)), "{path}");
        }
    }

    #[test]
    fn a_template_is_named_as_a_render_call_names_it() {
        let named = |path: &str| template_of(&PathBuf::from(path));
        assert_eq!(
            named("app/views/stories/show.html.erb"),
            Some(("stories/show".to_owned(), false))
        );
        assert_eq!(
            named("engines/blog/app/views/admin/users/_row.html.erb"),
            Some(("admin/users/row".to_owned(), true))
        );
        // Directly under `views/`, which no controller renders but a `render template:` can name.
        assert_eq!(
            named("app/views/home.html.erb"),
            Some(("home".to_owned(), false))
        );
        // Not a view, and a file name with nothing before its extensions.
        assert_eq!(named("app/models/story.rb"), None);
        assert_eq!(named("app/views/stories/_.html.erb"), None);
    }

    /// Every shape of path a routes file can and cannot take.
    ///
    /// The two negative rows are about the `config/` anchor, and both are real shapes: a routes
    /// fixture at `core/lib/spree/testing_support/dummy_app/routes.rb`, and an `extras/routes.rb`
    /// that is not the DSL. A rule keyed on the name alone would read both.
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
    /// A table, not a test each, because what must be readable is the **chain**: a file three
    /// directories down conjures three namespaces, not one, and every empty row is a different
    /// reason for being empty.
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
            // `app/{*,*/concerns}` is Rails' own glob, so `concerns` is a root, never a namespace:
            // `Searchable`, never `Concerns::Searchable`.
            ("app/models/concerns/searchable.rb", &[]),
            // One level down from a `concerns` root is a namespace again.
            ("app/models/concerns/reports/searchable.rb", &["Reports"]),
            // The three Rails leaves out of the autoload paths.
            ("app/views/stories/show.html.erb", &[]),
            ("app/assets/config/manifest.js", &[]),
            // No `app` anchor: `lib/` is not an autoload path in a Rails application, and a file
            // there really must write its namespace down.
            ("lib/reports/story.rb", &[]),
            // An `app` with nothing after it names no root, and a root with nothing after it holds
            // no file. No walk hands over either path; both are what a `..` or a truncated argument
            // would look like.
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

    /// The file's own spelling is taken, and the directory only has to be the word.
    ///
    /// Zeitwerk's default inflector camelizes, and a project that registers an acronym does not.
    /// Every row is a real directory and constant, except the last two, which the rule must refuse.
    #[test]
    fn the_file_spells_the_namespace_and_the_directory_only_has_to_be_the_word() {
        let rows: [(&str, &str, Option<&[&str]>); 8] = [
            // Zeitwerk's own inflector, unchanged.
            (
                "app/services/user/policy/x.rb",
                "User::Policy",
                Some(&["User", "User::Policy"]),
            ),
            // An acronym the project registered.
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
            // The right words, one segment too few: a file opening a shallower namespace than its
            // path, which Zeitwerk would not have loaded from there.
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

    /// Every shape of path a file move reads a constant from, side by side.
    ///
    /// A table, not a test each, because what must be readable is where the rule stops: five rows
    /// are `None`, each for a different reason.
    #[test]
    fn a_path_names_the_class_zeitwerk_would_look_for_in_it() {
        let rows: &[(&str, Option<&str>)] = &[
            ("app/models/order.rb", Some("Order")),
            ("app/models/user_session.rb", Some("UserSession")),
            // Every directory between the root and the file, and the file itself last.
            (
                "app/services/chat/thread/policy/message_existence.rb",
                Some("Chat::Thread::Policy::MessageExistence"),
            ),
            // `concerns` directly under a root is its own root, so nothing is named `Concerns`:
            // Rails' glob is `app/{*,*/concerns}`.
            ("app/models/concerns/searchable.rb", Some("Searchable")),
            // A `concerns` that is *not* directly under a root is an ordinary directory.
            (
                "app/models/shop/concerns/priced.rb",
                Some("Shop::Concerns::Priced"),
            ),
            // The anchor is a segment literally named `app`, which an engine keeps.
            ("engines/billing/app/models/invoice.rb", Some("Invoice")),
            // The three directories under `app/` Rails leaves out of the autoload paths.
            ("app/views/stories/show.rb", None),
            ("app/assets/config/manifest.rb", None),
            // No `app` segment at all.
            ("lib/order.rb", None),
            ("config/routes.rb", None),
            // Not Ruby, so Zeitwerk never looks at it.
            ("app/views/stories/show.html.erb", None),
            // A directory that cannot spell a constant is one Zeitwerk cannot descend.
            ("app/services/123/thing.rb", None),
        ];
        for (path, expected) in rows {
            assert_eq!(
                autoloaded_constant(&PathBuf::from(path)).as_deref(),
                *expected,
                "{path}"
            );
        }
    }

    /// The file's own name, which is the half asked wherever the file sits.
    #[test]
    fn a_file_name_names_a_class_whether_or_not_anything_autoloads_it() {
        assert_eq!(
            named_constant(&PathBuf::from("lib/order.rb")).as_deref(),
            Some("Order")
        );
        assert_eq!(
            named_constant(&PathBuf::from("spec/models/order_spec.rb")).as_deref(),
            Some("OrderSpec")
        );
        // Nothing to camelize, and nothing Ruby would read as a constant if there were.
        assert_eq!(named_constant(&PathBuf::from("app/models/123.rb")), None);
        assert_eq!(named_constant(&PathBuf::from("Rakefile")), None);
        assert_eq!(named_constant(&PathBuf::from("app/models/")), None);
    }

    /// Two inflectors, one class — and where that stops being true.
    #[test]
    fn one_class_spelled_two_ways_is_one_class_and_two_classes_are_not() {
        // The acronym table is `config/initializers/inflections.rb`, Ruby that only runs, so the
        // same class is written both ways in different projects.
        assert!(same_constant("APIKey", "ApiKey"));
        assert!(same_constant(
            "REST::AccountSerializer",
            "Rest::AccountSerializer"
        ));
        assert!(same_constant("Order", "Order"));
        // An underscore belongs to the file name, never the constant, so it is dropped on both
        // sides, not one.
        assert!(same_constant("Api_Key", "ApiKey"));

        assert!(!same_constant("Order", "Purchase"));
        // A different number of segments is a different name before any spelling is compared.
        assert!(!same_constant("Shop::Order", "Order"));
        assert!(!same_constant("Order", "Shop::Order"));
        // Same letters, different words.
        assert!(!same_constant("OrderItem", "Order"));
    }

    /// The directory a confirmed name carries is still the directory, whatever the spelling.
    ///
    /// The re-spelling replaces the *name* and must not touch the path beside it:
    /// `Context::autoloaded` sorts by that path.
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

    /// A directory that cannot spell a constant stops the chain and takes what is below it.
    ///
    /// Zeitwerk cannot descend into `123/` either, so answering `Reports` for the directory above
    /// and nothing below is the honest shape, not a chain with a hole.
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
    /// A table, not a test each, because what must be readable is *where the convention stops*:
    /// three rows are `None`, each for a different reason.
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

    /// The same walk without the suffix, and the row showing why the caller must gate it.
    ///
    /// `app/views/shared/_header.html.erb` names `Shared`: a fine answer to "what constant does
    /// this directory spell" and no answer at all to "what renders this". [`controller_of`] cannot
    /// produce that shape and this one can, which is why they are two functions, not one with a
    /// flag.
    #[test]
    fn the_mailer_a_path_names() {
        let rows = [
            ("app/views/user_mailer/welcome.html.erb", Some("UserMailer")),
            (
                "app/views/admin/report_mailer/daily.text.erb",
                Some("Admin::ReportMailer"),
            ),
            // Not gated here, on purpose: a directory naming nothing in particular still spells a
            // constant, and `analysis::views` asks whether it is a mailer.
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

    /// A `default template_path:` spelled around the mailer's own name, read backwards.
    #[test]
    fn the_mailer_a_moved_view_directory_names() {
        let rows = [
            (
                "mailers/notify_mailer",
                "mailers/",
                "",
                Some("NotifyMailer"),
            ),
            (
                "mailers/admin/report_mailer",
                "mailers/",
                "",
                Some("Admin::ReportMailer"),
            ),
            (
                "emails/user_mailer/html",
                "emails/",
                "/html",
                Some("UserMailer"),
            ),
            // The setting says nothing moved.
            ("user_mailer", "", "", Some("UserMailer")),
            // Spelled some other way, or nothing between.
            ("notify_mailer", "mailers/", "", None),
            ("mailers/", "mailers/", "", None),
            ("mailers/notify_mailer", "mailers/", "/html", None),
            ("mailers/123", "mailers/", "", None),
        ];
        for (directory, before, after, expected) in rows {
            assert_eq!(
                mailer_in(directory, before, after).as_deref(),
                expected,
                "{directory}"
            );
        }
    }

    /// Rails' own glob, both halves.
    ///
    /// The `_helper.rb` half keeps `core/app/helpers/spree/core/controller_helpers/auth.rb` off the
    /// list (it really is in no view context by default), and the `app/helpers` half keeps
    /// `spec/helpers` off. Each of the four false rows fails exactly one of the two.
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
        // `split('_')` on `_stories` yields an empty first part, which must not eat the `?` from
        // `chars().next()` and answer `None` for a directory Rails accepts.
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

    /// Which files Rails would have dumped a schema into, and which not.
    ///
    /// A table, because what must be readable is that there is **more than one**: Rails names the
    /// primary database's dump `db/schema.rb` and every other `db/<database>_schema.rb`, and a new
    /// Rails 8 application ships three of the second kind. Three rows are `false`, each for a
    /// different reason.
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

    /// The same table for the other format, which is the same method in Rails.
    ///
    /// `schema_dump` names the primary database's dump and every other one the same way in both
    /// formats, so the two predicates are one rule with two names. The `false` rows are the
    /// valuable ones: repositories put the name `structure.sql` on things that are not Rails dumps,
    /// so the `db/` anchor does the same work here as there. The last row is why the secondary form
    /// is `_` plus the name, not a bare suffix.
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
        // And neither predicate ever answers for the other's file, which lets the two sources be
        // one list from `schema_declarations` on.
        for path in ["db/schema.rb", "db/animals_schema.rb"] {
            assert!(!is_structure(&PathBuf::from(path)), "{path}");
        }
        for path in ["db/structure.sql", "db/animals_structure.sql"] {
            assert!(!is_schema(&PathBuf::from(path)), "{path}");
        }
    }

    #[test]
    fn a_templates_instance_variable_is_typed_by_the_controller_its_path_names() {
        // The view↔renderer convention, end to end. A template has no enclosing class, so the
        // instance-variable machinery has nothing in the file to walk: the assignment is in another
        // file the template never names, and only a path connects them.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        // The provenance, which is what makes a convention shippable: the class and the line, both
        // in a file the card is not drawn over.
        assert!(!markdown.contains("Guessed from name alone"), "{markdown}");
        // And it is a derived answer, not a guess, which the card shows by *not* saying the other
        // thing.
        assert!(
            !markdown.contains("Guessed from name alone"),
            "a convention that names a file is not a guess: {markdown}"
        );

        // The same rung, asked about the variable itself instead of a call on it. A template has no
        // assignment of its own to walk to, so the convention that types `@story.title` must also
        // type this.
        let bare = card(&mut harness, &view, source, "@story");
        assert_eq!(bare, "```ruby\nStoriesController#@story: Story?\n```");

        // A variable the controller never assigns is not this controller's to answer for, and the
        // convention says so by finding no assignment, not a wrong one.
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
        // `app/views/admin/stories/` is `Admin::StoriesController`, and the failure that matters is
        // not missing it but reaching the *top-level* `StoriesController` instead: a different
        // controller assigning a different variable.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
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
        assert!(!markdown.contains("Guessed from name alone"), "{markdown}");
    }

    #[test]
    fn a_template_whose_controller_does_not_exist_reaches_for_no_other_one() {
        // The convention is the exact name, nothing like it. `app/views/comments/` names
        // `CommentsController`, which this application lacks, and the controller it does have
        // assigns exactly the variable the template reads, so falling back to "something similar"
        // would produce a confident, wrong, checkable-looking answer.
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
        // The rung below answers instead, wearing its own label. This shows the two are separable:
        // same file, same variable, a different tier.
        assert!(markdown.contains("Guessed from name alone"), "{markdown}");
    }

    #[test]
    fn a_controller_edited_but_not_saved_types_the_template_it_renders() {
        // Why the controller's text is read through the buffer accessor instead of straight from
        // disk: an open buffer is newer than the file on disk, so reading the assignment from disk
        // would make the one answer that crosses a file boundary lag behind what the user sees.
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
        // The graph and the filesystem can disagree until a change reaches the walk, and this rung
        // is the one place a request reads a file the cursor is not in. A controller deleted since
        // the index was built must answer nothing, not panic or produce a stale type from a path
        // that no longer resolves.
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
        // The rung below still answers, which makes this a missing file, not a broken request.
        assert!(markdown.contains("Guessed from name alone"), "{markdown}");
    }

    #[test]
    fn a_definition_jumps_to_the_same_place_the_card_names() {
        // Both new rungs go through `resolve_typed`, which hover and go-to-definition share for one
        // reason: a card and a jump disagreeing about what `@story.` is would be worse than either
        // being absent.
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
