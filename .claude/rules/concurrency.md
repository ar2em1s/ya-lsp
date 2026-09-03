---
paths:
  - "src/analysis/threaded_tests.rs"
  - "src/analysis/mod.rs"
---

# The run loop, and how it is tested

- **`analysis/threaded_tests.rs` is the only place `Analysis::run` is executed.** `mod tests`
  calls `analysis.handle(task)` on the test's own thread, which can ask what an answer *is* and
  nothing about what happens when two things arrive at once. Everything the loop is made of —
  the debounce deadline, the `receiver.is_empty()` check that makes gem indexing yield, the
  shared `Cancellations` set, and the order requests are answered in — is unreachable from
  there. A new invariant about *ordering* belongs in `threaded_tests.rs`; a new invariant about
  an answer's *shape* belongs in `mod tests`, which is faster and easier to read.
- **Assert positions in the message stream, never elapsed time.** `Threaded` keeps every message
  the thread has sent, in order, and every assertion is `a < b` over that list. A latency
  assertion would be a flake on a shared runner and would say nothing a position does not: "the
  diagnostics arrived while the questions were still being answered" is the claim, and it is
  exactly what the arm at the top of `run` decides. The only latency bar in this repository is
  the canary's ceiling, and it is twentyfold for the same reason.
- **The margin lives in the fixture, and it only runs one way.** Two tests need the thread to
  still be busy when the next thing lands, so the fixture carries several times the work the
  test consumes: the gem is 1,200 files (twelve batches, twelve round trips fit, the test asks
  four), and the debounce test asks 24 folding questions against a deadline that falls on the
  fourth. Check the direction before changing either — a *slower* machine reaches the deadline
  earlier in the sequence and still passes, so the only way to break these is to make the
  fixture cheaper.
- **`$/progress` reports are rate-limited to 250 ms, so do not wait for one.** `progress.rs`
  swallows any report inside `MIN_REPORT_INTERVAL` of the last one, which means a fixture whose
  whole gem index takes less than that sends `begin` and then `end` with nothing between. Waiting
  on `report` cost a 30-second timeout and a fixture three times too big before that was the
  answer rather than the file count. Wait on `begin`, and prove the interleaving with round trips.
- **`Task::Panic` is `#[cfg(test)]`, and it is a stand-in the way `crash_the_next_resolve_if_asked`
  is.** The panic `AnalysisHandle::join` reports is by construction the *unforeseen* one, so
  there is no input that provokes it and nothing to reproduce; what is worth pinning is ya-lsp's
  half — that a thread which died is noticed rather than joined in silence. Do not reach for the
  existing `RESOLVES_TO_CRASH` here: it is a `thread_local`, deliberately, so that tests running
  in parallel cannot arm each other's crashes, which also means the test thread cannot arm the
  analysis thread's.
- **The debounce does not outrank a gem batch, and that is a trade rather than an oversight.**
  `step_gem_indexing` returning true `continue`s, so while there is background work and an empty
  queue the loop never reaches `resolve_at`. An edit made during a cold start is indexed at once,
  but the diagnostics its debounce armed wait for the bundle — bounded by it, and short-circuited
  by any request, which is why nobody has seen it. Reversing the priority costs one resolve per
  150 ms of typing against a mid-index resolve measured at ~100 ms p90, so it wants a number from
  a real bundle first. `push_diagnostics_for_an_edit_wait_for_the_background_index` is where the
  current answer is written down; a change of mind fails there and nowhere else, which is the
  point of pinning it.
- **`Threaded` joins in `Drop`, and that is not tidiness.** A test failing an assertion unwinds
  with the thread still indexing and the `TempDir` about to be deleted underneath it, which turns
  one legible failure into a second unrelated one. For the same reason the harness never holds a
  clone of the `Sender`: `AnalysisHandle::join` drops its own and waits, and a second live sender
  would mean waiting forever.
- **This item is the only one whose completion is visible in the coverage number.** It took the
  last two arms in `analysis/mod.rs` that M8 could not reach — `thread.join().is_err()` and the
  already-past deadline — and moved branches from 96.60% to 96.91%. Both are still the only way
  those arms are taken; a rewrite of `run` that loses them loses the arms with them.
