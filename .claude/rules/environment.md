---
paths:
  - "src/analysis/environment.rs"
  - "src/analysis/search.rs"
  - "src/analysis/hierarchy.rs"
  - "src/analysis/completion.rs"
  - "src/analysis/locator.rs"
  - "src/analysis/references.rs"
  - "src/analysis/rename.rs"
---

# What the application can load

A Ruby application has two environments — the one the program runs in and the one the suite runs
in — and a third kind of file that is in neither: a generator's template tree, which is Ruby a gem
ships in order to **copy** it one day into a project that does not exist yet. rubydex models one
graph and none of them — a `Document` has a uri and nothing else to ask — so
`analysis/environment.rs` is the whole of what the crate knows about the difference, and every
surface that acts on it reads that module rather than its own copy of the rule. The question the
module asks is therefore *can the application load this at all*, and the suite is one of two
answers rather than the only one; `Fence::unloadable` is where they meet.

- **The tag is a deny-list of four directory names, matched as path segments — and a project may
  replace it.** `TEST_TREES = ["spec", "test", "tests", "features"]` is the default and
  `environment::Names` is what carries whatever `[trees]` says instead; see `features.md` for the
  keys. The obvious rule — *the target is under
  `app/` or `lib/`* — does not survive the corpora: lobsters autoloads `extras/`, mastodon
  declares `module Mastodon` in `config/application.rb`, forem reopens `Sidekiq::Job` in
  `config/initializers/`. Six Ruby repositories do not agree on where source lives; they agree on
  where tests live. **Segments and not a prefix**, because solidus' specs are `core/spec/` and it
  ships a whole Rails application under `spec/dummy/`.
- **The three lists travel as `Names`, and it rides inside `Layout`.** Which directory names each
  rule reads is now a project's to say, and the value that says so is `Copy`, defaults to the
  built-in lists, and lives on the same struct as the workspace root and the load path — for
  `Fence`'s reason one level down: **a surface that fences holds one value**, and the halves of a
  fence coming apart is the defect this module has already had. A layout that knew where the
  project was and not what its trees are called would be the same shape of hole.
  - Four readers take it directly because they have no `Layout` in hand: `fenced_from`,
    `Trees::of`, `completion`'s per-document loop, and `locator::preferred_definition`. A test
    sets one value and asks all four, because a fence half-threaded is the defect this module has
    already had once.
  - **`Names::default()` is the built-in lists, not empty lists.** Every caller with no
    configuration — `Fence::off`, `Layout::default`, every test with no opinion about trees — gets
    exactly what the constants say. `Some(&[])` is a fence deliberately turned **off** and is a
    different thing from `None`.
  - **Replace for the two that can delete, extend for the one that cannot.** The asymmetry is this
    module's own, made settable: a name on the target list *deletes an answer* when it is wrong,
    so `trees.test` and `trees.migration` replace and say so at `info`; a name on the cursor list
    only turns the fence off, so `trees.test_support` adds.
  - **`in_a_generator_template` takes no names and is unconfigurable.** A generator template is
    the same thing wherever it is shipped from, which is the same argument that already exempts it
    from `Layout`.

- **Three verdicts, and one table.** Which surface does which is decided in `environment.rs`'s
  header and nowhere else:

  | verdict | surfaces | why |
  |---|---|---|
  | **drop** | `completion`; the name rung **and the root rung** of `definition` and `hover`; **the place list** those two answer from (`locator::places`); `signatureHelp`; `callHierarchy/outgoingCalls` | they answer *what can I call from here, and where is it*, and a name the application cannot load is not an answer |
  | **rank** | `workspace/symbol`; `typeHierarchy/subtypes` | they answer *find me this*, and a drop would make a real declaration unfindable by the only means of looking for it |
  | **never** | `references`, `rename`, `documentHighlight`, `callHierarchy/incomingCalls`, `typeHierarchy/supertypes` | they answer *where is this used*, and a use under `spec/` is a use |

