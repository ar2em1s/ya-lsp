//! Can the application load this document at all, and what may each surface do about it?
//!
//! rubydex models one graph and no environments. A `Document` has a uri and nothing else, so every
//! rule here reads the path.
//!
//! Four kinds of file the application does not load from where the cursor is:
//!
//! 1. **Test trees.** Only the suite loads them. See [`Names::in_a_test_tree`].
//! 2. **Generator templates.** A gem ships them to be *copied* into a future project; nothing ever
//!    loads them. See [`in_a_generator_template`].
//! 3. **Migrations.** One task loads one file by path, so a model copy written inside one is
//!    reachable from nothing. See [`Names::in_a_migration`].
//! 4. **Documents outside the project.** Not the project's code at all. See [`Layout::is_outside`].
//!
//! [`Fence::unloadable`] joins the first three. The fourth has its own gate.
//!
//! # Three verdicts
//!
//! **Drop.** These answer *what can I call from here, and where is it*:
//!
//! - `completion`
//! - the name rung of `definition` and `hover` ([`locator::loadable_from`](super::locator))
//! - the **root** rung of those two, plus `signatureHelp` and the outgoing callee, through
//!   `locator::resolve_call`'s root arm
//! - the place list those answer from ([`locator::places`](super::locator))
//!
//! Why drop and not sink: a sunk row still takes a slot under `MAX_COMPLETION_ITEMS` and counts
//! against `by_name`'s candidate ceiling, so the rows it buries are the real ones.
//!
//! **The place list judges a *definition*, not a declaration.** Ruby reopens freely, so a namespace
//! collects a definition in every spec that touches it. The namespace is real; the spec file is not
//! somewhere to send a reader in a controller.
//!
//! **The root rung is fenced because it is *precise*.** A member found on `Object`, `Module` or
//! `Class` is found on every receiver. A top-level `def` in a spec, or one inside an
//! `RSpec.describe` block (rubydex records both the same way), lands there. Unfenced, the answer
//! says *Resolved* and names a method the application can never call.
//!
//! When the fence removes an answer, `definition` and `hover` fall back to the name rung, which is
//! fenced too, so the reader gets what they would get if the spec's `def` did not exist.
//! `signatureHelp` and `outgoingCalls` have no fallback and draw nothing.
//!
//! **Rank.** `workspace/symbol` and the type hierarchy's subtypes. These answer *find me this*, and
//! a drop would make a real declaration unfindable. Neither carries the reader's position, so there
//! is no cursor to turn a fence off with. A rank costs nothing when wrong, and it keeps test
//! doubles below the real subclasses under the cap.
//!
//! **Never.** `references`, `rename`, `documentHighlight` and incoming calls. These answer *where
//! is this used*, and a use under `spec/` is a use: a rename that skips the suite breaks it.
//! `references` and `rename` read place lists through [`locator::sites`](super::locator), which
//! applies none of this. `supertypes` never fences either: a module a spec prepends really is in
//! the chain.
//!
//! [`locator::preferred_definition`](super::locator) is none of these. It picks *which* place a row
//! points at, and the tag only breaks a tie.
//!
//! Every other request is about the cursor's own file (outline, folding, selection, tokens, hints,
//! links, code actions, diagnostics), so it has nothing to fence. **Add a new surface to this
//! list**, not a new fence somewhere else.
//!
//! # A library is not a suite
//!
//! [`Names::in_a_test_tree`] reads directory names written for *the project's* trees. A gem's
//! `lib/rack/test/`, or railties' `rails/commands/test/`, is a published library, and calling it
//! test-only deletes real answers.
//!
//! A [`Layout`] settles it: *is this inside the project, and can `require` name it?* [`Fence`]
//! carries the cursor gate and the layout together, so no surface can get one without the other.
//!
//! **The root rung is the exception, on purpose.** A hit there answers for every receiver, so it is
//! the worst place to loosen a fence. [`Fence::loadable_on_a_root`] reads only the directory name;
//! [`Fence::loadable`] and [`Fence::only_the_suite`] read the layout.
//!
//! [`Trees`] answers the same question from the workspace's own document set, for `completion` and
//! the two ranked lists. `locator`'s rungs do not have that set, so they take a layout.
//!
//! # Generator templates
//!
//! A template tree loads nowhere, ever, which is stricter than a spec. Its files declare names the
//! project declares too (`ApplicationPolicy`, `ApplicationController`, even top-level `def`s), and
//! they sit under a gem's `lib/`, where `require` *could* name them. So the load-path clause cannot
//! help, and the tag has to say what the tree is *for*.
//!
//! It needs no [`Layout`]: a template is the same thing in a gem and in the project. That is also
//! why the root rung can read it (see [`Fence::loadable_on_a_root`]).
//!
//! **Templates stay indexed.** They are real Ruby somebody edits, and `references`, `rename` and
//! `documentHighlight` must still find uses in them.
//!
//! # Migrations
//!
//! `db/migrate` is not an autoload path. The task that runs a migration loads that one file by
//! path, which is why people write a private copy of a model inside one: the data script needs the
//! schema of the day it was written. `workspace/rails/` reads those copies like any model, because
//! they are models. Unfenced, they reach name-based lists and a jump lands in a migration.
//!
//! No *root* clause, like a template: an engine's `db/migrate/` is migrations wherever it was
//! copied from. The **load-path** clause stays as the escape hatch: the tag is a substring under
//! `db/`, so it also catches a `db/data_migrations/` a project may autoload, and putting that tree
//! on `[index] load_paths` un-fences it.
//!
//! # Outside the project
//!
//! Every rule above means *the application does not load this*. A file open **beside** the project
//! means something else: it is not the project's code at all.
//!
//! - **Inward works.** rubydex holds the file because the client sent `didOpen` and
//!   [`index_buffer`](super::Analysis) gates on nothing. So a scratch file resolves `Story` to
//!   `app/models/story.rb`, which is what its reader wants.
//! - **Outward is fenced.** [`Layout::is_outside`] is a **prefix** test, not a segment test: no
//!   word names this tree, and a project's own `scratch/` must not collide with it.
//! - **The verdict is drop on every surface**, including the rank and never ones. The project does
//!   not contain the file, so a picker row for it has nothing to find, and a rename must not edit
//!   it.
//!
//! # The cursor turns the fence off
//!
//! A developer editing a spec is exactly who the next spec's `def` is for. So the dropping surfaces
//! refuse *leaving the application for a test tree*, not test trees as such.
//!
//! [`fenced_from`] decides this once, from the cursor's document, and answers each meaning
//! separately, which is why [`Gates`] has two fields. `locator` reaches it through [`Fence::at`];
//! `completion` calls it once per request, because it is a fact about the cursor, not about any
//! candidate.
//!
//! A cursor inside a scratch document gets that document's classes completed, jumped to and
//! renamed. Every other cursor gets no trace of it.

use std::collections::HashSet;

use rubydex::model::{
    declaration::Declaration,
    graph::Graph,
    ids::{DeclarationId, UriId},
};

use super::synthesized::source_of;

/// The trees that only run under a test harness.
///
/// A **deny**-list, because repositories do not agree on where source lives but do agree on where
/// tests live. An allow-list of `app/` and `lib/` would call loaded first-party code wrong: an
/// `extras/` directory, `module Mastodon` in `config/application.rb`, a reopened class in
/// `config/initializers/`.
pub const TEST_TREES: [&str; 4] = ["spec", "test", "tests", "features"];

/// Which directory names each rule reads, once a project has said.
///
/// The three lists differ on purpose:
///
/// - [`TEST_TREES`] deletes answers when wrong, so a project **replaces** it and is shown what it
///   replaced.
/// - [`TEST_SUPPORT`] only turns the fence off, so a project **adds** to it; being wrong costs
///   nothing.
/// - The migration pair can delete answers too, so it is replaced like the first.
///
/// `Default` is the built-in lists: what `Layout::default` and every test with no opinion about
/// trees get. `Some(&[])` turns a fence **off**, which is not the same as `None`.
///
/// [`in_a_generator_template`] has no key. A template is a gem-authoring convention, not a choice a
/// project makes about its layout. If a real case turns up, it becomes a fourth key.
#[derive(Clone, Copy, Default)]
pub struct Names<'a> {
    /// Replaces [`TEST_TREES`]. `None` is the built-in four.
    test: Option<&'a [String]>,
    /// Added to [`TEST_SUPPORT`], never replacing it.
    support: &'a [String],
    /// Replaces the [`MIGRATION_ROOT`]/[`MIGRATION_MARK`] pair, each entry written
    /// `parent/mark`. `None` is the built-in pair.
    migration: Option<&'a [String]>,
}

impl<'a> Names<'a> {
    /// What a project's `[trees]` says, or the built-in lists where it says nothing.
    pub(super) fn of(trees: &'a crate::workspace::config::TreesConfig) -> Self {
        Self {
            test: trees.test.as_deref(),
            support: &trees.test_support,
            migration: trees.migration.as_deref(),
        }
    }

    /// Whether a document is in a test tree, read off its path alone.
    ///
    /// **Any segment matches, not just a prefix.** Engine monorepos keep specs in `core/spec/`, and
    /// `spec/dummy/app/models` is a spec even though a whole Rails app lives under it.
    ///
    /// The filename needs no case of its own: a Ruby file ends in `.rb`, `.erb` or `.rbs`, so it
    /// never equals one of the names. A workspace root that does equal one turns the fence off,
    /// because the cursor is then in a test tree too. That is the safe direction to fail.
    pub(super) fn in_a_test_tree(self, uri: &str) -> bool {
        match self.test {
            None => uri.split('/').any(|segment| TEST_TREES.contains(&segment)),
            Some(names) => uri
                .split('/')
                .any(|segment| names.iter().any(|name| name == segment)),
        }
    }

    /// Whether a document is a migration, read off its path alone.
    ///
    /// A pair counts only while another segment follows it, so the mark must be a *directory*. A
    /// file directly under `db/` with the word in its name is a loader, not a migration.
    pub(super) fn in_a_migration(self, uri: &str) -> bool {
        let mut segments = uri.split('/');
        let (Some(mut previous), Some(mut current)) = (segments.next(), segments.next()) else {
            return false;
        };
        for next in segments {
            if self.is_the_migration_pair(previous, current) {
                return true;
            }
            previous = current;
            current = next;
        }
        false
    }

