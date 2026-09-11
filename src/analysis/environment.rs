//! Whether the application can load a document at all, and what each surface may do about it.
//!
//! A Ruby project has two environments — the one the program runs in and the one the suite runs
//! in — and for a long time that was the whole of this module. A `def` under `spec/` exists only
//! while RSpec has loaded it, so offering it to a cursor in a model is offering a name that
//! cannot be called from where the cursor is: the same thing `reachable` refuses a private
//! method for, arrived at from the other direction.
//!
//! **There is a third kind of file and it is in neither environment.** A generator's template
//! tree is Ruby a gem ships in order to *copy* it, one day, into a project that does not exist
//! yet — not loaded by the program, not loaded by the suite, and not loaded by the gem that
//! ships it. So the question every surface here asks is not *which environment* but **can the
//! application load this at all**, and the suite is one of two answers rather than the only one.
//! [`Fence::unloadable`] is where they meet.
//!
//! **And a fourth, whose environment lasts one process.** A migration is loaded by path, alone,
//! by the task that runs it, so the backfill copy of a model somebody wrote inside one is
//! reachable from nothing — see [`in_a_migration`]. Each of these arrived the same way: not as a
//! new mechanism, but as one more tree the list did not name.
//!
//! rubydex models one graph and no environments. A `Document` has a uri and nothing else to
//! ask, so the tag is read off the path and nothing else, which is [`in_a_test_tree`].
//!
//! # Three verdicts, and the third is the one with teeth
//!
//! **Drop** — `completion`; the name-based rung of `definition` and `hover`
//! ([`locator::loadable_from`](super::locator)); the **root** rung of the same two, plus
//! `signatureHelp` and the call hierarchy's outgoing callee, which reach it through
//! `locator::resolve_call`'s root arm; and the **place list** those two answer from
//! ([`locator::places`](super::locator)). All of them answer *what can I call from here, and
//! where is it*, and a name the application cannot load is not an answer to that question at all.
//! Sinking it instead would leave it holding a slot under `MAX_COMPLETION_ITEMS` and leave the
//! name counting against `by_name`'s candidate ceiling, so the rows it buried would be the real
//! ones.
//!
//! **The place list is the one that judges a *definition* rather than a declaration**, and the
//! difference matters: `loadable` asks whether a name exists anywhere outside the test trees,
//! which is a question about the name; a place list is asked whether *this file* is somewhere to
//! send a reader. Ruby reopens freely, so a namespace collects a definition per file that ever
//! touched it — solidus offers **539** places for `Spree`, 76 of them under `spec/` — and the
//! reader in a controller loads none of those. Measured over 4,501 constant cursors outside the
//! test trees: **413 positions carried one, 24,143 places dropped, 0 positions emptied**, and the
//! audit's *Resolved cards land in a test tree* went **44 to 0** over all six corpora.
//!
//! **The root rung is the one that had to be argued for, because it is *precise*.** A member
//! found on `Object`, `Module` or `Class` is a member found on every receiver in the workspace,
//! and a `def` at the top level of a spec — or inside an `RSpec.describe` block, which rubydex
//! records identically — lands on exactly those. So the tier says *Resolved* and the answer is a
//! method the application could never call. Measured over the six corpora at 800 bare calls each,
//! outside the test trees: **31 of 4,251 answered cursors resolved this way and all 31 were
//! wrong** — sixteen migrations' `execute`, eleven service objects' `model`, `policy` and
//! `params`, `Post#cook`. Fencing it is therefore not a new policy; it is the same policy
//! reaching the one rung that was exempt by accident.
//!
//! **Where the jump falls through and the card does not.** `definition` and `hover` fall back to
//! the name rung, which is itself fenced, so what the reader gets is the answer the workspace
//! would have given if the spec's `def` had never been written — that is the test of a
//! declaration correctly removed, and it is sometimes a long list rather than a good answer.
//! `signatureHelp` and `outgoingCalls` have no such rung: an exact callee is the whole of their
//! contract, so the fenced case draws nothing.
//!
//! **Rank** — `workspace/symbol` and the type hierarchy's subtypes. Both answer *find me this*:
//! the user typed the name or clicked the class, and a drop would make a declaration that really
//! exists unfindable by the only means there is of looking for it. Neither request carries a
//! position either — `workspace/symbol` has no document at all and a subtypes call carries the
//! *subject's* uri rather than the reader's — so there is no cursor to turn a fence off with,
//! and a rank needs none: it costs nothing when it is wrong. What it buys is the cap. Ordering
//! is the whole feature in both, and a hundred test doubles ahead of the real subclasses is the
//! same failure as an unranked list.
//!
//! **Never** — `references`, `rename`, `documentHighlight` and the call hierarchy's incoming
//! calls. These answer *where is this used*, and a use under `spec/` is a use. A work list that
//! quietly omitted the suite is a rename that breaks it, which is the same reason those requests
//! do not follow a derived receiver either. `supertypes` never asks for a different reason: a
//! module a spec prepends really is in the chain, and the chain is what Ruby reports.
//!
//! A fourth thing is neither: [`locator::preferred_definition`](super::locator) picks *which*
//! of a declaration's places a single row points at, and there the tag breaks a tie rather than
//! deciding whether the row exists.
//!
//! **`references` and `rename` reach a place list too, and they ask
//! [`locator::sites`](super::locator) instead** — the list before any of this is applied. That is
//! the *never* row holding at the one point where the two kinds of list touch.
//!
//! **Every other request has no question to ask, and the table is exhaustive on purpose.** The
//! single-document answers — the outline, folding, the selection, the tokens, the hints, the
//! links, the code actions, the diagnostics — are about the file the cursor is in, so a spec's
//! answers are the spec's and there is nothing to leave. Add a surface to the table above rather
//! than to a new fence somewhere else.
//!
//! # A library is not a suite, and the path alone cannot say which
//!
//! [`in_a_test_tree`] is four directory names read off a path, and it was written against *the
//! project's* trees. A gem shipping `lib/rack/test/` is publishing a library, and `railties` puts
//! `rails/commands/test/` there. Calling either of those test-only deletes a real answer, and for
//! a while this module did: only [`locator::places`](super::locator) was handed the load paths,
//! so a method whose one definition is rack-test's `lib/rack/test/utils.rb` answered **nothing at
//! all** from the name rung while the same file one directory up answered. 128 gem files across
//! the six corpora carry such a segment and appear in real answers.
//!
//! A [`Layout`] settles it with a better version of the same question — *is this inside the
//! project at all, and can `require` name it* — and [`Fence`] is what carries it, because the two
//! halves of the fence coming apart is exactly what the defect was. A surface that fences now
//! holds one value with the cursor gate and the layout in it, and cannot be written with one and
//! not the other.
//!
//! **The root rung is the exception, and it is an argument rather than an oversight.** A hit on
//! `Object`, `Module` or `Class` answers for every receiver in the workspace, so it is the one
//! rung where a wrong answer is worst — and *loosening* a fence there is loosening it backwards.
//! `rbs` ships `lib/rbs/test/setup.rb`, a script `require` really can name that really does write
//! a top-level `def match`; exempting it turned a 106-candidate *Guessed* list on `match` in
//! discourse's `config/routes.rb` into a one-place ***Resolved*** card pointing at an RBS test
//! harness. So [`Fence::loadable_on_a_root`] reads the directory name and nothing else, and
//! [`Fence::loadable`] and [`Fence::only_the_suite`] read the layout.
//!
//! [`Trees`] answers the same question from the other side, off the workspace's own document set,
//! and is what `completion` and the two ranked lists read. `locator`'s rungs resolve without that
//! set in hand, which is why they take a layout instead.
//!
//! # A template is copied, not loaded, and that is a stricter case than a spec
//!
//! A spec at least loads under RSpec; [`in_a_generator_template`] names a tree that loads
//! nowhere ever. pundit ships `class ApplicationPolicy`, administrate ships
//! `class ApplicationController`, rpush ships `Rpush::Notification` and eighteen migrations, and
//! fabrication ships a **top-level** `def with_ivars` — each of them a name the project declares
//! itself, or would, sitting under the gem's `lib/` where `require` could name it and nothing
//! ever does. The load-path clause that settles rack-test therefore cannot settle this one, and
//! the tag has to say what the tree is *for*.
//!
//! It needs no [`Layout`] for the same reason: a generator template is the same thing in a gem
//! and in the project, and three of the corpora ship twelve of their own. That makes it the one
//! rule here the **root rung** can also read — see [`Fence::loadable_on_a_root`], whose hardest
//! case this is.
//!
//! **What the tag is not is a reason to stop indexing the tree.** A template is real Ruby
//! somebody edits, and the *never* row — `references`, `rename`, `documentHighlight` — has to
//! find its uses like any other file's. Dropping it from the index would answer this rule by
//! breaking that one.
//!
//! # A migration runs once, in a process of its own
//!
//! `db/migrate` is not an autoload path. The task that runs a migration loads that one file by
//! path and nothing else, which is why people write a private copy of a model inside one: the
//! real model has moved on, and the data script needs the schema of the day it was written.
//! mastodon writes 51 such classes with **80** association macros between them, and
//! `workspace/rails/` reads them exactly as it reads a model's, because they *are* models —
//! `class Account < ApplicationRecord` with `belongs_to :account` under it, and a real span on
//! the macro's own line.
//!
//! **So nothing upstream of here is wrong, and the fix is a tree name.** Measured over the
//! audit's own draw of 5,523 positions: 20 carried a place inside a migration and 19 of the 20
//! were mastodon, every one of them a *Guessed* card — a receiver with no type falls to the
//! name-based list and the migration's copy is on it. Two of the nineteen had a migration file
//! **first**, which is where the jump lands: a 2017 backfill, from a helper in `app/`.
//!
//! **The four corpora that scored zero are the draw and not the shape.** lobsters really does
//! declare `CreateCategories::Tag#category` inside a migration; its 777 drawn cursors never
//! asked for the name. What lobsters and solidus mostly generate in there is **columns** — 590
//! and 1,613 members — and a column's place is the `db/schema.rb` line, which dedupes against
//! the real model's and is invisible for that reason rather than because it is fenced.
//!
//! Like a template and unlike a spec it needs no *root* clause — an engine's `db/migrate/` is
//! migrations wherever it was copied from, and discourse's plugins ship 53 such directories, so
//! there is no owner to ask about. It keeps the **load-path** clause, which is the escape hatch:
//! the tag is a substring under `db/`, so it catches a `db/data_migrations/` that a project may
//! genuinely autoload, and a project that put that tree on `[index] load_paths` has said
//! `require` reaches it. No `db/` directory in any of the six corpora is on a load path.
//!
//! # The cursor is what turns it off
//!
//! A developer editing a spec is exactly who the `def` in the next spec file over is the answer
//! for. So what the two dropping surfaces refuse is *leaving the application for a test tree*,
//! not test trees as such, and [`fenced_from`] is the one place that is decided — reached through
//! [`Fence::at`] by everything in `locator`, and directly by `completion`, which builds the gate
//! once per request because it is a fact about the cursor rather than about any candidate.