- **The root rung is precise, and fencing it is the one part of this that needed an argument.**
  A member found on `Object`, `Module` or `Class` is a member found on *every* receiver, and a
  `def` at the top of a spec — or inside an `RSpec.describe` block, which rubydex records
  identically — lands on exactly those. `resolve_call`'s root arm returns it as `Resolution::precise`,
  which is why `loadable_from` never saw it: `resolve_typed` fences only an imprecise answer.
  Measured over the six corpora at **800 bare calls each, outside the test trees: 31 of 4,251
  answered cursors resolved this way and every one was wrong** — sixteen migrations' `execute`
  answered out of a plugin's spec, eleven service objects' `model`/`policy`/`params` out of a
  migrations-tooling spec, `Post#cook` out of `spec/lib/email_cook_spec.rb`. After: **0**.
- **The jump falls through to the name rung; the card falls through to nothing.** `definition` and
  `hover` re-ask `by_name`, which `resolve_typed` fences in turn, so the reader gets the answer the
  workspace would have given had the spec's `def` never been written — the test of a declaration
  correctly removed. **That is not the same as a good answer**: discourse's `model` becomes 13
  candidates, and its `execute` becomes 599. The noise is the untyped bare call's, not this
  fence's, and it is the same list any such cursor gets. `signatureHelp` and `outgoingCalls` have
  no name rung — an exact callee is the whole of their contract — so there the fenced case simply
  draws nothing, which is the right degradation for a card that paints itself under the argument
  being typed.
- **The place list judges a definition, not a declaration, and it is the fourth thing `places`
  narrows.** `environment::loadable` asks whether a *name* exists outside the test trees; a place
  list is asked whether *this file* is somewhere to send a reader. Ruby reopens freely, so a wide
  namespace collects a definition per file that ever touched it and a monorepo's specs are most of
  those files — solidus offers **539** places for `Spree`, **76** of them under `spec/`, and a
  reader in a controller loads none of them. Same two safety clauses as the signature rule it sits
  beside: kept where **no** loadable place survives, off entirely where the cursor is itself in a
  test tree or a `testing_support` tree. Measured over 4,501 constant cursors outside the test
  trees: **413 positions carried one, 24,143 places dropped, 0 emptied, 0 lists grew, answered
  counts identical** — and the audit's *Resolved cards land in a test tree* went **44 to 0** across
  all six corpora, 2,603 targets to 0.
- **A library is not a suite, and the path alone cannot tell them apart.** The tag is four
  directory names and it was written against *the project's* trees. A gem shipping
  `lib/rack/test/` is publishing a library; `railties` puts `rails/commands/test/` there; `rbs`
  puts its own `sig/test/` there; and Ruby's vendored minitest signatures sit under
  `minitest/test/` in **every** project. `Fence` carries a `Layout` — the workspace root and the
  load paths — and asks the better question: *is this inside the project at all, and can `require`
  name it*. It carries the cursor gate too, and that pairing is the rule: **a surface fences with
  both halves or with neither.** They came apart once, when only `places` had the load paths, and
  the gap was a real defect — a method whose only definition is a gem's `lib/rack/test/utils.rb`
  answered `null` from the name rung while the same file one directory up answered, and **128 gem
  files across the six corpora** carry such a segment and appear in real answers. `Fence::off()`
  is how `locator::resolve` says *never fenced* out loud, so that too is a decision rather than an
  omission.
- **Every prefix clause is asked of the *source* uri, so the generated scheme comes off first.** A
  generated document is `ya-lsp-generated:` in front of the source's whole uri and is deliberately
  not a path, so a prefix test reads it as nowhere at all: outside the workspace, under no load
  path. Left in, that said a `Data.define` written in a project's own spec file was not the
  suite's — lobsters writes exactly that, and it reached **35 real answers** before the strip
  landed. The path tag never noticed, because the source's directories are still there to split
  on. Same lesson as `file:` squashing to `file` in `locator::named_after`: **a scheme in front of
  a uri is not a directory.**