    /// One directory and its parent, against the pairs in force.
    ///
    /// An entry with no `/` is skipped, not guessed at, and `config::validate` tells the user. A
    /// bare `migrate` would fence `app/services/migrate/` and delete real answers.
    fn is_the_migration_pair(self, parent: &str, directory: &str) -> bool {
        match self.migration {
            None => parent == MIGRATION_ROOT && directory.contains(MIGRATION_MARK),
            Some(pairs) => pairs.iter().any(|entry| {
                crate::workspace::config::migration_pair(entry)
                    .is_some_and(|(root, mark)| parent == root && directory.contains(mark))
            }),
        }
    }

    /// Whether a cursor in this document turns the fence off.
    ///
    /// The **union** of both lists. That is why the safe list can grow freely: every entry only
    /// ever makes this `true`.
    fn turns_the_fence_off(self, uri: &str) -> bool {
        self.in_a_migration(uri)
            || self.in_a_test_tree(uri)
            || uri.split('/').any(|segment| {
                TEST_SUPPORT.contains(&segment) || self.support.iter().any(|name| name == segment)
            })
    }
}

/// The directory a generator copies **out** of, and the one above it that marks it.
///
/// A Rails generator ships files it will one day write into somebody else's project. They are
/// ordinary Ruby, so indexing believes them: `class ApplicationPolicy`,
/// `class ApplicationController`, migrations, even top-level `def`s. Nobody loads any of them.
///
/// **Both names, in this order. Either alone is wrong:**
///
/// - `templates` alone deletes real libraries: `YARD::Templates::Engine` lives under
///   `lib/yard/templates/`.
/// - `generators` alone deletes the generator itself, which `rails generate` really requires.
///
/// Only the tree between them loads nowhere.
const GENERATOR_TREE: &str = "generators";
/// The second half of [`GENERATOR_TREE`]'s pair.
const TEMPLATE_TREE: &str = "templates";

/// Whether a document is a generator's template, read off its path alone.
///
/// A `templates` segment somewhere **after** a `generators` segment. The two `any` calls share one
/// iterator: the first stops at `generators` and the second continues from there. The gap varies:
/// none in `lib/generators/templates/`, four segments in railties'
/// `lib/rails/generators/rails/app/templates/`.
///
/// **No [`Layout`] needed.** [`Names::in_a_test_tree`] must ask whose tree it is, because a gem's
/// `lib/rack/test/` is a library. A template is the same thing wherever it ships from.
pub(super) fn in_a_generator_template(uri: &str) -> bool {
    let mut segments = uri.split('/');
    segments.any(|segment| segment == GENERATOR_TREE)
        && segments.any(|segment| segment == TEMPLATE_TREE)
}

/// [`in_a_generator_template`] or [`Names::in_a_migration`], from **one** walk of the path.
///
/// Both rules stay where they are written; this only asks them together. Every caller wants both
/// ([`Trees::of`] and `completion::Locality`), and the pass runs over every document in the graph
/// on each completion, so walking each path twice showed up in profiles.
///
/// The migration half keeps its shape: a pair counts only once a further segment has been seen, so
/// a directory at the end of the path is never a migration root.
pub(super) fn in_an_unloadable_tree(names: Names<'_>, uri: &str) -> bool {
    let mut after_generators = false;
    let (mut previous, mut current): (Option<&str>, Option<&str>) = (None, None);
    for segment in uri.split('/') {
        if after_generators && segment == TEMPLATE_TREE {
            return true;
        }
        if segment == GENERATOR_TREE {
            after_generators = true;
        }
        if let (Some(parent), Some(directory)) = (previous, current)
            && names.is_the_migration_pair(parent, directory)
        {
            return true;
        }
        (previous, current) = (current, Some(segment));
    }
    false
}

/// The directory a migration runs out of, and the one above it that marks it.
///
/// **`db/migrate` is not an autoload path.** Rails loads one migration file, by path, in the
/// process that runs it. A backfill copy of a model written inside one is private to that file:
/// real Ruby, really declared, reachable from nothing. This crate reads its association macros like
/// any model's, which is how they reach name-based lists.
///
/// Four shape choices:
///
/// 1. **Parent plus name, not the name alone.** `app/services/migrate/` is ordinary application
///    code; under `db/` the word is unambiguous. The known cost: a project's own
///    `app/models/db/migrations/` would be fenced.
/// 2. **A substring, not a list.** Projects already spell it `db/migrate`, `db/post_migrate` and
///    `db/old_migrations`. A fixed list would go stale silently.
/// 3. **A directory, not a file.** A file directly under `db/` with the word in its name is a
///    loader.
/// 4. **No root clause**, for [`in_a_generator_template`]'s reason: an engine's `db/migrate/` is
///    migrations wherever it was copied from. The load-path clause stays as the escape hatch; see
///    [`Fence::only_a_migration`].
const MIGRATION_ROOT: &str = "db";
/// The second half of [`MIGRATION_ROOT`]'s pair. It matches anywhere in the directory's name, so
/// `migrate`, `post_migrate` and `old_migrations` all count.
const MIGRATION_MARK: &str = "migrat";

/// The built-in migration pair, spelled the way `trees.migration` takes it.
///
/// A third spelling of the two constants above. The VS Code manifest documents this default and
/// `tests/vscode_manifest.rs` checks it against this constant.
/// `the_built_in_pair_is_the_one_the_setting_would_have_to_write` keeps all three in step.
pub const MIGRATION_PAIR: &str = "db/migrat";

/// The trees a **cursor** is read against, on top of [`TEST_TREES`].
///
/// **A separate list, on purpose.** A wrong name on the target list deletes answers, so that list
/// stays at the four names everyone agrees on. A wrong name here only turns the fence **off**, so
/// it is safe to name a tree that merely looks like test scaffolding.
///
/// One name, because only one has earned it: engines publish shared examples and factories under
/// `lib/<gem>/testing_support/` for other people's suites, and a reader there is exactly who a
/// spec's `def` is for. Broader names are not free even here: `support` also matches directories
/// that are not test code, such as `script/import_scripts/support`.
const TEST_SUPPORT: [&str; 1] = ["testing_support"];

/// Which of the two meanings a fence carries, decided by where the cursor is.
///
/// **Two fields, not one bit.** The tree meaning (*the application does not load this*) and the
/// outside meaning (*this is not the project's code*) are turned off by different cursors. With one
/// bit, a reader in a scratch document who turned the fence off to see their own file would also
/// unhide every spec, migration and test-support tree.
///
/// The second field is not a bit either; see [`Outward`].
#[derive(Clone, Copy, Default)]
pub(super) struct Gates<'a> {
    /// Whether [`Fence::unloadable`]'s three tree rules apply.
    pub(super) trees: bool,
    /// Which documents outside the project are answers here.
    pub(super) outward: Outward<'a>,
}

/// How far outside the project an answer may come from. It names the cursor's document instead of
/// carrying a yes or no.
///
/// - A cursor inside the project fences everything outside it.
/// - A cursor *outside* the project must not fence its own document: those classes are exactly what
///   its reader is asking about.
/// - But turning the gate off would also hand that reader every *other* outside document: the next
///   loose file, every unsaved buffer.
///
/// So the exception is one document wide. A loose file or unsaved buffer reaches **the project and
/// itself**, and nothing reaches it.
#[derive(Clone, Copy, Default)]
pub(super) enum Outward<'a> {
    /// No cursor: nothing is fenced, because a fence needs evidence and there is none.
    #[default]
    Unfenced,
    /// The cursor is in the project, so nothing outside the project is an answer.
    Project,
    /// The cursor is itself outside the project: the project is an answer, this one document is
    /// an answer, and nothing else out there is.
    Alone(&'a str),
}

impl<'a> Outward<'a> {
    /// Whether this gate is on at all.
    ///
    /// For a caller holding a list rather than one document: [`Fence::walk`] takes a `bool`, and a
    /// surface that would filter nothing skips the pass.
    pub(super) fn on(self) -> bool {
        !matches!(self, Self::Unfenced)
    }

    /// The cursor's document, when that document is itself outside the project.
    ///
    /// For `completion`, which cannot call [`fences`](Self::fences): it reads a `HashSet<UriId>`
    /// built at settle time, so it takes the exception as an id to exempt.
    pub(super) fn own_document(self) -> Option<&'a str> {
        match self {
            Self::Alone(uri) => Some(uri),
            _ => None,
        }
    }

    /// Whether one document is fenced out, **gate included**.
    ///
    /// One predicate instead of a gate plus a reading, because the exception sits between the two
    /// and a caller asking them separately could not express it.
    pub(super) fn fences(self, uri: &str, layout: Layout<'_>) -> bool {
        match self {
            Self::Unfenced => false,
            Self::Project => layout.is_outside(uri),
            // The cursor's own document, compared as a string: every uri here is canonical, and the
            // cursor comes from the same table as the candidates.
            Self::Alone(cursor) => uri != cursor && layout.is_outside(uri),
        }
    }
}

/// Whether a request made from `cursor` is fenced, and by which meaning.
///
/// `None` (a document the graph never held) is fenced by **neither**: a fence needs evidence, and a
/// missing path is not evidence. Every fencing surface asks here, so no two can answer differently.
///
/// The two gates read different things:
///
/// - **Trees** read a list of directory names, because a developer editing a spec is exactly who
///   the next spec's `def` is for.
/// - **Outside** reads only whether *this* cursor is outside too. If it is, the answer is
///   [`Outward::Alone`], not *off*, so the exception covers that one document and not the next
///   scratch file.
pub(super) fn fenced_from<'c>(cursor: Option<&'c str>, layout: Layout<'_>) -> Gates<'c> {
    Gates {
        // A migration is on the off-list for the same reason a spec is. Inside
        // `class Foo < Migration`, Ruby really resolves the constant to the copy declared in that
        // file, and that lexical question is not a path fence's to decide.
        trees: cursor.is_some_and(|uri| !layout.names.turns_the_fence_off(uri)),
        outward: match cursor {
            None => Outward::Unfenced,
            Some(uri) if layout.is_outside(uri) => Outward::Alone(uri),
            Some(_) => Outward::Project,
        },
    }
}