use std::collections::HashSet;

use rubydex::model::{
    declaration::Declaration,
    graph::Graph,
    ids::{DeclarationId, UriId},
};

use super::synthesized::source_of;

/// The trees that only run under a test harness.
///
/// A **deny**-list, and the corpora are the argument. The obvious rule — *the target is under
/// `app/` or `lib/`* — does not survive five real repositories: lobsters autoloads `extras/`,
/// mastodon declares `module Mastodon` in `config/application.rb`, forem reopens `Sidekiq::Job`
/// in `config/initializers/`. All three are loaded, first-party code, and an allow-list of two
/// directory names calls every one of them a wrong answer. Six Ruby repositories do not agree on
/// where source lives; they do agree on where tests live.
pub const TEST_TREES: [&str; 4] = ["spec", "test", "tests", "features"];

/// Which directory names each rule here reads, once a project has said.
///
/// **The three lists are not one list and are not shaped the same, which is the module's own
/// asymmetry made settable rather than a new one.** [`TEST_TREES`] deletes an answer when it is
/// wrong, so a project **replaces** it and is shown what it replaced; [`TEST_SUPPORT`] only turns
/// the fence off, so a project **adds** to it and being wrong costs nothing; the migration pair
/// is replaced like the first, because it is the second rule that can delete.
///
/// `Default` is the built-in lists, which is what every caller with no configuration in hand
/// gets — `Fence::off`, `Layout::default`, and every test that has no opinion about trees.
/// `Some(&[])` is a fence deliberately turned **off**, and is a different thing from `None`.
///
/// [`in_a_generator_template`] is deliberately absent. A generator template is the same thing
/// wherever it is shipped from — a gem-authoring convention rather than a choice a project makes
/// about its own layout — so nothing a user could write would make their tree more or less
/// copied-rather-than-loaded. If a case turns up it is a fourth key and it says so then.
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

    /// Whether a document is in a test tree, read off its path and nothing else.
    ///
    /// **Segments and not a prefix**: solidus is an engine monorepo whose specs are `core/spec/`,
    /// and chatwoot has an `enterprise/` overlay. Any matching segment answers, so
    /// `spec/dummy/app/models` is a spec and not an app — solidus really does ship a whole Rails
    /// application under `spec/`.
    ///
    /// The filename is scanned with the directories and needs no case of its own: a Ruby file
    /// ends in `.rb`, `.erb` or `.rbs` and cannot equal one of these four words. A *workspace
    /// root* that does equal one turns the fence off rather than on, because the cursor is then
    /// in a test tree too — which is the safe direction for a rule this crude to fail in.
    pub(super) fn in_a_test_tree(self, uri: &str) -> bool {
        match self.test {
            None => uri.split('/').any(|segment| TEST_TREES.contains(&segment)),
            Some(names) => uri
                .split('/')
                .any(|segment| names.iter().any(|name| name == segment)),
        }
    }

    /// Whether a document is a migration, read off its path and nothing else.
    ///
    /// Every pair in turn, and the shape of the loop is what makes the mark a *directory*:
    /// `current` is judged only while there is a `next` to follow it. A file sitting directly
    /// under `db/` whose own name holds the word is a loader, not a migration.
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

    /// One directory and the one above it, against whichever pairs are in force.
    ///
    /// An entry with no separator in it is skipped rather than guessed at, and
    /// `config::validate` is where the user is told: a bare `migrate` would fence
    /// `app/services/migrate/`, which is an answer deleted rather than a setting ignored.
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
    /// The **union** of the two lists and not either alone, and the union is why the safe list
    /// may be extended freely: everything on it only ever makes this answer `true`.
    fn turns_the_fence_off(self, uri: &str) -> bool {
        self.in_a_migration(uri)
            || self.in_a_test_tree(uri)
            || uri.split('/').any(|segment| {
                TEST_SUPPORT.contains(&segment) || self.support.iter().any(|name| name == segment)
            })
    }
}