- **The root rung is the one place the layout is deliberately not consulted.** A hit on `Object`,
  `Module` or `Class` answers for every receiver in the workspace, so it is where being wrong is
  worst — and loosening a fence there is loosening it backwards. `rbs` ships
  `lib/rbs/test/setup.rb`, a script `require` really can name that really does write a top-level
  `def match`; exempting it turned a 106-candidate *Guessed* list on `match` in discourse's
  `config/routes.rb` into a one-place ***Resolved*** card pointing at an RBS test harness, on five
  of 6,836 drawn call cursors. `Fence::loadable_on_a_root` reads the directory names and nothing
  else — **both** of them, because `in_a_generator_template` has no layout to refuse in the first
  place and this is the rung where it earns most. What the root rung gives up is a gem's top-level
  `def` under a directory called `test`, which is the shape §1.1 says nobody should be sent to
  anyway.
- **A template is copied and never loaded, which is a stricter case than a spec — and the tag
  says what a tree is *for*, not who owns it.** `in_a_generator_template` is a `templates`
  segment somewhere **after** a `generators` segment, two `any` calls sharing one iterator so
  that "after" is structural rather than checked. Both names, in that order, because either alone
  deletes real code: `templates` on its own takes yard's 54 files under `lib/yard/templates/`
  (`YARD::Templates::Engine` is a class people call) and temple's `lib/temple/templates/`;
  `generators` on its own takes the generator, which `rails generate` really does require. The
  gap between the two names is not fixed — rpush writes `lib/generators/templates/` with nothing
  between them and railties writes `lib/rails/generators/rails/app/templates/` with four.
  **It needs no `Layout`**, which is the one rule here that does not: a gem's `lib/rack/test/` is
  a published library and the project's `spec/` is not, so `in_a_test_tree` has to ask whose tree
  it is — but a generator template is the same thing wherever it ships from, and solidus, forem
  and mastodon ship twelve of their own. That is also what lets the **root rung** read it, which
  is where it matters most: fabrication's
  `lib/rails/generators/fabrication/cucumber_steps/templates/fabrication_steps.rb` puts a
  top-level `def with_ivars` on `Object`, and active_model_serializers' `serializer.rb` puts a
  `def id` there. **The tag is not a reason to stop indexing the tree** — a template is real Ruby
  somebody edits, and the *never* row has to find its uses.
- **A migration is the second tree in neither environment, and its environment lasts one
  process.** `db/migrate` is on no autoload path: the task loads one migration file, by path, and
  nothing else, which is exactly why people write a private copy of a model inside one — the real
  model has moved on and the data script needs the schema of the day it was written. mastodon
  writes 51 such classes with **80** association macros between them, lobsters and solidus write
  4 and 12, and `workspace/rails/` reads them exactly as it reads a model's, because they *are*
  models: `class Account < ApplicationRecord` with `belongs_to :account` under it and a real span
  on the macro's own line. **Nothing upstream of the fence is wrong** — the class is real, the
  macro is real, and the only thing wrong is that the name reaches a reader who can never call it.
- **`in_a_migration` is a pair of segments and a substring, which is the opposite of
  `TEST_TREES` and right for the opposite reason.** Six repositories agree on where tests live, so
  that one is four whole names; they already spell *this* tree three ways — `db/migrate`,
  mastodon's `db/post_migrate`, lobsters' `db/old_migrations` — so a fixed list would go stale the
  first time somebody invented a fourth. The parent segment is what keeps it Rails' tree rather
  than an ordinary word: `app/services/migrate/` and `lib/migrations/` are autoloaded application
  code. And the match has to be a **directory** — a file sitting directly under `db/` whose own
  name holds the word is a loader, which no corpus ships and which the loop's shape rules out by
  only ever judging a segment that has another after it.