/// The documents the application does not load, grouped by tree.
///
/// Built once per request, because the set is then checked once per *definition* of every
/// candidate, and a bundle has six figures of those. A `HashSet<UriId>` lookup replaces a path
/// split; that is the whole reason this type exists.
///
/// **The tags read different sets, on purpose:**
///
/// - [`Names::in_a_test_tree`] reads `own`, the only set that knows where *this project's* test
///   trees are. A gem's `lib/rack/test/` is a library, so a miss must mean "not a spec".
/// - [`in_a_generator_template`] and [`Names::in_a_migration`] read the whole graph: templates and
///   an engine's migrations are the same thing wherever they ship from. That walk is the one `own`
///   was built with anyway.
pub(super) struct Trees<'a> {
    documents: HashSet<UriId>,
    /// Every document in the graph that is outside the project altogether.
    ///
    /// A second set rather than a flag on the first, because the verdicts differ: the first
    /// **ranks** a picker row down, this one **drops** it. A declaration the application does not
    /// load is still the project's; one written beside the project is not.
    ///
    /// **Borrowed from [`indexed::Placed`](super::indexed::Placed), never built here.**
    /// [`Layout::is_outside`] compares against every load path (hundreds of prefixes in a large
    /// bundle), and this type is built once per picker query over every document. Building it here
    /// slowed `workspace/symbol` measurably, and the settle has already built the same set.
    outside: &'a HashSet<UriId>,
}

impl<'a> Trees<'a> {
    pub(super) fn of(
        graph: &Graph,
        own: &HashSet<UriId>,
        outside: &'a HashSet<UriId>,
        names: Names<'_>,
    ) -> Self {
        let suite = own.iter().copied().filter(|id| {
            graph
                .documents()
                .get(id)
                .is_some_and(|document| names.in_a_test_tree(document.uri()))
        });
        let elsewhere = graph
            .documents()
            .iter()
            .filter(|(_, document)| in_an_unloadable_tree(names, document.uri()))
            .map(|(id, _)| *id);
        Self {
            documents: suite.chain(elsewhere).collect(),
            outside,
        }
    }

    pub(super) fn holds(&self, uri_id: &UriId) -> bool {
        self.documents.contains(uri_id)
    }

    pub(super) fn is_outside(&self, uri_id: &UriId) -> bool {
        self.outside.contains(uri_id)
    }
}

/// One walk over a declaration's definitions, and the one place the rule is written.
///
/// Surfaces already walk the definitions for other reasons (the nearest document, whether the
/// project owns it), so the tag rides along. The *decision* does not: it has two cases that are
/// easy to get wrong, and writing them once gets them right once.
#[derive(Default)]
pub(super) struct Tally {
    definitions: usize,
    testing: usize,
}

impl Tally {
    pub(super) fn saw(&mut self, in_a_test_tree: bool) {
        self.definitions += 1;
        self.testing += usize::from(in_a_test_tree);
    }

    /// Whether **any** definition of the declaration is in code the application loads.
    ///
    /// *Any*: a class the suite reopens is still the application's class. This removes only a
    /// declaration whose *every* definition is under a test tree.
    ///
    /// **Counted, so a declaration with no definitions is loadable.** rubydex's built-ins such as
    /// `Object` and `Module` have none. "Any definition is loadable" would answer *no* for them and
    /// drop the top of the object model from every list.
    pub(super) fn loadable(&self) -> bool {
        self.definitions == 0 || self.testing < self.definitions
    }
}

/// Where this project's own files are, and what `require` can name.
///
/// The root and the load path together tell **this project's** suite from somebody else's library.
/// [`Names::in_a_test_tree`] cannot, and three kinds of library code prove it:
///
/// - a gem's `lib/rack/test/`
/// - a gem's `sig/test/` (`rbs` ships one)
/// - Ruby's vendored minitest signatures under `minitest/test/` and `minitest/spec/`, indexed into
///   every project
///
/// [`Trees`] answers the same question from the workspace's document set, for `completion` and the
/// two ranked lists. `locator`'s rungs lack that set and read this instead.
#[derive(Clone, Copy, Default)]
pub struct Layout<'a> {
    /// The workspace root as a directory URI prefix, from `Analysis::workspace_prefix`.
    ///
    /// Empty matches everything: the suite rule then fences more, and
    /// [`is_outside`](Self::is_outside) fences nothing. Tests with no opinion about roots leave it
    /// empty.
    pub(super) root: &'a str,
    /// The load path as document-URI prefixes, in the order `require` searches them, from
    /// `Analysis::load_prefixes`.
    pub(super) load: &'a [String],
    /// Which directory names the three path rules read, from `[trees]`.
    ///
    /// Kept inside the layout for [`Fence`]'s reason: a fencing surface holds **one** value, so its
    /// halves cannot come apart.
    pub(super) names: Names<'a>,
    /// The `[index] load_paths` entries that resolve **outside** the root, from
    /// `Analysis::own_prefixes` — the second way a document can be the user's own.
    pub(super) own: &'a [String],
    /// Every gem root, the RBS root and Ruby's own library, from `Analysis::foreign_prefixes` —
    /// the one way a document inside the root can still be somebody else's.
    pub(super) foreign: &'a [String],
}

impl Layout<'_> {
    /// Whether a document is code the user can act on. **The one place this rule is written.**
    ///
    /// A result the user cannot act on is worse than none. Nobody fixes a warning inside a gem or
    /// edits a gem to rename their own method, and `types::from_ancestor` must not parse actionpack
    /// to learn it does not assign the app's instance variable.
    ///
    /// **Both halves are needed:**
    ///
    /// - A *vendored* bundle lives at `vendor/bundle/ruby/<abi>`, inside the root. The root test
    ///   alone would call every gem, and vendored Ruby core signatures, the user's own code.
    /// - A monorepo's shared tree named in `[index] load_paths` sits outside the root but is the
    ///   user's own. Without [`Self::own`] it is indexed and then treated as a gem.
    ///
    /// Compared as URI prefixes: every side comes from `Url::from_file_path`, so all are canonical.
    pub(super) fn is_own(self, uri: &str) -> bool {
        let named =
            uri.starts_with(self.root) || self.own.iter().any(|prefix| uri.starts_with(prefix));
        named && !self.foreign.iter().any(|prefix| uri.starts_with(prefix))
    }

    /// Whether a document is outside the project altogether. **Not** the same question as
    /// [`is_own`](Self::is_own).
    ///
    /// A gem's file is not the user's own, but it is fully part of the project: indexed at startup,
    /// its root registered, named by `require`. This asks whether the document is anywhere the
    /// project reaches. Four prefix sets cover every such place: the workspace root, the load path,
    /// `[index] load_paths` outside the root, and every gem, RBS and Ruby library root. A document
    /// under none of them came only from a client's `didOpen`: a file open **beside** the project.
    ///
    /// **A prefix, never a segment**, the opposite of [`Names::in_a_test_tree`]. No word names this
    /// tree, and a project's own `scratch/` must not collide with it, so the only question is where
    /// the directory *is*.
    ///
    /// **An empty root fences nothing.** Every uri starts with `""`, so this falls out of the
    /// prefix test. It is the right direction for a new fence to fail: tests with no opinion, and
    /// the window before the workspace is known, both get more.
    ///
    /// Asked of the *source* uri, for [`only_the_suite`](Fence::only_the_suite)'s reason: a
    /// generated document's uri has a scheme in front and is not a path.
    pub(super) fn is_outside(self, uri: &str) -> bool {
        let uri = source_of(uri);
        // Three short lists first and the long one last. This orders the checks without changing
        // the answer, and it runs for every document in the graph at settle. `self.load` holds one
        // entry per gem (hundreds), while the root is one prefix, `own` is the user's
        // `[index] load_paths` and `foreign` is one gem directory per Ruby. Project and gem
        // documents leave early; only the few that are neither pay for the long scan.
        if uri.starts_with(self.root)
            || self.own.iter().any(|prefix| uri.starts_with(prefix))
            || self.foreign.iter().any(|prefix| uri.starts_with(prefix))
        {
            return false;
        }
        // The one list the three above cannot cover: a Gemfile gem with a `path:` or `git:` source
        // puts its `lib/` on the load path and lives under no gem root. In a monorepo that is the
        // sibling directory beside the application.
        !self.load.iter().any(|prefix| uri.starts_with(prefix))
    }
}

/// The fence every surface applies: whether the cursor turns it on, and the [`Layout`] that tells a
/// library from a suite.
///
/// **One value, so no surface can get one half without the other.** When they were separate
/// arguments, only [`locator::places`](super::locator) got the layout, and a method defined only in
/// rack-test's `lib/rack/test/utils.rb` answered nothing from the name rung.
#[derive(Clone, Copy)]
pub struct Fence<'a> {
    on: Gates<'a>,
    layout: Layout<'a>,
}

impl<'a> Fence<'a> {
    /// The fence a request asked from `cursor` carries — both meanings, each on its own gate.
    pub(super) fn at(cursor: Option<&'a str>, layout: Layout<'a>) -> Self {
        Self {
            on: fenced_from(cursor, layout),
            layout,
        }
    }

    /// The fence a *where is this used* surface carries: the tree meaning **off**, the outside
    /// meaning **on**.
    ///
    /// Built in [`locator::resolve`](super::locator), which `references`, `rename` and both
    /// hierarchies go through. A use under `spec/` is a use, so the tree rules must not apply. A
    /// document outside the project is not code anyone is renaming, so a work list must not offer
    /// to edit it.
    ///
    /// Spelled out at the call site so both halves are decisions, not omissions.
    pub(super) fn uses(cursor: Option<&'a str>, layout: Layout<'a>) -> Self {
        Self {
            on: Gates {
                trees: false,
                outward: fenced_from(cursor, layout).outward,
            },
            layout,
        }
    }