/// The directory a generator copies **out** of, and the one above it that says so.
///
/// A Rails generator ships the files it will one day write into somebody else's project, and
/// they are ordinary Ruby that ordinary indexing believes: pundit's
/// `lib/generators/pundit/install/templates/application_policy.rb` declares
/// `class ApplicationPolicy`, administrate's declares `class ApplicationController`, rpush's
/// declare `Rpush::Notification` and eighteen migrations, and fabrication's cucumber steps
/// declare a **top-level** `def with_ivars`. Every one of those is a name the application
/// declares itself, or would; none of them is a file anybody loads.
///
/// **Both names, in that order, because either one alone is wrong.** `templates` on its own
/// deletes real libraries — yard ships 54 files under `lib/yard/templates/` and
/// `YARD::Templates::Engine` is a class people call, temple ships `lib/temple/templates/`.
/// `generators` on its own deletes the generator, which is loadable code that `rails generate`
/// really does require. What loads nowhere is only the tree in between.
const GENERATOR_TREE: &str = "generators";
/// The second half of [`GENERATOR_TREE`]'s pair.
const TEMPLATE_TREE: &str = "templates";

/// Whether a document is a generator's template, read off its path and nothing else.
///
/// A `templates` segment somewhere **after** a `generators` segment. The two `any` calls share
/// one iterator, which is what makes that *after* rather than *both somewhere*: the first stops
/// at `generators` and the second goes on from where it stopped. The gap between them is not
/// fixed and cannot be — rpush writes `lib/generators/templates/` with nothing in between and
/// railties writes `lib/rails/generators/rails/app/templates/` with four.
///
/// **No [`Layout`], deliberately, and this is the one path rule that needs none.**
/// [`in_a_test_tree`] has to ask whose tree it is, because a gem's `lib/rack/test/` is a
/// published library while the project's `spec/` is not. A generator template is the same thing
/// wherever it is shipped from: solidus, forem and mastodon ship twelve of their own, and those
/// load exactly as often as pundit's.
pub(super) fn in_a_generator_template(uri: &str) -> bool {
    let mut segments = uri.split('/');
    segments.any(|segment| segment == GENERATOR_TREE)
        && segments.any(|segment| segment == TEMPLATE_TREE)
}

/// The directory a migration is run out of, and the one above it that says so.
///
/// **`db/migrate` is not an autoload path and never has been.** Rails loads one migration file,
/// by path, in the process that runs it, so a *backfill copy of a model* written inside one —
/// `class Account < ApplicationRecord` with three macros in it, so a data script can run against
/// the schema of that day — is private to that file. It is real Ruby, really declared, and
/// reachable from nothing: not from the application, not from the suite, and not from the next
/// migration along. mastodon writes **80** association macros inside such classes and this crate
/// reads every one of them, which is how they reach a name-based list.
///
/// **The parent segment and not the name alone.** `migrate` is an ordinary enough word for a
/// project to write `app/services/migrate/`, and what is unloadable is only the tree Rails owns.
/// Under `db/` it is unambiguous.
///
/// The cost of *any* `db` segment rather than the project's own is stated rather than hidden: a
/// project that wrote `app/models/db/migrations/` would be fenced and should not be. Checked
/// across the six corpora, **every `db` directory in them is Rails'** — five at a root, solidus'
/// five engines, discourse's plugins, and two under `spec/`, which the suite rule already has.
///
/// **A substring rather than a list of names**, which is the opposite of what [`TEST_TREES`]
/// does and is right for the opposite reason: six repositories agree on where tests live, and
/// they already spell this one three ways — `db/migrate`, mastodon's `db/post_migrate` and
/// lobsters' `db/old_migrations`. A fixed list would go stale silently the first time somebody
/// invented a fourth.
///
/// **And it has to be a directory.** A file sitting directly under `db/` whose own name holds
/// the word is a loader, not a migration. No corpus ships one; the clause is what keeps this a
/// rule about a *tree*, which is the only thing the rules here are about.
///
/// **No *root* clause, for [`in_a_generator_template`]'s reason.** An engine ships `db/migrate/`
/// and those are migrations wherever they were copied from — discourse's plugins carry 53 such
/// directories of their own — so there is no owner to ask about. The load-path clause is kept,
/// and it is the escape hatch: see [`Fence::only_a_migration`].
const MIGRATION_ROOT: &str = "db";
/// The second half of [`MIGRATION_ROOT`]'s pair, matched anywhere inside the directory's name so
/// that `migrate`, `post_migrate` and `old_migrations` all answer.
const MIGRATION_MARK: &str = "migrat";

/// The built-in migration pair, spelled the way `trees.migration` takes it.
///
/// A third spelling of the two constants above, and it exists for one reason: the VS Code
/// manifest documents this default, `tests/vscode_manifest.rs` checks the two against each
/// other, and a default written out in JSON with nothing holding it to the code is documentation
/// that rots. `the_built_in_pair_is_the_one_the_setting_would_have_to_write` is what keeps the
/// three in step.
pub const MIGRATION_PAIR: &str = "db/migrat";

