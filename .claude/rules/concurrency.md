---
paths:
  - "src/analysis/threaded_tests.rs"
  - "src/analysis/mod.rs"
  - "src/analysis/indexer.rs"
  - "src/analysis/testing.rs"
  - "src/server/watcher.rs"
  - "src/server/mod.rs"
---

# The run loop, and how it is tested

## Where a test goes

1. **`threaded_tests.rs` is the only place `Analysis::run` executes.** Ordering invariants go there:
   the debounce, gem indexing yielding, `Cancellations`, and answer order.
2. **Everything else uses `testing.rs`'s `Harness`**, which calls `analysis.handle(task)` on the test
   thread. Tests of an answer's shape go next to their module.
3. **Assert positions in the message stream (`a < b`), never elapsed time.**
4. **The one exception is whole-file requests**: `semanticTokens/full`, `documentLink` and a
   whole-document `inlayHint`. They bound what the request queued *behind* them waits, with a
   twentyfold ceiling.
5. **Count claims about work; never time them** (`cursor::CLASSIFIED`, `hints.md`).

## Test mechanics

- **The fixture carries the margin, and it only works one way.** A slower machine still passes. Only
  a *cheaper* fixture breaks the test. The gem is 1,200 files (12 batches), and the debounce test
  asks 24 questions.
- **`Threaded` joins in `Drop`.** The harness never holds a clone of the `Sender`, or the join waits
  forever.
- **Don't wait for a `$/progress` report.** They are rate-limited (`MIN_REPORT_INTERVAL`, 250 ms). Wait
  on `begin`.
- **The crash stand-ins exist because the pinned rubydex has no real crashing input:**
  - `indexer::CRASHES` is a Ruby comment in the source, recognised by `crash_if_asked`.
  - `SOURCE_INDEXES_TO_CRASH` and `RESOLVES_TO_CRASH` are thread-local counters.
  - `REQUESTS_TO_CRASH` covers `dispatch`, and `Task::Panic` (`#[cfg(test)]`) covers join.
  - The Ruby that used to crash is still asserted, now to *work*. Look for a real input before adding
    another stand-in.
- **Keep `threaded_tests.rs` able to reach `thread.join().is_err()` and the already-past deadline.**
  No other test reaches those two arms in `mod.rs`.
- **Assert "indexed" with `definitions_in`, not `has`.** Declarations only exist after resolve.

## Progress streams

- **Two tokens: gem indexing, and `ya-lsp/generate` (`Analysis::generate`).** They are adjacent in
  time, and a shared token would close twice.
- **`generate` closes after `refresh_hints`.** Its claim is "answers are not final yet".
- **A stream closes itself on `Drop`, guarded by `ended`, with the message `interrupted`.** A panic
  caught by one of the three bulkheads would otherwise leave a spinner forever.

## Keystrokes: defer the index, rebase the cursor

1. **`didChange` records the edit and indexes nothing.** `completion`, `hover` and `definition` answer
   against the last settled graph, through a `Rebase`. A refused offset settles and retries, so
   deferring never makes an answer worse. **Not a jump that found its member with no place**
   (`Analysis::unplaced`, RSpec's `describe`): a settle finds the same nowhere, and paying one per
   ask cost the audit 193 settles. It answers the last settled graph, as a placed member does.
   **Not a document just opened whose own facts wait on the settle** either (`Analysis::reopened`,
   `requests::reopens`): the last settled graph never held a spec's groups, so a non-empty answer
   there is a name guess one settle short of the right one. It settles first, once, the cheap
   reopening kind. Held by `a_spec_just_opened_is_answered_after_its_groups`.
2. **`indexed_text` records what the index *accepted*.** After a contained panic the graph keeps its
   old version, and so must the map. It is recorded even when indexing is skipped.
3. **Everything that becomes a graph key goes through the map, including a `Receiver`.** An
   untranslated receiver degrades to a plausible name guess that `answered_nothing` can't see. Held by
   `a_deferred_card_on_an_instance_variable_keeps_the_type_its_assignment_gives_it`.
4. **Graph → buffer goes only through `Rebase::span_to_buffer`, which takes a *span*.** A caret needs
   an unchanged byte on each side (`map`). A span's start and end each lean toward their own side.
   `map` is defined as the agreement of the two one-sided maps. Without this, every `module Foo` at
   offset 0 loses its place while you type.
5. **Map a span coming out of the graph by its own document's map.** `Analysis::link` does this for
   jumps. `references`, highlights and hierarchies don't, which is why they are not in `defers`.

## Settle, debounce and the cold start

- **`RESOLVE_DEBOUNCE` is one constant, chosen to be far from typing pace.** A settle landing
  between two keystrokes is the worst case. Per-workspace scaling was tried and dropped. The cost:
  diagnostics arrive later after typing stops.
- **Never index eagerly and resolve later.** `consume_document_changes` leaves the class with no
  members until the resolve runs.
- **`mark_dirty_for` stays with the edit.** `settle` calls `index_pending` before clearing `dirty`.
- **`pending_index` holds URIs, never text.** A buffer closed before its deferred index keeps what
  is on disk (`a_buffer_closed_before_its_deferred_index_ran_keeps_what_is_on_disk`).
- **Skip text the graph already holds** (`graph_holds`, which compares `xxh3_64` with
  `Document::content_hash`). This saves the parse and the member-index rebuild.
  - Exception: a document on the skip list is retried anyway.
- **The debounce does not outrank a gem batch.** While `step_bundle` has work and the queue is
  empty, the loop keeps indexing. `push_diagnostics_for_an_edit_wait_for_the_background_index` pins
  that choice.
- **A cold server drains before answering:** `while self.step_bundle() {}`, then settle, then dispatch.
  - Drain, never wait, because the work runs on this thread.
  - Key on `gem_work.is_some()`, not on `dirty`.
  - It applies to every graph request, not only `defers`.

## The watcher thread

1. **Drop `Watching` before `AnalysisHandle::join`.** `serve` does this by hand. The collector holds a
   `Sender`.
2. **`Watching::drop` drops the stop sender before joining.** Both are `Option`s, taken in that
   order.
3. **Arm the watcher on the collector thread.** `watch` returns immediately, so tests wait for a
   probe (`watcher::tests::armed`).
4. **What is watched comes from the walk's own list** (`workspace::watched_directories`).
5. **macOS reports canonical paths.** `Collector::spell` restores the root's spelling. Tests
   deliberately don't canonicalize their root.

- The watcher's config is a startup snapshot. `Workspace::indexes` on the live config decides what
  gets indexed.