    /// Whether the three tree rules apply here. See [`fenced_from`].
    pub(super) fn on_trees(self) -> bool {
        self.on.trees
    }

    /// Whether a document is fenced out for being outside the project.
    ///
    /// **The gate is inside this one**, unlike [`unloadable`](Self::unloadable), whose callers
    /// check the gate themselves. Here the exception *is* the gate: the cursor's own document is
    /// outside the project and must not be fenced. A caller that checked a bool and then asked the
    /// raw reading would fence the very document the question came from.
    pub(super) fn outside(self, uri: &str) -> bool {
        self.on.outward.fences(uri, self.layout)
    }

    /// Whether a document is one only **this project's** test run loads.
    ///
    /// All three must hold:
    ///
    /// 1. **The path says test.** [`Names::in_a_test_tree`]: directory names only, since a
    ///    `Document` has nothing else to ask.
    /// 2. **It is inside the workspace.** The names describe *the project's* trees. This leaves out
    ///    everything indexed from outside: a gem's `sig/test/`, and Ruby's minitest signatures
    ///    under `minitest/test/` and `minitest/spec/`.
    /// 3. **`require` cannot name it.** This keeps a *vendored* gem's `lib/rack/test/` (inside the
    ///    workspace, still a library) and the project's own `lib/foo/test/` (on the load path, so
    ///    loaded). A project that puts `spec/` on `[index] load_paths` gets what it asked for.
    ///
    /// An empty [`Layout`] (before the bundle is discovered, or a test with no opinion) leaves
    /// clause 1 deciding alone, which fences more.
    ///
    /// Clauses 2 and 3 read the *source* uri; that is what [`source_of`] is for.
    fn only_the_suite(self, uri: &str) -> bool {
        let uri = source_of(uri);
        self.layout.names.in_a_test_tree(uri)
            && uri.starts_with(self.layout.root)
            && !self
                .layout
                .load
                .iter()
                .any(|prefix| uri.starts_with(prefix.as_str()))
    }

    /// Whether the application never loads a document: the whole question, in one place.
    ///
    /// Three reasons:
    ///
    /// 1. [`only_the_suite`](Self::only_the_suite): only the suite loads it. Needs a [`Layout`],
    ///    because directory names cannot say whose tree it is.
    /// 2. [`in_a_generator_template`]: copied, never loaded, by anyone. Needs no layout: the pair
    ///    of names says what the tree is *for*.
    /// 3. [`only_a_migration`](Self::only_a_migration): one task loads it, alone. No root clause
    ///    (an engine's `db/migrate/` is still migrations), but it keeps the load-path escape hatch.
    ///
    /// **The index is deliberately untouched.** These files are real Ruby, and `references`,
    /// `rename` and `documentHighlight` must still find uses in them. The tag answers only the
    /// surfaces the module docs list as drop or rank.
    pub(super) fn unloadable(self, uri: &str) -> bool {
        self.only_the_suite(uri)
            || in_a_generator_template(source_of(uri))
            || self.only_a_migration(uri)
    }

    /// Whether a document is one only the migration task loads.
    ///
    /// [`Names::in_a_migration`], **and `require` cannot name it**: one of
    /// [`only_the_suite`](Self::only_the_suite)'s two layout clauses. No root clause, because an
    /// engine's `db/migrate/` is migrations like the project's. That is also why [`Trees`] and
    /// [`loadable_on_a_root`](Self::loadable_on_a_root) read the bare tag.
    ///
    /// **The load-path clause is the escape hatch.** The tag is a substring under `db/`, so it also
    /// catches `db/data_migrations/`. A project that puts that tree on `[index] load_paths` has
    /// said `require` reaches it, and gets its jumps, guesses and cards back.
    ///
    /// **Known gap: `completion` and the picker ignore the escape hatch.** They read [`Trees`] and
    /// have no [`Layout`]; the test-tree rule has the same gap there. Fixing it means giving those
    /// two surfaces a layout.
    fn only_a_migration(self, uri: &str) -> bool {
        let uri = source_of(uri);
        self.layout.names.in_a_migration(uri)
            && !self
                .layout
                .load
                .iter()
                .any(|prefix| uri.starts_with(prefix.as_str()))
    }

    /// Whether the application loads any definition of one declaration.
    ///
    /// [`Tally`]'s rule for a caller holding only an id. **True outright when the fence is off**,
    /// so no caller can apply the rule and forget the cursor.
    ///
    /// **A declaration missing from the graph is not loadable.** This differs from
    /// [`Tally::loadable`]'s no-definitions rule: there the question is about definitions, here the
    /// lookup failed and there is nowhere to send anyone.
    pub(super) fn loadable(self, graph: &Graph, id: DeclarationId) -> bool {
        self.walk(graph, id, self.on.trees, |uri| self.unloadable(uri))
    }

    /// Whether **any** definition of one declaration is inside the project.
    ///
    /// [`Tally`]'s rule through [`Layout::is_outside`], gated separately, so a surface that never
    /// fences test trees can still refuse a scratch document. *Any*: a class the user reopens in a
    /// file beside the project is still the project's class. Only a declaration whose *every*
    /// definition is outside goes.
    pub(super) fn inside(self, graph: &Graph, id: DeclarationId) -> bool {
        self.walk(graph, id, self.on.outward.on(), |uri| self.outside(uri))
    }

    /// [`loadable`](Self::loadable) for a member found on a **root**. Reads only the directory
    /// name, never the [`Layout`].
    ///
    /// A hit on `Object`, `Module` or `Class` is a hit on every receiver. rubydex has no notion of
    /// a block or a script, so a top-level `def` anywhere becomes a member of everything. That
    /// makes this the fence's hardest case.
    ///
    /// **Why no layout here.** The layout asks *can the application load this file*, which suits a
    /// candidate list and fails here. `rbs` ships `lib/rbs/test/setup.rb`, a requireable script
    /// with a top-level `def match`. Exempting it turned `match` in a routes file from a *Guessed*
    /// list that held the right answer into a one-place *Resolved* card pointing at an RBS test
    /// harness. A fence loosened where being wrong is worst is loosened backwards. The cost: a
    /// gem's top-level `def` under a `test` directory, which nobody should be sent to anyway.
    ///
    /// [`in_a_generator_template`] and [`Names::in_a_migration`] apply here too, since both already
    /// say what a tree is *for*. Templates are the worst case: a gem's cucumber-steps template has
    /// a top-level `def with_ivars` that would otherwise be a member of every receiver.
    pub(super) fn loadable_on_a_root(self, graph: &Graph, id: DeclarationId) -> bool {
        self.walk(graph, id, self.on.trees, |uri| {
            self.layout.names.in_a_test_tree(uri)
                || in_a_generator_template(uri)
                || self.layout.names.in_a_migration(uri)
        }) && self.inside(graph, id)
    }

    /// One walk over a declaration's definitions, with the gate and the path reading passed in.
    ///
    /// `on` is passed instead of read from `self` because there are two gates and each walk answers
    /// for exactly one. A trees question must not be switched off by a cursor that only switched
    /// off the outside meaning.
    fn walk(
        self,
        graph: &Graph,
        id: DeclarationId,
        on: bool,
        unloadable: impl Fn(&str) -> bool,
    ) -> bool {
        if !on {
            return true;
        }
        graph.declarations().get(&id).is_some_and(|declaration| {
            let mut tally = Tally::default();
            for definition_id in declaration.definitions() {
                tally.saw(
                    graph
                        .definitions()
                        .get(definition_id)
                        .and_then(|definition| graph.documents().get(definition.uri_id()))
                        .is_some_and(|document| unloadable(document.uri())),
                );
            }
            tally.loadable()
        })
    }
}

/// Where a declaration is written, from the one walk both ranked lists need.
///
/// Two questions asked together, because asking them apart means two walks over every candidate for
/// one sort key.
pub(super) struct Placement {
    /// Whether any definition is in the user's own code.
    ///
    /// *Any*, not the first: a class the project reopens is the project's, even when the gem that
    /// defined it sorts first.
    pub(super) own: bool,
    /// Whether any of its definitions is in code the application loads. See [`Tally::loadable`].
    pub(super) loadable: bool,
    /// Whether any definition is inside the project at all. The one of the three that **drops**
    /// instead of ranking. See [`Fence::inside`].
    pub(super) inside: bool,
}