/// The trees a **cursor** is read against, over and above [`TEST_TREES`].
///
/// **The two lists are deliberately different sizes, and the asymmetry is the point.** A name on
/// the target list deletes an answer when it is wrong, so that list stays at the four names six
/// repositories agree on. A name on *this* list only turns the fence **off**, so being wrong here
/// costs nothing but the protection it was going to give — which makes it safe to name a tree
/// that merely looks like test scaffolding.
///
/// One name, because one name is what the corpora justify. solidus ships **119 Ruby files** under
/// a `testing_support` segment that is not inside any test tree — shared examples and factories
/// under `core/lib/spree/testing_support/`, published for other people's suites — and a developer
/// reading one of those is exactly who a spec's `def` is the answer for. Measured over the six:
/// `shared_examples` (13 files), `factories` (66) and `fabricators` (0) add nothing this does not
/// already cover, and `support` would add 10 that are **not** test code at all —
/// `script/import_scripts/support` on discourse — which is the whole reason a guessed name is not
/// free even in the safe direction.
const TEST_SUPPORT: [&str; 1] = ["testing_support"];

/// Whether a request made from `cursor` is fenced at all.
///
/// `None` — a document the graph has never held — is **not** fenced, because the fence needs
/// evidence to fire and the absence of a path is not evidence. Every fencing surface asks it here
/// so that they cannot answer it differently.
pub(super) fn fenced_from(cursor: Option<&str>, names: Names<'_>) -> bool {
    // A migration is on the off-list for the same reason a spec is: a developer editing one is
    // exactly who the class declared at the top of it is the answer for — and inside that
    // `class Foo < Migration` body Ruby really does resolve the constant to the inner copy,
    // which is a lexical question no fence should be deciding.
    cursor.is_some_and(|uri| !names.turns_the_fence_off(uri))
}

/// The documents the application does not load, by the tree they are in.
///
/// Built once per request, because the set is then consulted once per *definition* of every
/// candidate and a bundle has six figures of them — a `HashSet<UriId>` lookup rather than a path
/// split, which is the whole reason this type exists rather than a call to the two tags.
///
/// **The two tags are read of different sets, and that asymmetry is the rule rather than an
/// accident.** [`in_a_test_tree`] is read of `own`, because `own` is the only set that knows
/// where *this project's* test trees are: a gem that ships `lib/rack/test/` is publishing a
/// library, so a miss here has to mean "not a spec" rather than "unknown".
/// [`in_a_generator_template`] is read of the whole graph, because a template is the same thing
/// wherever it ships from and a gem's is the common case — pundit's, jbuilder's, devise's.
/// [`in_a_migration`] is read of it for the same reason: an engine's `db/migrate/` is migrations,
/// and discourse's plugins ship 53 such directories of their own. Walking the graph costs one
/// pass over the documents, which is the pass `own` itself was built with.
///
/// [`Locality`]: super::completion
pub(super) struct Trees {
    documents: HashSet<UriId>,
}

impl Trees {
    pub(super) fn of(graph: &Graph, own: &HashSet<UriId>, names: Names<'_>) -> Self {
        let suite = own.iter().copied().filter(|id| {
            graph
                .documents()
                .get(id)
                .is_some_and(|document| names.in_a_test_tree(document.uri()))
        });
        let elsewhere = graph
            .documents()
            .iter()
            .filter(|(_, document)| {
                in_a_generator_template(document.uri()) || names.in_a_migration(document.uri())
            })
            .map(|(id, _)| *id);
        Self {
            documents: suite.chain(elsewhere).collect(),
        }
    }

    pub(super) fn holds(&self, uri_id: &UriId) -> bool {
        self.documents.contains(uri_id)
    }
}

/// One walk over a declaration's definitions, and the one place the rule is written down.
///
/// Every surface here walks a declaration's definitions for something else already — the
/// nearest document, whether the project owns it — so the tag rides along on a walk that was
/// happening anyway. What must not ride along is the *decision*, which has two cases that are
/// each easy to get wrong once and impossible to get wrong in one place.
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
    /// *Any*, because a class the suite reopens is still the application's class and a `def` the
    /// application also writes is the application's `def` whatever a spec does beside it. What
    /// this removes is the declaration whose *every* definition is under a test tree.
    ///
    /// **Counted, so that a declaration with no definitions at all is loadable.** rubydex's own
    /// built-ins have none — `Object` and `Module` are in the graph without a line of anybody's
    /// Ruby behind them — and a rule written as "any definition is loadable" answers *no* for
    /// those, which drops the top of the object model out of every list outside a test tree.
    pub(super) fn loadable(&self) -> bool {
        self.definitions == 0 || self.testing < self.definitions
    }
}

/// Where this project's own files are, and what `require` can name.
///
/// Two prefix sets, and between them they say whether a directory called `test` is **this
/// project's** suite or somebody else's published library. The tag [`in_a_test_tree`] cannot
/// tell those apart on its own, and three kinds of file prove it, all of them library code:
/// a gem's `lib/rack/test/`, a gem's `sig/test/` — `rbs` ships exactly that — and Ruby's own
/// vendored signatures for minitest, which live under `minitest/test/` and `minitest/spec/` and
/// are indexed into every project there is.
///
/// It travels as one value because it is asked as one question. [`Trees`] is the same question
/// answered from the workspace's own document set, which is what `completion` and the two ranked
/// lists read; `locator`'s rungs resolve without that set in hand and read this instead.
#[derive(Clone, Copy, Default)]
pub struct Layout<'a> {
    /// The workspace root as a directory URI prefix, from `Analysis::workspace_prefix`.
    ///
    /// Empty matches everything, which is the direction that fences more rather than less, and
    /// it is the value a test that has no opinion about roots leaves here.
    pub(super) root: &'a str,
    /// The load path as document-URI prefixes, in the order `require` searches them, from
    /// `Analysis::load_prefixes`.
    pub(super) load: &'a [String],
    /// Which directory names the three path rules read, from `[trees]`.
    ///
    /// It rides here rather than beside here for [`Fence`]'s reason one level down: a surface
    /// that fences holds **one** value, and the halves of a fence coming apart is the defect
    /// this module has already had. A layout that knew where the project was and not what its
    /// trees are called would be the same shape of hole.
    pub(super) names: Names<'a>,
}

/// The fence, as every surface that applies it carries it.
///
/// **Two facts that have to travel together**, and the whole of this type is that they now do.
/// One is whether the cursor turns the fence on at all; the other is the [`Layout`] that tells a
/// library from a suite. They were two separate arguments once, and only
/// [`locator::places`](super::locator) was handed the second: a method whose only definition is
/// rack-test's `lib/rack/test/utils.rb` answered **nothing at all** from the name rung, while the
/// same file one directory up answered, and `railties` puts `rails/commands/test/` in the same
/// position. 128 gem files across the six corpora carry such a segment and appear in real
/// answers. A pair that cannot be taken apart is what stops the next surface being written with
/// one half of it.
#[derive(Clone, Copy)]
pub(super) struct Fence<'a> {
    on: bool,
    layout: Layout<'a>,
}

