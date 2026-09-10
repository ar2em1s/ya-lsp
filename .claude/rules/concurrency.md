---
paths:
  - "src/analysis/threaded_tests.rs"
  - "src/analysis/mod.rs"
  - "src/analysis/indexer.rs"
---

# The run loop, and how it is tested

- **`analysis/threaded_tests.rs` is the only place `Analysis::run` is executed.** `mod tests` calls
  `analysis.handle(task)` on the test's own thread, which can ask what an answer *is* and nothing
  about what happens when two things arrive at once. The debounce deadline, the `receiver.is_empty()`
  check that makes gem indexing yield, the shared `Cancellations` set and the order requests are
  answered in are all unreachable from there. A new invariant about *ordering* belongs in
  `threaded_tests.rs`; one about an answer's *shape* belongs in `mod tests`, which is faster to read.
- **Assert positions in the message stream, never elapsed time.** `Threaded` keeps every message the
  thread sent, in order, and every assertion is `a < b` over that list. A latency assertion would
  flake on a shared runner and would say nothing a position does not.
- **The one exception is `semanticTokens/full`, and it is a ceiling rather than a measurement.** A
  whole-file answer at typing speed on a loop that serialises everything is a head-of-line question,
  and this harness is the only thing that can ask it. What is bounded is **what the request behind it
  waits**, not its own latency: a cheap request goes into the queue *with* it, before the thread has
  looked at either. The bar is twentyfold, like the canary's.
- **The margin lives in the fixture, and it only runs one way.** Two tests need the thread still busy
  when the next thing lands, so the fixture carries several times the work the test consumes: the gem
  is 1,200 files (twelve batches; twelve round trips fit, the test asks four), and the debounce test
  asks 24 folding questions against a deadline falling on the fourth. Check the direction before
  changing either — a *slower* machine reaches the deadline earlier and still passes, so the only way
  to break these is to make the fixture cheaper.
- **`$/progress` reports are rate-limited to 250 ms, so do not wait for one.** `progress.rs` swallows
  any report inside `MIN_REPORT_INTERVAL` of the last, so a fixture whose whole gem index takes less
  than that sends `begin` then `end` with nothing between. Waiting on `report` cost a 30-second
  timeout and a fixture three times too big. Wait on `begin`, and prove the interleaving with round
  trips.
- **`Task::Panic` is `#[cfg(test)]`, a stand-in the way `crash_the_next_resolve_if_asked` is.** The
  panic `AnalysisHandle::join` reports is by construction the *unforeseen* one, so there is no input
  that provokes it; what is worth pinning is that a dead thread is noticed rather than joined in
  silence. Do not reach for `RESOLVES_TO_CRASH` here — it is a `thread_local`, deliberately, so
  parallel tests cannot arm each other's crashes, which also means the test thread cannot arm the
  analysis thread's.
- **Every bulkhead seam is tested with a stand-in, because the pinned rev leaves no real input.** A
  scan that found a panic on the previous rubydex release finds **none** at the pinned rev. **The
  seams stay**, because `create_declaration`'s two unwraps are still on upstream's `main`: a version
  retires an instance of the hazard, not the class.
- **The stand-in is the *text* for six routes and a counter for the seventh.** `indexer::CRASHES` is a
  Ruby comment `crash_if_asked` recognises in the source, because `index_files` builds on worker
  threads and a `thread_local` cannot reach one; `indexer::SOURCE_INDEXES_TO_CRASH` is the counter,
  for the one route whose text no fixture writes. `REQUESTS_TO_CRASH` in `analysis/mod.rs` is the
  same shape for `dispatch`. **The Ruby that used to provoke each is still a fixture and still
  asserted, the other way round** — `extend_self_in_an_anonymous_module_indexes_like_any_other_file`
  and `a_constant_alias_reopened_under_its_alias_answers_rather_than_crashing` — so a pin moved back
  to 0.2.5 fails loudly there rather than quietly killing a test thread inside a guard. Before adding
  a fourth stand-in, look for a real input: the measurement is what earns the exception.
- **The debounce does not outrank a gem batch — a trade, not an oversight.** `step_gem_indexing`
  returning true `continue`s, so while there is background work and an empty queue the loop never
  reaches `resolve_at`. An edit during a cold start reaches the *buffer* at once, but its index and
  the diagnostics its debounce armed are both the settle's and wait for the bundle — bounded by it,
  short-circuited by any request. A completion meanwhile answers over the map. Reversing the priority
  costs one resolve per `RESOLVE_DEBOUNCE` of typing against a mid-index resolve that is not cheap,
  so it wants a number from a real bundle first.
  `push_diagnostics_for_an_edit_wait_for_the_background_index` is where the current answer is written
  down; a change of mind fails there and nowhere else.