/// [`Placement`], in one pass.
pub(super) fn placement(
    graph: &Graph,
    declaration: &Declaration,
    own: &HashSet<UriId>,
    trees: &Trees<'_>,
) -> Placement {
    let mut tally = Tally::default();
    let mut elsewhere = Tally::default();
    let mut is_own = false;
    for definition in declaration
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
    {
        is_own |= own.contains(definition.uri_id());
        tally.saw(trees.holds(definition.uri_id()));
        elsewhere.saw(trees.is_outside(definition.uri_id()));
    }
    Placement {
        own: is_own,
        loadable: tally.loadable(),
        inside: elsewhere.loadable(),
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {

    /// [`fenced_from`]'s tree gate: the half `Names` alone decides.
    ///
    /// The other half needs a [`Layout`], and every test in this group is about directory names.
    fn fenced_by_a_tree(cursor: Option<&str>, names: Names<'_>) -> bool {
        fenced_from(
            cursor,
            Layout {
                names,
                ..Layout::default()
            },
        )
        .trees
    }

    use super::*;
    use crate::analysis::testing::*;

    /// One of the three lists, as a project would write it in `ya-lsp.toml`.
    fn named(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_project_that_keeps_its_suite_somewhere_else_replaces_the_list() {
        // **Replaces, never extends.** An appending key would invite someone to add `lib` and
        // silently delete their whole workspace from completion, definition and hover.
        let qa = named(&["qa"]);
        let moved = Names {
            test: Some(&qa),
            ..Names::default()
        };
        assert!(moved.in_a_test_tree("file:///p/qa/models/store_spec.rb"));
        assert!(
            !moved.in_a_test_tree("file:///p/spec/models/store_spec.rb"),
            "the built-in list is replaced, not added to"
        );

        // Unset is the built-in four, and that is what every caller with no configuration gets.
        assert!(Names::default().in_a_test_tree("file:///p/spec/models/store_spec.rb"));

        // An empty list turns the fence **off**, which is not the same as unset.
        let nothing: Vec<String> = Vec::new();
        let off = Names {
            test: Some(&nothing),
            ..Names::default()
        };
        assert!(!off.in_a_test_tree("file:///p/spec/models/store_spec.rb"));
    }

    #[test]
    fn the_cursor_list_is_added_to_and_the_built_in_name_survives_it() {
        // The safe list: a name here only turns the fence **off**, so being wrong costs only the
        // protection.
        let extra = named(&["fixtures"]);
        let widened = Names {
            support: &extra,
            ..Names::default()
        };
        assert!(!fenced_by_a_tree(
            Some("file:///p/fixtures/stories.rb"),
            widened
        ));
        assert!(
            !fenced_by_a_tree(
                Some("file:///p/core/lib/spree/testing_support/shared.rb"),
                widened
            ),
            "the built-in name is added to, never replaced"
        );
        // And an ordinary file is still fenced, so the list has not simply swallowed everything.
        assert!(fenced_by_a_tree(
            Some("file:///p/app/models/store.rb"),
            widened
        ));
    }

    #[test]
    fn a_migration_tree_somewhere_else_keeps_the_pair_and_the_substring() {
        // A `parent/mark` pair, because `migrate` alone is an ordinary word
        // (`app/services/migrate/`).
        let moved = named(&["lib/data_migrat"]);
        let elsewhere = Names {
            migration: Some(&moved),
            ..Names::default()
        };
        assert!(elsewhere.in_a_migration("file:///p/lib/data_migrations/backfill.rb"));
        assert!(
            elsewhere.in_a_migration("file:///p/lib/data_migrate/backfill.rb"),
            "the mark is a substring of the directory, exactly as `db/migrat` is"
        );
        assert!(
            !elsewhere.in_a_migration("file:///p/db/migrate/backfill.rb"),
            "replaced, not added to"
        );
        assert!(
            !elsewhere.in_a_migration("file:///p/app/services/migrate/run.rb"),
            "the parent is half the rule and stays half of it"
        );

        // The built-in pair, and the three spellings one rule covers.
        for uri in [
            "file:///p/db/migrate/x.rb",
            "file:///p/db/post_migrate/x.rb",
            "file:///p/db/old_migrations/x.rb",
        ] {
            assert!(Names::default().in_a_migration(uri), "{uri}");
        }

        // `[]` turns the fence off, which is a legitimate answer and not a mistake.
        let nothing: Vec<String> = Vec::new();
        let off = Names {
            migration: Some(&nothing),
            ..Names::default()
        };
        assert!(!off.in_a_migration("file:///p/db/migrate/x.rb"));

        // An entry with no parent is skipped, not guessed at. `config::validate` tells the user,
        // because a bare `migrate` would fence a tree they load.
        let bare = named(&["migrate"]);
        let ignored = Names {
            migration: Some(&bare),
            ..Names::default()
        };
        assert!(!ignored.in_a_migration("file:///p/db/migrate/x.rb"));
    }

    #[test]
    fn a_replaced_test_list_reaches_every_surface_that_fences_on_one() {
        // **Guards against a half-threaded fence.** The fence has broken this way before, when only
        // one reader got the layout. So one configured value must reach all four readers:
        // `fenced_from`, `Trees::of`, `completion`'s per-document loop and
        // `locator::preferred_definition`.
        let mut harness = Harness::configured("[trees]\ntest = [\"qa\"]\n");
        let app = harness.write("app/store.rb", ANCESTRY);
        harness.write(
            "qa/support/qa_helpers.rb",
            "def qa_only_helper\n  Store\nend\n",
        );
        harness.write("spec/support/helpers.rb", "def spec_only_helper\nend\n");
        harness.index();

        // **Dropped**, and by the name the project moved the fence onto.
        let offered = harness.suggestions(&app, &format!("{ANCESTRY}qa_only_hel~\n"));
        assert!(
            !offered.iter().any(|row| row == "qa_only_helper"),
            "{offered:?}"
        );

        // **And `spec/` is no longer fenced.** A key that only *added* `qa` would get this wrong,
        // which is why the list replaces.
        let offered = harness.suggestions(&app, &format!("{ANCESTRY}spec_only_hel~\n"));
        assert!(
            offered.iter().any(|row| row == "spec_only_helper"),
            "{offered:?}"
        );

        // **Ranked, never dropped**: the picker is the only way to look for a name, so a real
        // declaration stays findable.
        let found = harness.symbol_names("qa_only_helper");
        assert!(
            found.iter().any(|name| name.contains("qa_only_helper")),
            "{found:?}"
        );

        // **Never**: a use under a fenced tree is still a use, and omitting it makes a rename that
        // breaks the suite.
        let uses = harness.reference_list(&app, ANCESTRY, "Store", true);
        assert!(
            uses.iter().any(|row| row.contains("qa_helpers.rb")),
            "{uses:?}"
        );
    }

    #[test]
    fn replacing_a_fence_list_says_what_it_replaced() {
        // A replacement must be visible: whoever set this took the four standard directory names
        // out of play, and this log is the only place that shows it. `index.include` has the same
        // need, met by `messages::include_is_empty`.
        let (_, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            let mut harness = Harness::configured(
                "[trees]\ntest = [\"qa\"]\nmigration = [\"lib/data_migrat\"]\n",
            );
            harness.write("app/store.rb", "class Store\nend\n");
            harness.index();
        });
        assert!(logged.contains("trees.test is [\"qa\"]"), "{logged}");
        assert!(logged.contains("spec"), "the list it replaced: {logged}");
        assert!(logged.contains("trees.migration is"), "{logged}");
        assert!(logged.contains("db/migrat"), "{logged}");

        // A project that has not moved them logs nothing, `test_support` included: adding replaces
        // nothing, so there is nothing to report.
        let (_, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            let mut harness = Harness::configured("[trees]\ntest_support = [\"fixtures\"]\n");
            harness.write("app/store.rb", "class Store\nend\n");
            harness.index();
        });
        assert!(!logged.contains("trees."), "{logged}");
    }

    #[test]
    fn the_built_in_pair_is_the_one_the_setting_would_have_to_write() {
        // Three spellings of one rule: the two constants and the manifest's default for
        // `trees.migration`. This stops them drifting.
        assert_eq!(
            crate::workspace::config::migration_pair(MIGRATION_PAIR),
            Some((MIGRATION_ROOT, MIGRATION_MARK))
        );
    }
    #[test]
    fn the_tag_matches_a_path_segment_and_never_a_prefix() {
        // An engine monorepo keeps specs in `core/spec/` and a whole Rails app under `spec/dummy/`.
        // A prefix test would miss both.
        assert!(Names::default().in_a_test_tree("file:///p/spec/models/store_spec.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/core/spec/models/store_spec.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/spec/dummy/app/models/store.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/test/unit/store_test.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/features/step_definitions/a.rb"));

        assert!(!Names::default().in_a_test_tree("file:///p/app/models/store.rb"));
        // Three near misses: a longer directory name, a file starting with the word, and a file
        // *named* the word. The filename needs no case of its own because a Ruby file cannot equal
        // one of the four.
        assert!(!Names::default().in_a_test_tree("file:///p/specs/models/store.rb"));
        assert!(!Names::default().in_a_test_tree("file:///p/app/spec_helper_loader.rb"));
        assert!(!Names::default().in_a_test_tree("file:///p/app/spec.rb"));
    }

    #[test]
    fn a_cursor_the_graph_never_held_fences_nothing() {
        // No path is no evidence, so the fence stays off. Both dropping surfaces read this one
        // function, so they cannot answer differently.
        assert!(fenced_by_a_tree(
            Some("file:///p/app/models/store.rb"),
            Names::default()
        ));
        assert!(!fenced_by_a_tree(
            Some("file:///p/spec/models/store_spec.rb"),
            Names::default()
        ));
        assert!(!fenced_by_a_tree(None, Names::default()));
        // And neither does the other gate, for the same reason: a fence needs evidence to fire.
        assert!(!fenced_from(None, Layout::default()).outward.on());
    }

    #[test]
    fn only_this_project_s_own_test_trees_are_test_trees() {
        // The three clauses, and the library shapes that need them. The four names describe *the
        // project's* trees; anything indexed from outside is somebody else's published code:
        // rack-test's `lib/rack/test/utils.rb`, `rbs`' `sig/test/`, Ruby's minitest signatures
        // under `minitest/test/`.
        //
        // `Workspace::load_paths` order: the bundle, then the project's own `lib/` and `app/`.
        let load = vec![
            "file:///gems/rack-test-2.2.0/lib/".to_owned(),
            "file:///p/lib/".to_owned(),
        ];
        let layout = Layout {
            root: "file:///p/",
            load: &load,
            names: Names::default(),
            own: &[],
            foreign: &[],
        };
        let fence = Fence::at(Some("file:///p/app/models/store.rb"), layout);
        assert!(fence.on_trees());
        assert!(!fence.only_the_suite("file:///gems/rack-test-2.2.0/lib/rack/test/utils.rb"));
        assert!(
            !fence.only_the_suite("file:///gems/rbs-4.2.0/sig/test/errors.rbs"),
            "a gem's signatures are on no load path, and are still not this project's suite"
        );
        assert!(!fence.only_the_suite("file:///rbs/stdlib/minitest/0/minitest/test/hooks.rbs"));
        assert!(!fence.only_the_suite("file:///p/app/models/store.rb"));
        assert!(
            fence.only_the_suite("file:///p/spec/models/store_spec.rb"),
            "the project's own trees are inside it and on no load path: fenced as before"
        );
        assert!(
            !fence.only_the_suite("file:///p/lib/tasks/test/seed.rb"),
            "the project's own `lib/` is a load path, so `require` reaches this"
        );

        // **A generated declaration is judged by the file that implied it.** Its uri is the
        // source's with `ya-lsp-generated:` in front, which is not a path, so every prefix clause
        // reads it as nowhere unless the scheme comes off first. Without the strip, a `Data.define`
        // in a spec file stops being the suite's.
        assert!(fence.only_the_suite("ya-lsp-generated:file:///p/spec/models/store_spec.rb"));
        assert!(!fence.only_the_suite("ya-lsp-generated:file:///p/app/models/store.rb"));
        assert!(!fence.only_the_suite(
            "ya-lsp-generated:file:///gems/rack-test-2.2.0/lib/rack/test/utils.rb"
        ));

        // An empty layout: before the bundle is discovered, or a test with no opinion about roots.
        // The path tag decides alone, which fences more.
        let bare = Fence::at(Some("file:///p/app/models/store.rb"), Layout::default());
        assert!(bare.only_the_suite("file:///gems/rack-test-2.2.0/lib/rack/test/utils.rb"));

        // The cursor turns it off, and `resolve` turns it off in writing.
        assert!(!Fence::at(Some("file:///p/spec/models/store_spec.rb"), layout).on_trees());
        assert!(!Fence::at(None, layout).on_trees());
        assert!(
            !Fence::uses(Some("file:///p/app/models/store.rb"), layout).on_trees(),
            "`resolve` turns the tree gate off in writing, and only that one"
        );
    }

    #[test]
    fn a_migration_is_a_tree_under_db_and_the_spelling_is_not_a_fixed_list() {
        // The three spellings real projects write: in a project, in a plugin's own `db/`, and in an
        // engine shipped as a gem. A list of names would have missed `old_migrations`.
        assert!(
            Names::default()
                .in_a_migration("file:///p/db/migrate/20180528141303_fix_accounts_unique_index.rb")
        );
        assert!(Names::default().in_a_migration(
            "file:///p/db/post_migrate/20221101190723_backfill_admin_action_logs.rb"
        ));
        assert!(
            Names::default()
                .in_a_migration("file:///p/db/old_migrations/20200809023435_create_categories.rb")
        );
        assert!(Names::default().in_a_migration(
            "file:///p/plugins/poll/db/migrate/20180820080623_migrate_polls_data.rb"
        ));
        assert!(
            Names::default()
                .in_a_migration("file:///g/spree-4.4.0/db/migrate/20210101000000_add_column.rb")
        );

        // The parent segment makes it Rails' tree and not an ordinary word. Both of these are
        // autoloaded application code.
        assert!(!Names::default().in_a_migration("file:///p/app/services/migrate/runner.rb"));
        assert!(!Names::default().in_a_migration("file:///p/lib/migrations/step.rb"));

        // The loaded neighbours under `db/`, which every Rails project has.
        assert!(!Names::default().in_a_migration("file:///p/db/schema.rb"));
        assert!(!Names::default().in_a_migration("file:///p/db/seeds.rb"));
        assert!(!Names::default().in_a_migration("file:///p/db/views/story.rb"));

        // A *file* directly under `db/` with the word in its name is a loader, not a tree. This
        // clause keeps the rule about directories.
        assert!(!Names::default().in_a_migration("file:///p/db/migration_helpers.rb"));

        // And nothing to make a pair out of.
        assert!(!Names::default().in_a_migration("db"));
        assert!(!Names::default().in_a_migration(""));
    }

    #[test]
    fn a_migration_is_unloadable_wherever_it_ships_from_unless_require_can_name_it() {
        // An empty `Layout`, before the bundle is discovered. The suite clause falls back to the
        // path tag alone here; this rule has no clause to lose, like a generator template.
        let bare = Fence::at(Some("file:///p/app/models/story.rb"), Layout::default());
        assert!(bare.unloadable("file:///p/db/migrate/20180528141303_fix_index.rb"));
        assert!(!bare.unloadable("file:///p/db/schema.rb"));

        // **The generated document is the one that matters.** The `belongs_to` and `has_many`
        // members a reader reaches are generated, and filed under the scheme in front of the
        // migration's uri. The suite clause once had exactly this bug.
        assert!(
            bare.unloadable("ya-lsp-generated:file:///p/db/migrate/20180528141303_fix_index.rb")
        );
        assert!(!bare.unloadable("ya-lsp-generated:file:///p/app/models/story.rb"));

        // An engine's migrations, outside the workspace and on a load path. A spec there would be a
        // library; a migration there is still a migration. No root clause, unlike the suite rule.
        let load = vec!["file:///gems/spree-4.4.0/lib/".to_owned()];
        let layout = Layout {
            root: "file:///p/",
            load: &load,
            names: Names::default(),
            own: &[],
            foreign: &[],
        };
        let fence = Fence::at(Some("file:///p/app/models/story.rb"), layout);
        assert!(fence.unloadable("file:///gems/spree-4.4.0/db/migrate/20210101_add.rb"));

        // **The escape hatch.** The tag is a substring under `db/`, so it catches
        // `db/data_migrations/` whether or not the project autoloads it. Putting it on
        // `[index] load_paths` says `require` reaches it.
        let declared = vec!["file:///p/db/data_migrations/".to_owned()];
        let asked = Fence::at(
            Some("file:///p/app/models/story.rb"),
            Layout {
                root: "file:///p/",
                load: &declared,
                names: Names::default(),
                own: &[],
                foreign: &[],
            },
        );
        assert!(!asked.unloadable("file:///p/db/data_migrations/backfill_stories.rb"));
        assert!(
            asked.unloadable("file:///p/db/migrate/20180528141303_fix_index.rb"),
            "and it says nothing about the tree next door"
        );
    }

    #[test]
    fn a_cursor_inside_a_migration_turns_the_fence_off() {
        // The asymmetry again: a wrong name on the target list deletes an answer, while a name on
        // the cursor list only drops protection. Inside `class FixAccountsUniqueIndex`, Ruby
        // resolves the constant to the copy declared at the top of that file, so a reader there is
        // exactly who it is for. That is a lexical question, not a path rule's.
        assert!(!fenced_by_a_tree(
            Some("file:///p/db/migrate/20180528141303_fix_accounts_unique_index.rb"),
            Names::default()
        ));
        assert!(fenced_by_a_tree(
            Some("file:///p/db/schema.rb"),
            Names::default()
        ));
        assert!(fenced_by_a_tree(
            Some("file:///p/app/models/story.rb"),
            Names::default()
        ));
    }

    #[test]
    fn a_generator_s_template_is_copied_and_never_loaded_by_anybody() {
        // The pair, in order, and each half alone. Four shapes: pundit's, rpush's (nothing between
        // the names), railties' (four segments between) and a project's own.
        assert!(in_a_generator_template(
            "file:///g/pundit-2.3.1/lib/generators/pundit/install/templates/application_policy.rb"
        ));
        assert!(in_a_generator_template(
            "file:///g/rpush-9.2.0/lib/generators/templates/add_rpush.rb"
        ));
        assert!(in_a_generator_template(
            "file:///g/railties-8.0.5/lib/rails/generators/rails/app/templates/config.ru"
        ));
        assert!(in_a_generator_template(
            "file:///p/lib/generators/store/install/templates/store.rb"
        ));

        // `templates` alone deletes a real library: `YARD::Templates::Engine` is a class people
        // call. `generators` alone deletes the generator, which `rails generate` requires.
        assert!(!in_a_generator_template(
            "file:///g/yard-0.9.45/lib/yard/templates/engine.rb"
        ));
        assert!(!in_a_generator_template(
            "file:///g/pundit-2.3.1/lib/generators/pundit/install/install_generator.rb"
        ));

        // Order matters, and the shared iterator enforces it: `templates` must come *after*
        // `generators`.
        assert!(!in_a_generator_template(
            "file:///p/lib/templates/generators/thing.rb"
        ));
        // Segments, like every rule here: a longer name, and a file named one of them.
        assert!(!in_a_generator_template(
            "file:///p/lib/generators/app_templates/none.rb"
        ));
        assert!(!in_a_generator_template(
            "file:///p/lib/generators/templates.rb"
        ));
    }

    #[test]
    fn one_walk_of_a_path_answers_both_tree_rules_exactly_as_the_two_of_them_did() {
        // The combined walk runs over every document on each completion, so each path is walked
        // once. The answers must not change: every shape either rule already tests, checked against
        // both at once.
        let names = Names::default();
        for uri in [
            "file:///g/pundit-2.3.1/lib/generators/pundit/install/templates/application_policy.rb",
            "file:///g/rpush-9.2.0/lib/generators/templates/add_rpush.rb",
            "file:///g/railties-8.0.5/lib/rails/generators/rails/app/templates/config.ru",
            "file:///g/yard-0.9.45/lib/yard/templates/engine.rb",
            "file:///p/lib/templates/generators/thing.rb",
            "file:///p/lib/generators/app_templates/none.rb",
            "file:///p/lib/generators/templates.rb",
            "file:///p/db/migrate/20180528141303_fix_accounts_unique_index.rb",
            "file:///p/db/post_migrate/20221101190723_backfill.rb",
            "file:///p/db/old_migrations/20200809023435_create_categories.rb",
            "file:///p/plugins/poll/db/migrate/20180820080623_migrate_polls_data.rb",
            "file:///g/spree-4.4.0/db/migrate/20210101000000_add_column.rb",
            "file:///p/app/services/migrate/runner.rb",
            "file:///p/lib/migrations/step.rb",
            "file:///p/db/schema.rb",
            "file:///p/db/migration_helpers.rb",
            "file:///p/app/models/story.rb",
            "db",
            "",
        ] {
            assert_eq!(
                in_an_unloadable_tree(names, uri),
                in_a_generator_template(uri) || names.in_a_migration(uri),
                "{uri}"
            );
        }

        // The migration pair's `[trees]` override reaches the combined walk too: a project that
        // replaced the pair keeps `db/migrate` loadable.
        let replaced = vec!["lib/data_migrations".to_owned()];
        let elsewhere = Names {
            migration: Some(&replaced),
            ..Names::default()
        };
        assert!(in_an_unloadable_tree(
            elsewhere,
            "file:///p/lib/data_migrations/backfill.rb"
        ));
        assert!(!in_an_unloadable_tree(
            elsewhere,
            "file:///p/db/migrate/backfill.rb"
        ));
        // The template half has no `[trees]` key, so it answers the same either way.
        assert!(in_an_unloadable_tree(
            elsewhere,
            "file:///p/lib/generators/store/install/templates/store.rb"
        ));
    }

    #[test]
    fn a_template_is_unloadable_whoever_ships_it_and_needs_no_layout_to_say_so() {
        // Generator templates: the second reason a document never loads, and one the tag answers
        // without asking whose tree it is. A template is the same in a gem and in the project, so
        // an empty layout answers identically.
        let load = vec![
            "file:///g/pundit-2.3.1/lib/".to_owned(),
            "file:///p/lib/".to_owned(),
        ];
        let layout = Layout {
            root: "file:///p/",
            load: &load,
            names: Names::default(),
            own: &[],
            foreign: &[],
        };
        let template =
            "file:///g/pundit-2.3.1/lib/generators/pundit/install/templates/application_policy.rb";
        for fence in [
            Fence::at(Some("file:///p/app/models/store.rb"), layout),
            Fence::at(Some("file:///p/app/models/store.rb"), Layout::default()),
        ] {
            assert!(
                fence.unloadable(template),
                "under the gem's load path, inside no workspace, and still copied rather than run"
            );
            assert!(
                !fence.only_the_suite(template),
                "and not because it is a suite"
            );
            assert!(!fence.unloadable("file:///p/app/policies/application_policy.rb"));
        }

        // The generated scheme comes off here too: this is a prefix rule, and a generated uri is
        // not a path. Templates generate nothing today; the strip keeps that from being the reason
        // this passes.
        let fence = Fence::at(Some("file:///p/app/models/store.rb"), layout);
        assert!(fence.unloadable(&synthesized::generated_uri(
            &DocUri::from_graph_uri(template).expect("a file uri"),
            "class:Thing"
        )));
        assert!(!fence.unloadable(&synthesized::generated_uri(
            &DocUri::from_graph_uri("file:///p/app/models/store.rb").expect("a file uri"),
            "class:Store"
        )));
    }

    #[test]
    fn the_rule_is_any_definition_the_application_loads_and_no_definitions_at_all() {
        // The three cases, where they are decided. The middle one is the fence. The last is the bug
        // the count prevents: a running `bool` starting at *not loadable* never flips for `Object`,
        // which rubydex declares with no definitions, so the top of the object model would drop out
        // of every list.
        let mut only_the_application = Tally::default();
        only_the_application.saw(false);
        assert!(only_the_application.loadable());

        let mut only_the_suite = Tally::default();
        only_the_suite.saw(true);
        only_the_suite.saw(true);
        assert!(!only_the_suite.loadable());

        let mut both = Tally::default();
        both.saw(true);
        both.saw(false);
        assert!(
            both.loadable(),
            "a class the suite reopens is still the application's"
        );

        assert!(Tally::default().loadable(), "no definitions is no evidence");
    }

    #[test]
    fn a_class_only_a_spec_declares_is_the_project_s_and_still_not_loadable() {
        // The two halves of `Placement` are independent, and a test double separates them. It is
        // the user's own code, so the picker ranks it above every gem. The application never loads
        // it, so it ranks below the application's own.
        let mut harness = Harness::new();
        harness.write("app/models/store.rb", "class Store\nend\n");
        harness.write(
            "spec/models/store_spec.rb",
            "class Store\n  def reopened_by_the_suite\n  end\nend\n\nclass FakeStore\nend\n",
        );
        harness.index();

        let graph = &harness.analysis.graph;
        let own = harness.analysis.own_documents();
        let nowhere = HashSet::new();
        let trees = Trees::of(graph, own, &nowhere, Names::default());
        let placed = |name: &str| {
            let declaration = graph
                .declarations()
                .iter()
                .find(|(_, declaration)| declaration.name() == name)
                .unwrap_or_else(|| panic!("{name} is not in the graph"))
                .1;
            placement(graph, declaration, own, &trees)
        };

        let store = placed("Store");
        assert!(store.own);
        assert!(
            store.loadable,
            "written in both is written in the application"
        );

        let double = placed("FakeStore");
        assert!(double.own, "a spec is the user's own code");
        assert!(!double.loadable);
    }

    #[test]
    fn the_second_meaning_is_a_prefix_because_no_directory_name_can_say_it() {
        // Four prefix lists, covering every place the project reaches. A tree rule asks what a
        // directory is *called*; this one can only ask where it *is*, because a project's own
        // `scratch/` sits inside the root.
        let load = named(&["file:///p/lib/", "file:///gems/rack-2.2.0/lib/"]);
        let own = named(&["file:///shared/lib/"]);
        let foreign = named(&["file:///gems/rack-2.2.0/", "file:///rbs/"]);
        let layout = Layout {
            root: "file:///p/",
            load: &load,
            names: Names::default(),
            own: &own,
            foreign: &foreign,
        };

        assert!(
            layout.is_outside("file:///elsewhere/scratch_pad.rb"),
            "a file the user opened beside their project reaches none of the four"
        );
        assert!(!layout.is_outside("file:///p/app/models/store.rb"));
        assert!(
            !layout.is_outside("file:///shared/lib/money.rb"),
            "an `[index] load_paths` entry outside the root is the project's own code"
        );
        assert!(
            !layout.is_outside("file:///gems/rack-2.2.0/lib/rack.rb"),
            "a gem is not the user's own and is a full member of every answer about the project"
        );
        assert!(!layout.is_outside("file:///rbs/core/string.rbs"));
        assert!(
            layout.is_outside("file:///p-elsewhere/thing.rb"),
            "a sibling whose name merely starts the same way is outside: the prefixes are \
             directory URIs, which is what stops `/app` swallowing `/app-vendor`"
        );

        // The scheme comes off first, for `only_the_suite`'s reason: a generated uri is not a path,
        // so every prefix clause would read it as nowhere, which is exactly what *outside* means
        // here.
        assert!(!layout.is_outside("ya-lsp-generated:file:///p/app/models/store.rb"));
        assert!(layout.is_outside("ya-lsp-generated:file:///elsewhere/scratch_pad.rb"));

        // An empty root: before the workspace is known, or a test with no opinion. It fences
        // **nothing**, which is the right direction for a new fence to fail.
        assert!(!Layout::default().is_outside("file:///elsewhere/scratch_pad.rb"));
    }

    #[test]
    fn the_two_gates_are_turned_off_by_different_cursors_and_never_by_each_other() {
        // Why `Gates` has two fields. A reader in a scratch file wants their own file's names, not
        // every spec; a reader in a spec wants no scratch buffer's names.
        let layout = Layout {
            root: "file:///p/",
            ..Layout::default()
        };

        let app = fenced_from(Some("file:///p/app/models/store.rb"), layout);
        assert!(app.trees, "both, for an ordinary cursor");
        assert!(matches!(app.outward, Outward::Project));

        let spec = fenced_from(Some("file:///p/spec/models/store_spec.rb"), layout);
        assert!(
            !spec.trees,
            "a developer editing a spec is who a spec answers"
        );
        assert!(
            matches!(spec.outward, Outward::Project),
            "and has said nothing about a file outside the project"
        );

        let loose = fenced_from(Some("file:///elsewhere/scratch_pad.rb"), layout);
        assert!(
            matches!(
                loose.outward,
                Outward::Alone("file:///elsewhere/scratch_pad.rb")
            ),
            "its own cursor names itself rather than switching the meaning off"
        );
        assert!(
            loose.trees,
            "and it is still not a spec, so the tree rules go on applying"
        );

        // `resolve`'s pair, the one place the two gates are set apart by hand: `references` and
        // `rename` must find a use under `spec/` and must not reach outside the project.
        let uses = Fence::uses(Some("file:///p/app/models/store.rb"), layout);
        assert!(!uses.on_trees());
        assert!(uses.outside("file:///elsewhere/scratch_pad.rb"));

        // From inside the scratch file, the same surface reaches **it and nothing else outside**.
        // The gate stays on and the exception is one document wide. Switching the gate off would
        // hand this cursor every other loose file and unsaved buffer.
        let from_there = Fence::uses(Some("file:///elsewhere/scratch_pad.rb"), layout);
        assert!(!from_there.outside("file:///elsewhere/scratch_pad.rb"));
        assert!(!from_there.outside("file:///p/app/models/store.rb"));
        assert!(
            from_there.outside("file:///elsewhere/other_scratch.rb"),
            "no crosslinking: the file next to it is as outside as it ever was"
        );
        assert!(
            from_there.outside("untitled:Untitled-1"),
            "and an unsaved buffer is outside every project, including from out here"
        );

        // The same three answers from the unsaved buffer's side: `Layout::is_outside` is a prefix
        // test, and `untitled:` starts with none of the prefixes.
        let unsaved = Fence::uses(Some("untitled:Untitled-1"), layout);
        assert!(matches!(
            fenced_from(Some("untitled:Untitled-1"), layout).outward,
            Outward::Alone("untitled:Untitled-1")
        ));
        assert!(!unsaved.outside("untitled:Untitled-1"));
        assert!(!unsaved.outside("file:///p/app/models/store.rb"));
        assert!(
            unsaved.outside("untitled:Untitled-2"),
            "two unsaved buffers are two documents, and neither is the other's project"
        );
    }

    #[test]
    fn a_scratch_file_beside_the_project_reads_it_and_is_a_member_of_nothing() {
        // Both halves of one rule, end to end. The inward half needs no special code:
        // `Task::DidOpen` hands any document to the indexer, and every rung reads the graph. It is
        // asserted so the outward half cannot be built by closing the door instead.
        let mut harness = Harness::new();
        harness.write(
            "app/models/store.rb",
            "class Store\n  def restock\n  end\nend\n",
        );
        let caller_source = concat!(
            "class Caller\n",
            "  def run\n",
            "    scratch_pad_only\n",
            "  end\n",
            "\n",
            "  def build\n",
            "    Store.new\n",
            "  end\n",
            "end\n",
        );
        let caller = harness.write("app/models/caller.rb", caller_source);
        harness.index();

        // Outside the workspace root by construction: a second temporary directory, which is
        // what a file open in the next editor tab really is.
        let beside = tempfile::tempdir().unwrap();
        let path = beside.path().join("scratch_pad.rb");
        let loose_source = concat!(
            "class ScratchPad\n",
            "  def scratch_pad_only\n",
            "    Store.new.restock\n",
            "  end\n",
            "\n",
            "  def again\n",
            "    ScratchPad.new\n",
            "  end\n",
            "end\n",
            "\n",
            "class Store\n",
            "  def scratch_only\n",
            "  end\n",
            "end\n",
        );
        std::fs::write(&path, loose_source).unwrap();
        let loose = crate::workspace::DocUri::from_path(&path).unwrap();
        harness.open(&loose, loose_source);

        // Inward: it reads the project, which is what a user opening a scratch file wants.
        assert_eq!(
            linked(&harness.definition_at(&loose, loose_source, "Store.new")),
            ["store.rb:0:6", "scratch_pad.rb:10:6"],
            "a scratch file resolves the project's own classes, and sees its own reopen of one \
             — which is the cursor rule, not an accident: the fence is on for every cursor but \
             this document's"
        );
        assert!(
            !harness
                .ask(
                    "textDocument/documentSymbol",
                    serde_json::json!({ "textDocument": { "uri": loose.as_str() } })
                )
                .is_null(),
            "and answers about itself"
        );

        // Outward: the project sees none of it.
        assert!(
            harness.symbol_names("ScratchPad").is_empty(),
            "a file the workspace does not contain is not one of its symbols"
        );
        assert!(harness.symbol_names("scratch_pad_only").is_empty());
        assert_eq!(
            harness.symbol_names("Store"),
            ["Store", "Store#restock"],
            "and the project's own rows are untouched — the scratch file reopens `Store` and \
             neither adds a member to its list nor moves where its row points"
        );
        assert!(harness.symbol_names("scratch_only").is_empty());
        assert_eq!(
            linked(&harness.definition_at(&caller, caller_source, "Store.new")),
            ["store.rb:0:6"],
            "and the same jump from inside the project sees only the project's own place"
        );

        // The name rung, where this was worst: a jump out of `app/` into a buffer the application
        // cannot load.
        assert!(
            linked(&harness.definition_at(&caller, caller_source, "scratch_pad_only")).is_empty(),
            "nothing in the project declares it, and the scratch file is not in the project"
        );

        // The cursor rule, which makes the fence asymmetric: inside the scratch file, its own name
        // is still the answer.
        assert_eq!(
            linked(&harness.definition_at(&loose, loose_source, "ScratchPad.new")),
            ["scratch_pad.rb:0:6"]
        );

        // Completion, the third surface and the one a user meets first. Same pair of answers:
        // nothing from `app/`, and the name from the scratch file itself.
        assert!(
            !harness
                .suggestions(
                    &caller,
                    &caller_source.replace("scratch_pad_only", "scratch_pad_onl~")
                )
                .contains(&"scratch_pad_only".to_owned()),
            "a name only a document outside the project declares is not offered inside it"
        );
        assert!(
            harness
                .suggestions(
                    &loose,
                    &loose_source.replace("ScratchPad.new", "scratch_pad_onl~")
                )
                .contains(&"scratch_pad_only".to_owned()),
            "and is offered to the cursor that is in it"
        );
    }

    #[test]
    fn an_unsaved_buffer_reaches_the_project_and_itself_and_no_other_buffer() {
        // The door, end to end: **the project's code and itself, and no crosslinking.** A buffer
        // with no file is the scratch file above minus a path, so the same halves are asserted,
        // plus one more: the buffer in the next tab is as outside as the project is.
        let mut harness = Harness::new();
        harness.write(
            "app/models/store.rb",
            "class Store\n  def restock\n  end\nend\n",
        );
        let caller_source = concat!(
            "class Caller\n",
            "  def run\n",
            "    untitled_only\n",
            "  end\n",
            "end\n",
        );
        let caller = harness.write("app/models/caller.rb", caller_source);
        harness.index();

        let untitled = |name: &str| {
            crate::workspace::DocUri::from_lsp(&name.parse().expect("a uri"))
                .expect("an unsaved buffer is a document")
        };
        let first = untitled("untitled:Untitled-1");
        let first_source = concat!(
            "class Draft\n",
            "  def untitled_only\n",
            "    Store.new.restock\n",
            "  end\n",
            "\n",
            "  def crosses\n",
            "    other_only\n",
            "  end\n",
            "\n",
            "  def calls_itself\n",
            "    untitled_only\n",
            "  end\n",
            "end\n",
        );
        harness.open(&first, first_source);

        let second = untitled("untitled:Untitled-2");
        let second_source = concat!(
            "class OtherDraft\n",
            "  def other_only\n",
            "  end\n",
            "end\n"
        );
        harness.open(&second, second_source);

        // Inward: it reads the project, which is the whole reason the door is worth opening.
        assert_eq!(
            linked(&harness.definition_at(&first, first_source, "Store.new")),
            ["store.rb:0:6"],
            "an unsaved buffer resolves the project's own classes"
        );
        assert!(
            !harness
                .ask(
                    "textDocument/documentSymbol",
                    serde_json::json!({ "textDocument": { "uri": first.as_str() } })
                )
                .is_null(),
            "and answers about itself"
        );

        // Outward: the project sees none of it, exactly as it sees none of a loose file.
        assert!(harness.symbol_names("Draft").is_empty());
        assert!(harness.symbol_names("untitled_only").is_empty());
        assert!(
            linked(&harness.definition_at(&caller, caller_source, "untitled_only")).is_empty(),
            "a jump out of `app/` into a buffer the user has not saved is the defect, not a \
             degraded answer"
        );
        assert!(
            !harness
                .suggestions(
                    &caller,
                    &caller_source.replace("untitled_only", "untitled_onl~")
                )
                .contains(&"untitled_only".to_owned())
        );

        // **Sideways: nothing.** One bit could not say this: switching the gate off for a cursor
        // that is itself outside would hand this buffer every other one in the window.
        assert!(
            linked(&harness.definition_at(&first, first_source, "other_only")).is_empty(),
            "the buffer in the next tab is as outside as the project is"
        );
        assert!(
            !harness
                .suggestions(&first, &first_source.replace("other_only", "other_onl~"))
                .contains(&"other_only".to_owned()),
            "and is not offered either"
        );

        // But its own names are answers from its own cursor. That is what the one-document
        // exception is for.
        assert!(
            harness
                .suggestions(&first, &first_source.replace("other_only", "untitled_onl~"))
                .contains(&"untitled_only".to_owned())
        );

        // **The two hierarchies need more than the rule.** Both `prepare`s go through
        // `locator::preferred_definition`, which must consult the cursor: otherwise a class the
        // buffer itself declares has no row to expand, even with the cursor inside it.
        let prepared = harness.ask(
            "textDocument/prepareTypeHierarchy",
            serde_json::json!({
                "textDocument": { "uri": first.as_str() },
                "position": position_of(first_source, "Draft"),
            }),
        );
        assert_eq!(
            prepared
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("Draft"),
            "a type hierarchy rooted in the buffer answers about the buffer: {prepared}"
        );
        let calls = harness.ask(
            "textDocument/prepareCallHierarchy",
            serde_json::json!({
                "textDocument": { "uri": first.as_str() },
                "position": position_of(first_source, "untitled_only"),
            }),
        );
        assert_eq!(
            calls
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("Draft#untitled_only"),
            "and so does a call hierarchy: {calls}"
        );

        // **The two *where is this used* lists are scoped, not fenced.** Both read the project's
        // own documents, so a use written in the buffer would never be a candidate. The scope is
        // the project's documents plus the one the question comes from, and no other.
        let uses = harness.ask(
            "textDocument/references",
            serde_json::json!({
                "textDocument": { "uri": first.as_str() },
                "position": position_of(first_source, "untitled_only"),
                "context": { "includeDeclaration": false },
            }),
        );
        assert_eq!(
            uses.as_array().map(Vec::len),
            Some(1),
            "the buffer's own use of its own method: {uses}"
        );

        let incoming = harness.ask(
            "callHierarchy/incomingCalls",
            serde_json::json!({
                "item": calls.as_array().and_then(|items| items.first()).expect("a row"),
            }),
        );
        assert_eq!(
            incoming
                .as_array()
                .and_then(|calls| calls.first())
                .and_then(|call| call.get("from"))
                .and_then(|item| item.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("Draft#calls_itself"),
            "and the call written in the buffer is a caller: {incoming}"
        );

        // A buffer with no file also gets syntax errors published, like a scratch file: a missing
        // `end` with no red shows less than the editor's own grammar does.
        let broken = untitled("untitled:Untitled-3");
        harness.open(&broken, "class Half\n  def open\n");
        assert!(
            harness
                .latest(&broken)
                .is_some_and(|items| !items.is_empty()),
            "an unsaved buffer is squiggled"
        );
    }

    #[test]
    fn a_gem_is_neither_the_user_s_nor_anything_this_rule_judges() {
        // `own` is the only set that knows where *this project's* test trees are. A gem shipping
        // `test/` inside its `lib/` is not this rule's business, and that shows up as a miss in
        // `Trees`, not as a path test. So `Trees::of` is built from `own`, not from the graph's
        // documents.
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness =
            Harness::at_with_env(dir, crate::analysis::position::PositionEncoding::Utf16, env);
        harness.write("app/models/store.rb", "class Store\nend\n");
        harness.index();
        harness.index_gems();

        let graph = &harness.analysis.graph;
        let own = harness.analysis.own_documents();
        let nowhere = HashSet::new();
        let trees = Trees::of(graph, own, &nowhere, Names::default());
        let (_, megaphone) = graph
            .declarations()
            .iter()
            .find(|(_, declaration)| declaration.name() == "Shouty::Megaphone")
            .expect("the gem's class");
        let placed = placement(graph, megaphone, own, &trees);
        assert!(!placed.own);
        assert!(placed.loadable, "a gem is never fenced by this rule");
    }
}