impl<'a> Fence<'a> {
    /// The fence a request asked from `cursor` carries.
    pub(super) fn at(cursor: Option<&str>, layout: Layout<'a>) -> Self {
        Self {
            on: fenced_from(cursor, layout.names),
            layout,
        }
    }

    /// Off, for the callers that must never fence.
    ///
    /// [`locator::resolve`](super::locator) is the one: `references`, `rename` and the two
    /// hierarchies reach the graph through it, and a use under `spec/` is a use. Spelling it
    /// out at the call site is what makes that a decision rather than an omission.
    pub(super) fn off() -> Self {
        Self {
            on: false,
            layout: Layout::default(),
        }
    }

    /// Whether the fence is on at all. See [`fenced_from`].
    pub(super) fn is_on(self) -> bool {
        self.on
    }

    /// Whether a document is one only **this project's** test run loads.
    ///
    /// Three clauses, and the last two are what [`Layout`] exists for.
    ///
    /// **The path says `test`.** [`in_a_test_tree`], four directory names and nothing else to
    /// ask — a `Document` has a uri and no environment on it.
    ///
    /// **And it is inside the workspace.** The four names were chosen against *the project's*
    /// trees, so a file nobody here wrote is not what the rule is about. That covers everything
    /// indexed from outside: a gem's `sig/test/`, and Ruby's own minitest signatures under
    /// `minitest/test/` and `minitest/spec/`, which every project gets.
    ///
    /// **And `require` cannot name it.** The better version of the same question, and the clause
    /// that keeps a *vendored* gem's `lib/rack/test/` — which is inside the workspace by path
    /// and is still a published library. It also decides the project's own `lib/foo/test/`: on
    /// the load path, therefore loaded, whatever the directory is called. A project that puts
    /// `spec/` on `[index] load_paths` has said `require` reaches it and gets the answer it
    /// asked for.
    ///
    /// An empty [`Layout`] — before the bundle is discovered, or a test with no opinion — leaves
    /// the path tag deciding alone, which fences more rather than less.
    ///
    /// Both prefix clauses are asked of the *source* uri, which is what [`source_of`] is for.
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

    /// Whether a document is one the application never loads — the whole question, in one place.
    ///
    /// Three reasons a file can be indexed and never run, and **the module's opening sentence
    /// has to widen for the second and the third**. A project has two environments, the program's and the suite's,
    /// and [`only_the_suite`](Self::only_the_suite) is that distinction. A generator template is
    /// in neither: it is not loaded by the program, it is not loaded by the suite, and it is not
    /// loaded by the gem that ships it. It is *copied*, once, into a project that does not exist
    /// yet. So the rule the surfaces apply is not "which environment" but **can the application
    /// load this at all**, and the suite is one of the two answers rather than the only one.
    ///
    /// The clauses are shaped differently for a reason, and the difference is the argument for
    /// all three. [`only_the_suite`](Self::only_the_suite) needs a [`Layout`] because four
    /// directory names cannot say whose tree they are — a gem's `lib/rack/test/` is a library.
    /// [`in_a_generator_template`] needs none, because the pair of names says what the tree is
    /// *for* rather than who owns it, and the answer does not change when the gem does.
    /// [`only_a_migration`](Self::only_a_migration) sits between them: no root clause, because
    /// an engine's `db/migrate/` is migrations wherever it was copied from, and the load-path
    /// clause kept as the escape hatch for a `db/` tree a project really does autoload.
    ///
    /// **What is deliberately not here is the index.** A template is real Ruby somebody edits,
    /// and `references`, `rename` and `documentHighlight` must find its uses like any other
    /// file's. Dropping the tree from indexing would answer this rule and break that one; a tag
    /// answers only the surfaces in the table above.
    pub(super) fn unloadable(self, uri: &str) -> bool {
        self.only_the_suite(uri)
            || in_a_generator_template(source_of(uri))
            || self.only_a_migration(uri)
    }

    /// Whether a document is one only the migration task loads.
    ///
    /// [`in_a_migration`], **and `require` cannot name it** — one of
    /// [`only_the_suite`](Self::only_the_suite)'s two layout clauses and not both. The root
    /// clause is what asks *whose* tree this is, and a migration has no owner worth asking
    /// about: an engine's `db/migrate/` is migrations exactly as the project's is, which is why
    /// [`Trees`] and [`loadable_on_a_root`](Self::loadable_on_a_root) read the bare tag.
    ///
    /// The load-path clause is the escape hatch, and it is the same sentence the suite rule
    /// already makes: **a project that puts a directory on `[index] load_paths` has said
    /// `require` reaches it**, and gets the answer it asked for. Nothing in the six corpora needs
    /// it — no `db/` directory in any of them is on a load path — and it is here because the
    /// tag is a substring under `db/` rather than a list of names, so `db/data_migrations/` is
    /// caught by it whether or not the project autoloads that tree. Where the project does, the
    /// jump, the guess and the card come back.
    ///
    /// **What it does not reach is `completion` and the picker**, which read [`Trees`] and have
    /// no [`Layout`] in hand. That asymmetry is not new and is not this tag's: `in_a_test_tree`
    /// is read there the same crude way, so a project that puts `spec/` on a load path gets its
    /// jumps back and not its completion rows. Fixing it means handing those two surfaces a
    /// layout, which is a change to them rather than to this.
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
    /// [`Tally`]'s rule, asked of a declaration the caller has only an id for. **True outright
    /// where the fence is off**, so a caller cannot apply the rule and forget the cursor.
    ///
    /// **A declaration the graph does not hold is not loadable**, which is the one place this
    /// parts company with [`Tally::loadable`]'s "no evidence" rule. The question there is about
    /// definitions and here it is about a lookup that failed, and a declaration nothing can be
    /// found for has nowhere to send anybody anyway.
    pub(super) fn loadable(self, graph: &Graph, id: DeclarationId) -> bool {
        self.walk(graph, id, |uri| self.unloadable(uri))
    }