- **It needs no *root* clause, and it keeps the load-path one as the escape hatch.** An engine
  ships `db/migrate/` and those are migrations wherever they were copied from — discourse's
  plugins carry 53 such directories — so there is no owner to ask about, which is what lets
  `Trees` and the **root rung** read the bare tag. A top-level `def` above `def change` lands on
  `Object` and answers for every receiver in the workspace, which is the case worth having there.
  The load-path clause is kept because the tag is a *substring* under `db/`: it catches a
  `db/data_migrations/` that a project may genuinely autoload, and a project that put that tree
  on `[index] load_paths` has said `require` reaches it and gets its jumps and guesses back.
  **Nothing in the six corpora needs it** — no `db/` directory in any of them is on a load path.
- **What the escape hatch does not reach is `completion` and the picker, and that is not new.**
  Both read `Trees`, which has no `Layout`, so a project that puts a tree on a load path gets its
  jumps back and not its completion rows. `in_a_test_tree` is read there the same crude way and
  has been since it was written; fixing it means handing those two surfaces a layout, which is a
  change to them rather than to any tag.
- **The cursor gate turns it off, and that is the lexical question staying out of a path rule.**
  Inside `class FixAccountsUniqueIndex` Ruby really does resolve `Account` to the copy declared at
  the top of that same file, so a reader in a migration is exactly who it is the answer for.
  `fenced_from` reads it as a pair rather than a segment, which is why it sits beside the list
  rather than in it.
- **The *never* row holds inside a migration too, and a test says so.** `references` and `rename`
  must find a use in `db/migrate` like any other: a work list that quietly omitted the tree is a
  rename that leaves a migration calling a method that no longer exists, and the failure surfaces
  years later on somebody else's machine.
- **Measured, two binaries from one tree, the audit's own draw of 5,523 positions.**
  `definition`: **20 lists changed, 0 emptied, 261 places lost and every one of the 261 a
  migration — 0 gained, 0 tiers moved.** Nineteen of the twenty are mastodon and one is a
  discourse plugin's; lobsters, solidus, chatwoot and forem are byte-identical. Every affected
  list shrinks and none empties — 102 places to 76 on nine of them, 3 to 2 on the narrowest —
  and the two that had a migration **first** now open on a real declaration in loadable code.
  Every card was *Guessed* before and after, which is the shape of the defect: a receiver with no
  type falls to the name-based list, and the migration's private copy of the model was on it.
- **The picker was measured separately, because the audit does not ask it.** 276 queries drawn
  once — the audit's own words plus the migration-declared class names — asked of both binaries:
  rows **38,696 -> 38,723**, **1,265 rows lost and 0 of them real**, 1,292 gained, 51 top tens
  reordered. Every row that left was a migration's and every corpus ends with at least as many
  rows as it began, because what the sink frees is cap space: a migration row past the 256th
  slot is a real row that now fits. Migration rows in a top ten fall **276 -> 168** rather than
  to zero, which is the rank behaving correctly — for a query that *is* a migration class' name,
  the migration is still the best match, and `workspace/symbol` must never drop.
- **`completion` is where a wrong drop would be felt, and the top ten is the only honest metric.**
  3,464 member and call cursors from the same draw: **269 lists changed, 0 emptied**, rows
  910,873 -> 910,847 over six corpora — 26 in a million. A naive before/after label diff reports
  47,340 lost against 47,314 gained, which is one reordering counted twice and says nothing; a
  same-binary control returned 0 changed, so the instrument is deterministic and the *metric* was
  the problem. Scored on the first ten rows instead: **38 lists change their top ten** (solidus 33,
  discourse 5, the other four 0) and **not one list on any corpus changes its first row**. Each of
  the **232** labels that left a top ten was then asked of `workspace/symbol` where it is declared
  at all: **all 232, over 41 distinct labels, are declared only inside a migration.** Nothing real
  left a list.
