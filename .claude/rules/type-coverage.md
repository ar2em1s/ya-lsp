---
paths:
  - "src/analysis/coverage.rs"
  - "src/main.rs"
---

# `ya-lsp coverage`

A command, not a server. `coverage::run` builds an `Analysis` with no client, runs the pipeline out
(workspace, bundle, generator pass) and asks. Nothing on the protocol changes, and the running
server is never involved.

## Must

1. **The question is the typing probe's** (`tmp/bench/callsites.py`): `cursor::type_of` →
   `types::method_receiver`, Resolved or Derived and spellable, one fresh `Memo` per call, as one
   hover request would ask. A second meaning of *typed* would make the number disagree with the
   probe and with the margin.
2. **The population is `Layout::is_own` less `Fence::unloadable`**, `.rb` files only: the fence's
   own rule, never a second list of test folders (`environment.md` lists this as a drop surface).
   Nothing here names a framework.
3. **`used_calls` is Ruby's reading of where a value goes**, pinned by
   `a_call_counts_where_ruby_reads_its_value`. It knows no framework either: a controller action's
   last statement counts, which the probe's `typedefs_defs.rb` left out.
4. **The draw is seeded and ours** (`sample`, SplitMix64): two runs over one tree ask the same
   calls. No `rand` dependency, so a bump cannot move it.
5. **The error is Agresti–Coull with the finite-population correction, at 95%**
   (`Coverage::margin`). A census (no more calls than `SAMPLE`) prints no `±`.
6. **stdout carries the one result line; everything else goes to stderr.** This is the one mode
   whose stdout is not an LSP transport, and it runs only when the first argument is `coverage`.
   Logs only where `YA_LSP_LOG` asks. A terminal gets counters rewritten in place; anything else
   gets one line per phase, so a CI log stays short.
7. **A panic in one call is that call untyped** (`typed_at`), as `Analysis::serve` contains a
   request's.

## Measured (2026-09-30, on battery)

The sample against a census build (every call asked) over the six corpora: every sample within its
error, the widest at 45.6% ± 2.1% against 47.3%. A whole run, bundle indexing included, took
2.1–6.0 s and 0.46–1.19 GB; the largest corpus's census 44 s (`tmp/bench/v070/coverage-cmd/`).