    /// The same question asked of a member found on a **root**, where the path is read at its
    /// crudest and the [`Layout`] is not consulted.
    ///
    /// A hit on `Object`, `Module` or `Class` is a hit on every receiver in the workspace, which
    /// is why `resolve_call`'s root arm is fenced at all. §1.1 is the reason it is *this*
    /// fence's hardest case: rubydex has no notion of a block or of a script, so a top-level
    /// `def` anywhere at all becomes a member of everything.
    ///
    /// [`Layout`] answers *can the application load this file*, and that is the right question
    /// for a list of candidates and the wrong one here. `rbs` ships `lib/rbs/test/setup.rb` —
    /// a script `require` really can name, holding a real top-level `def match`. Exempting it
    /// turned `match` in discourse's `config/routes.rb` from a **106-candidate *Guessed*** list,
    /// which held the right answer, into a **one-place *Resolved*** card pointing at an RBS test
    /// harness, on five of 6,836 drawn call cursors. That is the failure 30 closed, arriving
    /// through the relaxation. **A fence loosened on the one rung where being wrong is worst is
    /// loosened backwards**, so the root rung goes on reading the directory name and nothing
    /// else, and what it loses is a gem's top-level `def` under a directory called `test` —
    /// which is the shape nobody should be sent to anyway.
    ///
    /// [`in_a_generator_template`] and [`in_a_migration`] join it here rather than being
    /// excepted with the layout, because both are already rules about what a tree is *for* and
    /// neither has a layout to drop.
    /// It is also the tag's worst case: fabrication ships
    /// `lib/rails/generators/fabrication/cucumber_steps/templates/fabrication_steps.rb`, whose
    /// top-level `def with_ivars` becomes a member of every receiver in three of the six
    /// corpora.
    pub(super) fn loadable_on_a_root(self, graph: &Graph, id: DeclarationId) -> bool {
        self.walk(graph, id, |uri| {
            self.layout.names.in_a_test_tree(uri)
                || in_a_generator_template(uri)
                || self.layout.names.in_a_migration(uri)
        })
    }