- **The four corpora that scored zero are the draw and not the shape.** lobsters really does
  declare `CreateCategories::Tag#category` inside a migration; its 777 drawn cursors never asked
  for the name. Association macros inside a migration directory run mastodon **80**, solidus 4,
  lobsters 2, and chatwoot, forem and discourse 0 — so the population is real and concentrated,
  and a zero here means *this sample did not reach it*.
- **The columns are a second half that was never visible and is not what this fixes.** lobsters
  generates **590** members onto 4 migration classes and solidus **1,613** onto 12, and almost
  all of them are columns, whose place is the `db/schema.rb` line — the same line the real
  model's column maps to, so `all_places` deduped them away long before any fence saw them.
- **The two tags are read of different sets, and that asymmetry is the rule.** `in_a_test_tree`
  is read of `own` — in `Trees` and in `completion`'s `Locality` alike — because `own` is the only
  set that knows where *this project's* trees are, so a miss has to mean *not a spec*.
  `in_a_generator_template` is read of the **whole graph**, because a gem's is the common case:
  pundit's `application_policy.rb`, jbuilder's `api_controller.rb`, AMS' `serializer.rb`, devise's
  and rolify's migrations. It costs one pass over the documents, which is the pass `own` itself is
  built with. `locator`'s rungs resolve with neither set in hand and read both paths directly.
- **Measured, two binaries from one tree, 5,859 drawn cursors on names a generator template
  declares.** `definition`: **731 lists shrank, 8 grew, 0 emptied**, and of **767 places dropped,
  767 were a generator template** — no list lost a real place. 14 distinct template files stopped
  appearing in real answers; AMS' `serializer.rb` was 506 of them. **67 positions went from two
  places to one** on `ApplicationPolicy` (chatwoot 23, forem 44), which is the row's own case.
  The 12 that had answered ***Resolved*, one place** are the root rung: eight now answer the
  model's real column out of discourse's `db/structure.sql` and four fall through to the name
  rung's guess, which is item 10's noise and not this fence's.
- **The other three surfaces, measured separately, because the audit cannot see any of them.**
  `completion` at a typed prefix in the project's own code: template rows **22 -> 7**, and all
  seven left are names Ruby's core or the project declares too. Every one of the fifteen removed
  is `Object`-owned and so was offered for *every* receiver — discourse's `id` out of AMS'
  template, chatwoot's `update` out of jbuilder's, whose card still carried an unexpanded
  `<%= route_url %>`. `workspace/symbol` over 247 queries: **6 lists changed**, 5 template rows
  sank (ranks 83 -> 125, 134 -> 150, 11 -> 22), 2 fell past the 256-row cap and 2 real rows came
  in behind them, **0 real rows lost**. `subtypes` over 480 lists and 4,753 rows: **0 changed** —
  chatwoot's two template rows are `ApplicationTool` and `SampleTool`, both template-only, which
  is the residue shape where sinking has nothing to sink below. The audit over 5,523 positions:
  **0 findings new, 0 gone, 9 counters moved**, every one of them tracing to the six
  `ApplicationPolicy` positions that went from two places to one.
- **Nothing in the six corpora requires a file under such a tree.** Checked across **115,409 gem
  library files**: every `require`, `require_relative` and `load` whose argument mentions a
  template, resolved against the gem's own `lib/`, and **0** of them land inside a
  `generators/**/templates/` tree. The load-path clause cannot say this — the tree is under the
  gem's `lib/`, so `require` *could* name it — which is the whole reason the tag reads the
  directory names.
- **`Trees` is the same question asked from the other side, and its `own` half is why
  `completion` was never wrong about a library.** The test tag is read of the workspace's own
  documents, so a gem is a miss rather than a suite — `completion`'s `Locality`, `search::rank`
  and `subtypes` all read it and all keep rack-test's `lib/rack/test/`. The template tag is read
  of the whole graph beside it, because there a gem's is exactly the case. `locator`'s rungs
  resolve without either set in hand, which is the whole reason they take the load paths instead.
  Two spellings of one rule, each where the set it needs is available.
