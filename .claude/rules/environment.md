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

rubydex has one graph and no notion of environments. `analysis/environment.rs` is the only module
that knows the difference. Every surface reads it, never its own copy of the rule.

## Two meanings, kept as two bits (`Gates`)

1. **Trees the application never loads**: test trees, migrations and generator templates.
   `Fence::unloadable` is where they meet.
2. **Documents outside the project** (`Layout::is_outside`): scratch files and unsaved buffers.

A different cursor turns each one off, so they must never share a boolean. `fenced_from` returns
the pair. `Fence::at` takes both, `Fence::uses` takes the second alone, and every caller says which
it means. There is no `Fence::off`.

## The verdict table (tree meaning)

This table is exhaustive: a new surface goes into it.

| Verdict | Surfaces | Why |
|---|---|---|
| **drop** | `completion`; the name and root rungs of `definition`/`hover`; `locator::places`; `signatureHelp`; `outgoingCalls` | "What can I call from here?" A name the app can't load is not an answer |
| **rank** | `workspace/symbol`; `subtypes` | "Find me this." A drop would make it unfindable |
| **never** | `references`, `rename`, `documentHighlight`, `incomingCalls`, `supertypes` | "Where is this used?" A use under `spec/` is a use |

- **Rank where a wrong answer is free; drop where it would cost a slot.** Ranked rows still fill
  `MAX_COMPLETION_ITEMS` and `by_name`'s ceiling.
- **The rank term is `loadable`:** below the tier in `search::rank`, and second (after `own`) in
  `subtypes`.
- **The *never* row is pinned by tests:**
  - `references::a_use_in_a_spec_is_a_use_and_this_list_is_never_fenced`
  - `rename::a_rename_edits_the_suite_and_is_never_fenced_by_a_test_tree`
  - `hierarchy::a_call_from_a_spec_is_an_incoming_call_and_this_list_is_never_fenced`
- **Single-document requests** (outline, folds, tokens, hints, links, code actions, diagnostics) have
  no question to ask.

## The three tree tags

| Tag | Rule | Needs `Layout`? |
|---|---|---|
| `in_a_test_tree` | `TEST_TREES` = `spec`, `test`, `tests`, `features`, matched as path **segments**, never a prefix (solidus has `core/spec/` and `spec/dummy/`) | Yes. A gem's `lib/rack/test/` is a library. Asks "inside the project and not `require`-able?" |
| `in_a_migration` | A `migrat` **substring** in a directory whose parent is `db` (`db/migrate`, `db/post_migrate`, `db/old_migrations`) | No root clause. Keeps the load-path escape hatch |
| `in_a_generator_template` | A `templates` segment somewhere after a `generators` segment | No. It is configurable by nothing |

- **Either name alone deletes real code:** `lib/yard/templates/` holds real classes, and
  `generators/` is required by `rails generate`.
- **Tagged trees stay indexed**, so the *never* row still finds their uses.
- **The cursor gate is wider than the target tag.** `TEST_SUPPORT` adds `testing_support` for cursors
  only. Being wrong there only turns protection off. Don't add `support`, which catches non-test code.
- **A cursor in a migration turns the migration fence off.** Ruby really does resolve `Account` to
  the file's private copy.
- **Which set each tag reads is the rule:** test tags read `own`, and template tags read the whole
  graph (a gem's template is the common case). `locator`'s rungs read paths directly.

## Configuration: `Names`, inside `Layout`

- **`Names` carries `[trees]` (`features.md`) and defaults to the built-in lists**, never to empty
  ones. `Some(&[])` means off, which differs from `None`.
- **A surface that fences holds one value**, so the halves of a fence can't drift apart.
  - `Trees::of` and `preferred_definition` take `Names`.
  - `fenced_from` and `indexed::Placed::of` take the whole `Layout`.
- **Tags are computed once per settle in `indexed::Placed`, never per request.**
  `Indexed::forget_placement` runs on config reload and after gem discovery. Inside the predicate,
  check the three short prefix lists before the long load-path list.

## The outside meaning

1. **It is a prefix test.** No word names a scratch tree. "Inside" means under the root, a
   `require` load path, an outside `[index] load_paths` entry, or a gem, RBS or Ruby root. **A gem is
   not outside.** An empty root fences nothing.
2. **Its verdict is *drop* everywhere, including the rank surfaces and `locator::resolve`.**
3. **`Outward` holds the cursor:** `Unfenced`, `Project`, or `Alone(uri)`. The whole predicate
   `Outward::fences` is `uri != cursor && layout.is_outside(uri)`. An outside document reaches the
   project and itself, never another outside document.
   - `documentLink`'s `requests::require_site` takes no fence, so `require_relative` still links.
4. **Surfaces with no cursor keep the gate on.** The `prepare`s and hierarchy expansions thread one
   through `hierarchy::rooted_in`. `workspace/symbol` always drops outside documents, and does so
   before the cap (`a_row_outside_the_project_is_dropped_before_the_cap_and_not_after_it`).
5. **Strip `ya-lsp-generated:` before any prefix test.** A scheme in front of a URI is not a
   directory.

## Unsaved buffers (`untitled:`)

- **Admitted only when `didOpen` says `languageId: "ruby"`.** They are outside for free, having no
  prefix.
- **Refused by `didChangeWatchedFiles`, and by `rename`** (not the user's own code).
- **Diagnostics are the one deliberate opening.** `collect_diagnostics` publishes for own code *or*
  an open outside buffer. Never widen `is_own_code` itself. `didClose` clears them.

## The root rung (`Object`, `Module`, `Class`)

- **Fence it: a hit there answers every receiver**, and spec-top-level `def`s land there.
  `resolve_typed` fences only imprecise answers, so the root arm is fenced separately.
- **Read only directory names there** (`Fence::loadable_on_a_root`), never the layout. Loosening a
  fence here is where being wrong costs the most.
- **A fenced jump falls through to the name rung. A fenced card, `signatureHelp` or `outgoingCalls`
  shows nothing.**
- **The block-only-root gate is separate** (`navigation.md`). Both gates apply.

## Rules that are easy to get wrong

- **`Tally` is the one "is it loadable?" rule.** Keep a declaration if **any** definition is
  loadable. A declaration with **no** definitions (`Object`, `Module`) is loadable.
- **`locator::places` judges each *definition*.** Drop test-tree places unless none survive, or unless
  the cursor itself is in a test or `testing_support` tree.
- **`preferred_definition` is a tie-break:** prefer the loaded copy. It refuses an outside place,
  except for the rooted document.
- **A cursor with no path fences nothing.**
- **Known gap:** `completion` and the picker read `Trees`, which has no `Layout`. A tree put on a
  load path gets its jumps back but not its completion rows.

## Measuring a change here

- **The audit cannot see ranking.** No audit lane asks `workspace/symbol` or the hierarchy. Run the
  sweep anyway, then measure the surface directly with two binaries built from one tree.