    /// One walk over a declaration's definitions, with the reading of a path handed in.
    fn walk(self, graph: &Graph, id: DeclarationId, unloadable: impl Fn(&str) -> bool) -> bool {
        if !self.on {
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

/// Where a declaration is written, from the single walk both ranked lists need.
///
/// Two questions of the same definitions, asked together because asking them apart means two
/// walks over a hundred and fifty thousand candidates to answer one sort key.
pub(super) struct Placement {
    /// Whether any of its definitions is in the user's own code.
    ///
    /// `any`, not "the first one": a class the project reopens is the project's, even when the
    /// gem that first defined it sorts ahead of it.
    pub(super) own: bool,
    /// Whether any of its definitions is in code the application loads. See [`Tally::loadable`].
    pub(super) loadable: bool,
}

/// [`Placement`], in one pass.
pub(super) fn placement(
    graph: &Graph,
    declaration: &Declaration,
    own: &HashSet<UriId>,
    trees: &Trees,
) -> Placement {
    let mut tally = Tally::default();
    let mut is_own = false;
    for definition in declaration
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
    {
        is_own |= own.contains(definition.uri_id());
        tally.saw(trees.holds(definition.uri_id()));
    }
    Placement {
        own: is_own,
        loadable: tally.loadable(),
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    /// One of the three lists, as a project would write it in `ya-lsp.toml`.
    fn named(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_project_that_keeps_its_suite_somewhere_else_replaces_the_list() {
        // **Replace and not extend**, which is the dangerous list's shape: a key that appended
        // would invite somebody to add `lib` and silently delete their whole workspace from
        // completion, definition and hover.
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

        // And an empty list is a fence deliberately turned **off**, which is a different thing
        // from one nobody set.
        let nothing: Vec<String> = Vec::new();
        let off = Names {
            test: Some(&nothing),
            ..Names::default()
        };
        assert!(!off.in_a_test_tree("file:///p/spec/models/store_spec.rb"));
    }

    #[test]
    fn the_cursor_list_is_added_to_and_the_built_in_name_survives_it() {
        // The safe list, and the asymmetry is the whole argument: a name here only turns the
        // fence **off**, so being wrong costs nothing but the protection it was going to give.
        let extra = named(&["fixtures"]);
        let widened = Names {
            support: &extra,
            ..Names::default()
        };
        assert!(!fenced_from(Some("file:///p/fixtures/stories.rb"), widened));
        assert!(
            !fenced_from(
                Some("file:///p/core/lib/spree/testing_support/shared.rb"),
                widened
            ),
            "the built-in name is added to, never replaced"
        );
        // And an ordinary file is still fenced, so the list has not simply swallowed everything.
        assert!(fenced_from(Some("file:///p/app/models/store.rb"), widened));
    }

    #[test]
    fn a_migration_tree_somewhere_else_keeps_the_pair_and_the_substring() {
        // The value is a `parent/mark` pair because the rule is not "a directory called
        // migrate" — `migrate` is an ordinary enough word for `app/services/migrate/`.
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

        // An entry with no parent in it is skipped rather than guessed at — `config::validate`
        // is where the user is told, because a bare `migrate` would fence a tree they load.
        let bare = named(&["migrate"]);
        let ignored = Names {
            migration: Some(&bare),
            ..Names::default()
        };
        assert!(!ignored.in_a_migration("file:///p/db/migrate/x.rb"));
    }

    #[test]
    fn a_replaced_test_list_reaches_every_surface_that_fences_on_one() {
        // **The failure this test exists for is a fence half-threaded**, which is the defect this
        // module has already had once: only `locator::places` was handed the layout, and
        // rack-test's `lib/rack/test/utils.rb` answered nothing while the same file one directory
        // up answered. A configurable list is the same hazard with a new way in, so the claim is
        // that one value reaches all four readers — `fenced_from`, `Trees::of`, `completion`'s
        // per-document loop and `locator::preferred_definition`.
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

        // **And `spec/` is not fenced any more**, which is the half a key that merely *added*
        // `qa` would have got wrong — and the reason this list replaces rather than extends.
        let offered = harness.suggestions(&app, &format!("{ANCESTRY}spec_only_hel~\n"));
        assert!(
            offered.iter().any(|row| row == "spec_only_helper"),
            "{offered:?}"
        );

        // **Ranked and never dropped**: the picker is the only means there is of looking for a
        // name, so a declaration that really exists stays findable by it.
        let found = harness.symbol_names("qa_only_helper");
        assert!(
            found.iter().any(|name| name.contains("qa_only_helper")),
            "{found:?}"
        );

        // **Never**: a use under a fenced tree is a use, and a work list that quietly omitted it
        // is a rename that breaks the suite.
        let uses = harness.reference_list(&app, ANCESTRY, "Store", true);
        assert!(
            uses.iter().any(|row| row.contains("qa_helpers.rb")),
            "{uses:?}"
        );
    }

    #[test]
    fn replacing_a_fence_list_says_what_it_replaced() {
        // A replacement is only safe if it is visible: somebody who set this has taken four
        // directory names six repositories agree on out of play, and this is the only place that
        // shows. `index.include` owes the same debt and `messages::include_is_empty` pays it.
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

        // And a project that has not moved either says nothing at all about them — `test_support`
        // included, which replaces nothing and so has nothing to say.
        let (_, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            let mut harness = Harness::configured("[trees]\ntest_support = [\"fixtures\"]\n");
            harness.write("app/store.rb", "class Store\nend\n");
            harness.index();
        });
        assert!(!logged.contains("trees."), "{logged}");
    }

    #[test]
    fn the_built_in_pair_is_the_one_the_setting_would_have_to_write() {
        // Three spellings of one rule — the two constants, and the string the manifest
        // publishes as the default of `trees.migration`. This is what stops them drifting.
        assert_eq!(
            crate::workspace::config::migration_pair(MIGRATION_PAIR),
            Some((MIGRATION_ROOT, MIGRATION_MARK))
        );
    }
    #[test]
    fn the_tag_matches_a_path_segment_and_never_a_prefix() {
        // solidus is the case: an engine monorepo whose specs are `core/spec/`, with a whole
        // Rails application under `spec/dummy/`. A prefix test answers "no" to both.
        assert!(Names::default().in_a_test_tree("file:///p/spec/models/store_spec.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/core/spec/models/store_spec.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/spec/dummy/app/models/store.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/test/unit/store_test.rb"));
        assert!(Names::default().in_a_test_tree("file:///p/features/step_definitions/a.rb"));

        assert!(!Names::default().in_a_test_tree("file:///p/app/models/store.rb"));
        // The three near misses: a longer directory name, a file that begins with the word,
        // and a file *called* one of them. The filename is scanned with the directories and
        // needs no case of its own precisely because a Ruby file cannot equal one of the four.
        assert!(!Names::default().in_a_test_tree("file:///p/specs/models/store.rb"));
        assert!(!Names::default().in_a_test_tree("file:///p/app/spec_helper_loader.rb"));
        assert!(!Names::default().in_a_test_tree("file:///p/app/spec.rb"));
    }

    #[test]
    fn a_cursor_the_graph_never_held_fences_nothing() {
        // The fence needs evidence to fire, and the absence of a path is not evidence. Both
        // dropping surfaces read this one function so that they cannot answer it differently —
        // they did, before it was one function: the name rung fenced a cursor it had no path
        // for and the completion list did not.
        assert!(fenced_from(
            Some("file:///p/app/models/store.rb"),
            Names::default()
        ));
        assert!(!fenced_from(
            Some("file:///p/spec/models/store_spec.rb"),
            Names::default()
        ));
        assert!(!fenced_from(None, Names::default()));
    }

    #[test]
    fn only_this_project_s_own_test_trees_are_test_trees() {
        // The three clauses, and the two library shapes that made them necessary. The tag is
        // four directory names written against *the project's* trees, and everything indexed
        // from outside it is somebody else's published surface: rack-test's
        // `lib/rack/test/utils.rb`, `rbs`' own `sig/test/`, and Ruby's vendored minitest
        // signatures under `minitest/test/` — which every project gets. Only `locator::places`
        // was handed any of this, so a method whose one definition is `lib/rack/test/utils.rb`
        // answered nothing at all from the name rung while the same file one directory up
        // answered.
        // `Workspace::load_paths` order: the bundle, then the project's own `lib/` and `app/`.
        let load = vec![
            "file:///gems/rack-test-2.2.0/lib/".to_owned(),
            "file:///p/lib/".to_owned(),
        ];
        let layout = Layout {
            root: "file:///p/",
            load: &load,
            names: Names::default(),
        };
        let fence = Fence::at(Some("file:///p/app/models/store.rb"), layout);
        assert!(fence.is_on());
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
        // source's with `ya-lsp-generated:` in front, which is not a path — so every prefix
        // clause reads it as nowhere at all unless the scheme comes off first, and a
        // `Data.define` written in a project's own spec file stops being the suite's. lobsters
        // writes exactly that, and it reached 35 real answers before the strip.
        assert!(fence.only_the_suite("ya-lsp-generated:file:///p/spec/models/store_spec.rb"));
        assert!(!fence.only_the_suite("ya-lsp-generated:file:///p/app/models/store.rb"));
        assert!(!fence.only_the_suite(
            "ya-lsp-generated:file:///gems/rack-test-2.2.0/lib/rack/test/utils.rb"
        ));

        // An empty layout is the window before the bundle is discovered, and a test with no
        // opinion about roots. It leaves the path tag deciding alone, which fences more.
        let bare = Fence::at(Some("file:///p/app/models/store.rb"), Layout::default());
        assert!(bare.only_the_suite("file:///gems/rack-test-2.2.0/lib/rack/test/utils.rb"));

        // The cursor turns it off, and `resolve` turns it off in writing.
        assert!(!Fence::at(Some("file:///p/spec/models/store_spec.rb"), layout).is_on());
        assert!(!Fence::at(None, layout).is_on());
        assert!(!Fence::off().is_on());
    }

    #[test]
    fn a_migration_is_a_tree_under_db_and_the_spelling_is_not_a_fixed_list() {
        // The three spellings the six corpora already write, in a project, in a plugin's own
        // `db/` and in an engine shipped as a gem. A list of names would have had to guess
        // `old_migrations`, which is lobsters', and it would have been guessed wrong.
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

        // The parent segment is what makes it Rails' tree rather than an ordinary word, and
        // both of these are application code somebody autoloads.
        assert!(!Names::default().in_a_migration("file:///p/app/services/migrate/runner.rb"));
        assert!(!Names::default().in_a_migration("file:///p/lib/migrations/step.rb"));

        // The neighbours under `db/` that really are loaded — every corpus ships two of them.
        assert!(!Names::default().in_a_migration("file:///p/db/schema.rb"));
        assert!(!Names::default().in_a_migration("file:///p/db/seeds.rb"));
        assert!(!Names::default().in_a_migration("file:///p/db/views/story.rb"));

        // A *file* directly under `db/` whose own name holds the word is a loader and not a
        // tree. No corpus ships one, and the clause is what keeps this a rule about a directory.
        assert!(!Names::default().in_a_migration("file:///p/db/migration_helpers.rb"));

        // And nothing to make a pair out of.
        assert!(!Names::default().in_a_migration("db"));
        assert!(!Names::default().in_a_migration(""));
    }

    #[test]
    fn a_migration_is_unloadable_wherever_it_ships_from_unless_require_can_name_it() {
        // An empty `Layout` is the window before the bundle is discovered. The suite clause
        // would fall back to the path tag alone there; this one has no clause to lose, which is
        // the whole of what it shares with a generator's template.
        let bare = Fence::at(Some("file:///p/app/models/story.rb"), Layout::default());
        assert!(bare.unloadable("file:///p/db/migrate/20180528141303_fix_index.rb"));
        assert!(!bare.unloadable("file:///p/db/schema.rb"));

        // **The generated document is the one that matters**, because the members a reader
        // reaches are `belongs_to` and `has_many` this crate wrote down, and they are filed
        // under the scheme in front of the migration's uri. Missing the strip here is the bug
        // that cost 35 real answers when the suite clause was written — same shape, same file.
        assert!(
            bare.unloadable("ya-lsp-generated:file:///p/db/migrate/20180528141303_fix_index.rb")
        );
        assert!(!bare.unloadable("ya-lsp-generated:file:///p/app/models/story.rb"));

        // An engine's, from outside the workspace entirely and on a load path: a spec there
        // would be somebody else's library, and a migration there is still a migration. There is
        // no root clause here, which is the difference from the suite rule.
        let load = vec!["file:///gems/spree-4.4.0/lib/".to_owned()];
        let layout = Layout {
            root: "file:///p/",
            load: &load,
            names: Names::default(),
        };
        let fence = Fence::at(Some("file:///p/app/models/story.rb"), layout);
        assert!(fence.unloadable("file:///gems/spree-4.4.0/db/migrate/20210101_add.rb"));

        // **And the escape hatch.** The tag is a substring under `db/`, so it catches
        // `db/data_migrations/` whether or not the project autoloads that tree — and a project
        // that put it on `[index] load_paths` has said `require` reaches it. Nothing in the six
        // corpora needs this: no `db/` directory in any of them is on a load path.
        let declared = vec!["file:///p/db/data_migrations/".to_owned()];
        let asked = Fence::at(
            Some("file:///p/app/models/story.rb"),
            Layout {
                root: "file:///p/",
                load: &declared,
                names: Names::default(),
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
        // The asymmetry this module already states: a name on the target list deletes an answer
        // when it is wrong, and a name on the cursor list only gives up the protection. Inside
        // `class FixAccountsUniqueIndex` Ruby resolves the constant to the copy declared at the
        // top of that same file, so a reader there is exactly who it is the answer for — and
        // that is a lexical question no path rule should be deciding.
        assert!(!fenced_from(
            Some("file:///p/db/migrate/20180528141303_fix_accounts_unique_index.rb"),
            Names::default()
        ));
        assert!(fenced_from(
            Some("file:///p/db/schema.rb"),
            Names::default()
        ));
        assert!(fenced_from(
            Some("file:///p/app/models/story.rb"),
            Names::default()
        ));
    }

    #[test]
    fn a_generator_s_template_is_copied_and_never_loaded_by_anybody() {
        // The pair, in order, and the two halves that are each wrong alone. Four shapes the
        // corpora ship: pundit's, rpush's with nothing between the names, railties' with four
        // segments between them, and a project's own.
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

        // `templates` alone deletes a real library — yard ships 54 files under
        // `lib/yard/templates/` and `YARD::Templates::Engine` is a class people call, temple
        // ships `lib/temple/templates/`. `generators` alone deletes the generator itself, which
        // `rails generate` really does require.
        assert!(!in_a_generator_template(
            "file:///g/yard-0.9.45/lib/yard/templates/engine.rb"
        ));
        assert!(!in_a_generator_template(
            "file:///g/pundit-2.3.1/lib/generators/pundit/install/install_generator.rb"
        ));

        // In that order, which is what one shared iterator buys: `templates` has to come after
        // `generators` and not merely somewhere in the same path.
        assert!(!in_a_generator_template(
            "file:///p/lib/templates/generators/thing.rb"
        ));
        // Segments, like every other rule here: a longer name and a file called one of them.
        assert!(!in_a_generator_template(
            "file:///p/lib/generators/app_templates/none.rb"
        ));
        assert!(!in_a_generator_template(
            "file:///p/lib/generators/templates.rb"
        ));
    }

    #[test]
    fn a_template_is_unloadable_whoever_ships_it_and_needs_no_layout_to_say_so() {
        // The second reason a document is never loaded, and the one the tag can answer without
        // asking whose tree it is. `only_the_suite` needs a `Layout` because four directory
        // names cannot tell rack-test's library from a suite; a generator template is the same
        // thing in a gem and in the project, so an empty layout answers it identically.
        let load = vec![
            "file:///g/pundit-2.3.1/lib/".to_owned(),
            "file:///p/lib/".to_owned(),
        ];
        let layout = Layout {
            root: "file:///p/",
            load: &load,
            names: Names::default(),
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

        // The generated scheme comes off for this clause too: it is a prefix rule reading a
        // uri that is deliberately not a path. A template generates nothing today, and the
        // strip is what keeps that from being the reason.
        let fence = Fence::at(Some("file:///p/app/models/store.rb"), layout);
        assert!(fence.unloadable(&synthesized::generated_uri(
            &DocUri::from_uri_str(template).expect("a file uri"),
            "class:Thing"
        )));
        assert!(!fence.unloadable(&synthesized::generated_uri(
            &DocUri::from_uri_str("file:///p/app/models/store.rb").expect("a file uri"),
            "class:Store"
        )));
    }

    #[test]
    fn the_rule_is_any_definition_the_application_loads_and_no_definitions_at_all() {
        // The three cases, stated where they are decided. The middle one is the fence; the last
        // is the bug the count is written to prevent — a running `bool` that starts at *not
        // loadable* has nothing to flip it for `Object`, which rubydex declares with no
        // definitions behind it, and the top of the object model then falls out of every list.
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
        // The two halves of `Placement` are independent, and a test double is the case that
        // separates them: it is the user's own code — so the picker must rank it above every
        // gem — and the application never loads it, so it ranks below the application's own.
        let mut harness = Harness::new();
        harness.write("app/models/store.rb", "class Store\nend\n");
        harness.write(
            "spec/models/store_spec.rb",
            "class Store\n  def reopened_by_the_suite\n  end\nend\n\nclass FakeStore\nend\n",
        );
        harness.index();

        let graph = &harness.analysis.graph;
        let own = harness.analysis.own_documents();
        let trees = Trees::of(graph, &own, Names::default());
        let placed = |name: &str| {
            let declaration = graph
                .declarations()
                .iter()
                .find(|(_, declaration)| declaration.name() == name)
                .unwrap_or_else(|| panic!("{name} is not in the graph"))
                .1;
            placement(graph, declaration, &own, &trees)
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
    fn a_gem_is_neither_the_user_s_nor_anything_this_rule_judges() {
        // `own` is the only set that knows where *this project's* test trees are. A gem that
        // ships a `test/` directory inside its `lib/` is not this rule's business, and the
        // shape of that answer is a miss in `Trees` rather than a path test — which is why
        // `Trees::of` is built from `own` and not from the graph's documents.
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness =
            Harness::at_with_env(dir, crate::analysis::position::PositionEncoding::Utf16, env);
        harness.write("app/models/store.rb", "class Store\nend\n");
        harness.index();
        harness.index_gems();

        let graph = &harness.analysis.graph;
        let own = harness.analysis.own_documents();
        let trees = Trees::of(graph, &own, Names::default());
        let (_, megaphone) = graph
            .declarations()
            .iter()
            .find(|(_, declaration)| declaration.name() == "Shouty::Megaphone")
            .expect("the gem's class");
        let placed = placement(graph, megaphone, &own, &trees);
        assert!(!placed.own);
        assert!(placed.loadable, "a gem is never fenced by this rule");
    }
}