- **The cursor gate is wider than the target tag, and deliberately.** `TEST_SUPPORT` adds
  `testing_support` for cursors only. A name on the *target* list deletes an answer when it is
  wrong; a name on the cursor list only turns the fence off, so being wrong costs nothing but the
  protection. solidus ships **119 Ruby files** under a `testing_support` segment that is in no test
  tree, and nine of the corpora's measured cursors sit in them — shared examples and factories
  under `core/lib/spree/testing_support/`, published for other people's suites. Measured, the other
  candidates earn nothing: `shared_examples` (13 files) and `factories` (66) are all already under
  `testing_support`, `fabricators` and `matchers` add 0, and **`support` would add 10 that are not
  test code at all** — discourse's `script/import_scripts/support` — which is why even the safe
  direction is not free.
- **Every other request has no question to ask, and the table is meant to be exhaustive.** The
  single-document answers — outline, folding, selection, tokens, hints, links, code actions,
  diagnostics — are about the file the cursor is in. A new surface goes in the table, not into a
  new fence somewhere else.
- **The "never" row is the one with teeth, and it is pinned by tests that say so.** A completion
  list missing a spec-only name costs a keystroke; a rename missing the spec's uses costs a red
  suite and a diff the user already accepted. `references::a_use_in_a_spec_is_a_use_and_this_list_is_never_fenced`,
  `rename::a_rename_edits_the_suite_and_is_never_fenced_by_a_test_tree` and
  `hierarchy::a_call_from_a_spec_is_an_incoming_call_and_this_list_is_never_fenced` exist to fail
  if anyone wires the fence into them. `supertypes` is in that row for a different reason: a
  module a spec prepends really is in the chain, and the chain is what Ruby reports.
- **Dropping and ranking are not a preference, they are different failures.** A sunk completion row
  still holds a slot under `MAX_COMPLETION_ITEMS` and still counts against `by_name`'s candidate
  ceiling, so sinking would bury the real rows it was meant to protect. A dropped picker row
  cannot be found at all, and neither `workspace/symbol` nor a subtypes call carries a position —
  the first has no document and the second carries the *subject's* uri, not the reader's — so
  there is no cursor to turn a fence off for the developer who is editing a spec. Rank where a
  wrong answer is free; drop where a wrong answer costs a slot.
- **The rank term goes *below* match quality in `search::rank` and immediately below `own` in
  `subtypes`.** `(own, tier, loadable, simple_len, name_len, name)` — a picker is how a name is
  looked up, so burying an exact match because it is a spec would break the only way of finding a
  spec helper. Below the tier it only rearranges candidates the query matched equally well, which
  is where every leak measured was. `subtypes` has no match quality, so there the term sits second.