- **A keystroke's index waits for the settle; the three requests a caret asks answer without it.**
  `didChange` records the edit and indexes nothing; `completion`, `hover` and `definition` answer
  against the last settled graph and translate the cursor with a `Rebase`. **The map is an
  optimization, not a filter**: an offset it cannot translate is refused, and a refused request
  settles and asks again, so a deferred answer is never *less* than an eager one. Measured at
  typing speed, completion during typing goes from a stall you can feel to one you cannot, and the
  long tail disappears entirely. Two rules hang off it, each a way to be silently wrong:
  - **`indexed_text` records what the index *accepted*, never what it was handed.** A panic contained
    by the bulkhead costs the document its update and the graph goes on answering with the version it
    held — so writing the new text there would make the map compare two equal strings, answer
    `identity`, and hand offsets to a graph an unknown number of edits behind, with no refusal to
    fall back from.
  - **A span coming *out* of the graph is mapped by the map of the document it is in**, which is not
    always the one asked. `Analysis::link` does that for every jump target; `references`, the
    highlights and the hierarchies do not, which is exactly why they are not on `defers`' list.
- **Why the index is the expensive half.** `didChange` does two things and only the second costs
  anything: applying the edit to `open` is microseconds; putting a heavily-referenced document into
  rubydex costs hundreds of times more, because the cascade is a function of how much of the graph
  names the document. The four requests `needs_the_graph` exempts read the buffer and never the
  graph, and an editor sends `semanticTokens/full` after every keystroke, so they were waiting on an
  index they do not read: **a stall per keystroke on a large workspace, imperceptible after.**
  - **Deferring the index needs both halves, and the debounce is the one easy to miss.** A settle on
    the largest corpus takes longer than the gap between two keystrokes and cannot be interrupted,
    so with a short debounce it is still in flight when the next request arrives, and **deferral
    alone makes things worse**. What makes it pay is the map *plus* a debounce long enough that the
    settle falls outside the burst.
  - **An eager index with a deferred resolve is the worst of both and must never be reinstated.**
    `consume_document_changes` invalidates the edited document's declarations and only
    `Resolver::resolve` puts them back, so the graph holds a class with **no members at all** rather
    than a stale one — measured as `candidates=0` where the settled graph gives a full list.
  - **Back-to-back timings overstate what a user feels**: the largest corpus measures a completion
    roughly three times as expensive that way as when a human types.
- **The resolve debounce is one constant, `RESOLVE_DEBOUNCE`.** The whole settle waits for it — the
  buffer's index included — so a settle firing mid-burst is the only thing a completion can wait for,
  and the cost of one is `debounce + settle - gap`. A debounce well *shorter* or well *longer* than
  the typing pace both drive that to near zero; the worst case is a debounce close to the pace
  itself, where the settle fires precisely between two keystrokes, and that is what the current value
  is chosen to avoid across every corpus. Scaling it per workspace was built, measured and abandoned,
  because no stable signal picks the lower regime: a settle costs what the *last* edit made it cost.
  The trade is stated rather than hidden: **diagnostics arrive noticeably later after the typist
  stops.**
- **`mark_dirty_for` stays with the edit, not with the index it defers.** A graph request arriving in
  between would otherwise find `dirty` false and answer without the edit at all. For the same reason
  `settle` calls `index_pending` **before** clearing `dirty`, since `index_buffer` marks it again.
- **`pending_index` holds URIs and must not carry the text.** A buffer closed between the edit and the
  step is skipped rather than replayed: `didClose` has already put the file back to what disk says,
  and the text that would be replayed is a version the user abandoned. Carrying the text is the
  obvious way to save the `open` lookup and it makes that skip impossible;
  `a_buffer_closed_before_its_deferred_index_ran_keeps_what_is_on_disk` is what fails.
- **There is deliberately no latency assertion for the deferral** — a limit of the fixture, not the
  harness. The cost removed is a property of the graph, so a one-file fixture cannot produce it:
  measured on `threaded_tests`' own large fixture the difference is real but far under any non-flaky
  ceiling. A ceiling there would look like a guard and hold nothing. What is
  asserted instead is the mechanism, in `mod tests` where the graph can be inspected —
  `definitions_in` and not `has`, because a *declaration* is built by `Resolver::resolve` and an
  indexed-but-unresolved document has none, so `has` reads the same as never indexed at all.
- **`Threaded` joins in `Drop`, and that is not tidiness.** A test failing an assertion unwinds with
  the thread still indexing and the `TempDir` about to be deleted underneath it, turning one legible
  failure into a second unrelated one. For the same reason the harness never holds a clone of the
  `Sender`: `AnalysisHandle::join` drops its own and waits, and a second live sender would mean
  waiting forever.
- **`threaded_tests.rs` is the only thing reaching two arms in `analysis/mod.rs`** —
  `thread.join().is_err()` and the already-past deadline, worth 0.31 of a point of branches. A rewrite
  of `run` that loses those tests loses the arms with them.