- **`Tally` is the rule and there is exactly one of it.** Two cases, each easy to get wrong once:
  a declaration is kept if **any** definition is loadable (a class the suite reopens is still the
  application's class), and a declaration with **no definitions at all** is loadable. The second
  is not hypothetical — rubydex declares `Object` and `Module` with no Ruby behind them, and the
  first spelling of this rule was a running `bool` starting at *not loadable*, which dropped the
  top of the object model out of every list drawn outside a test tree. `locator::loadable_from`
  had the same latent bug for a release and was fixed by being routed through `Tally`.
- **`Trees` is built from `own`, not from the graph.** It is consulted once per *definition* of
  every candidate and a bundle has six figures of them, so it is a `HashSet<UriId>` lookup rather
  than a path split. Building it from `own` is also what makes a gem's own `test/` directory none
  of this rule's business: a miss means *not a spec*, not *unknown*.
- **`fenced_from` is the cursor gate, and a cursor with no path fences nothing.** The fence needs
  evidence to fire. The two dropping surfaces asked this separately before and answered it
  differently in that one case.
- **`preferred_definition` is a tie-break, not a fourth verdict.** Where a declaration is written
  in both environments, the row points at the copy the application loads. A declaration written
  *only* in a test tree still gets its row, pointing into the test tree — a row that points
  somewhere beats a row that points nowhere, and whether it should be listed at all was already
  decided above. It matters wherever the application's path sorts after the spec's, which is every
  engine monorepo: `definitions_of` orders by uri and `admin/spec/` precedes `core/app/`.
- **The audit cannot see any of this, and that is the instrument.** Its completion lane scores
  whether a typed receiver's member is present and where it ranks, and `Distance` already sorts
  `Object` last; no lane scores a row for *where it was declared*, and no lane asks
  `workspace/symbol` or the type hierarchy at all. A sweep that reports zero moved counters is
  reporting on a question it does not ask: the picker re-ranking below read *0 findings new, 0 gone,
  0 counters moved* over 5,523 positions while a probe written for it counted 1,304 rows moving.
  **Run it anyway** — a quiet sweep is the only evidence that a change aimed at a surface it does
  not ask for did not break one it does. Then measure the surface directly, two binaries from one
  tree, as the numbers below were.
- **Measured, two binaries from one tree, 52–80 picker queries and 20–40 subtype lists per
  corpus.** `workspace/symbol`, rows in a test tree, before -> after:

  | corpus | rows in the capped list | in a top ten | first hit is a spec |
  |---|---|---|---|
  | lobsters | 40 -> 34 | 5 -> 2 | 0 -> 0 |
  | solidus | 482 -> 258 | 34 -> 11 | 3 -> 2 |
  | mastodon | 283 -> 209 | 41 -> 24 | 3 -> 2 |
  | chatwoot | 93 -> 68 | 9 -> 2 | 0 -> 0 |
  | forem | 322 -> 215 | 17 -> 6 | 3 -> 0 |
  | discourse | 1,737 -> 865 | 73 -> 29 | 12 -> 2 |

  **The churn at `MAX_WORKSPACE_SYMBOLS` ran one way only**: 1,304 rows fell out of the 256 a
  query returns and every one of them was in a test tree; of the 1,313 that came in, 8 were.
  A rank that only reordered would have moved none — this is the cap spending its budget on
  different candidates, which is the half of the change that actually returns rows to the user.
  The residue is a query no loadable declaration matches at that tier at all (mastodon's `belo`
  finds `Object#below_limit` and nothing else), which is a rank behaving correctly rather than a
  leak.
- **What it costs, bounded.** The cap now spends its 256 rows differently, so on a big project a
  short query can push a spec-only name past the fold that used to be inside it — 511 such rows on
  discourse for a four-character query. The bound that matters is the one a person actually
  types: across the six corpora **16 rows were an exact match on a test-tree declaration's own
  name, and 0 of them were lost**. Tier is above `loadable`, so typing the name brings it back;
  what is now harder to reach by a four-letter guess was a guess either way.
- **The subtype lists lost and gained nothing, which is what a pure re-order looks like.** solidus
  went from 5 test rows in a top ten to 0 and from 1 list led by a double to 0; discourse from 23
  to 19 and from 5 to 2. Its residue is base classes that are themselves declared under `spec/` —
  every subclass of a Capybara page object is a page object — where sinking has nothing to sink
  below. `BaseImporter` is the case the term was written for: `MockGitImporter` led the list
  alphabetically and now sits under the three real importers.
- **`preferred_definition`'s tie-break moved 3 rows, all on solidus, all engine monorepo.** A
  `Spree::PromotionHandler` row pointed into `legacy_promotions/spec/` and now points at
  `legacy_promotions/app/`. Small, and worth having for the shape rather than the count: it is the
  only one of the four mechanisms that changes where a click *lands* rather than what a list
  contains.
