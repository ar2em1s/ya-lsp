---
paths:
  - "scripts/canary.py"
  - "Makefile"
  - ".github/workflows/**"
---

# The canary

- **It is a canary, not a benchmark, and the difference decides every number in it.** A benchmark
  tracks a value and gets edited when the value moves; a canary answers one question — does opening
  a real application still work at all? — and only ever fails on a change in kind. So the index
  ceiling is 500 ms against a 20.99 ms measurement, roughly twenty-fold, because the failures it
  exists to catch (an accidental quadratic, a discovery rule that stops matching, a parser
  regression) move the number by a factor while a shared CI runner with a cold page cache moves it
  by a small multiple. **A canary that flakes is worse than no canary**, because the first thing a
  flaky check earns is a habit of ignoring it.
- **The counts are exact and the timing is not, and that asymmetry is the design.** 476 files, zero
  `parse-error`, 14 `parse-warning` and *no other code at all* are properties of the pinned commit
  and of ya-lsp, not of the machine: they are the same on an M-series laptop and on a shared
  runner, so there is no reason to leave slack in them. Time is the only quantity a runner can
  change, and it is the only one with a ceiling rather than an equality.
- **The exact-code assertion is the one that catches a decision, not a defect.** `parse-error` and
  `parse-warning` are the only two rules that ship on, and the reason is in `diagnostics.rs`: the
  other eight are rubydex saying *it* gave up on legal Ruby. Turning `dynamic-ancestor` on in this
  workspace produces **219 warnings across 41 files** of working code, which is the same result
  solargraph gave at 384 and the reason the default is `Off`. If a code the canary has never seen
  starts appearing, the thing to re-decide is the default — not the number in the `Makefile`.
- **The file count is read out of a log line, because no request answers it.** `workspace/symbol`
  answers with symbols and a cap; nothing on the wire says how many files are in the index. So the
  driver reads `analysis::Analysis::index_workspace`'s `indexed N files in T` at INFO, which is
  also where the cold index time is. That makes a format string a contract, so the driver pins its
  shape and fails with `LOG SHAPE` naming the function — a reword produces a legible failure
  instead of a canary that quietly measures nothing.
- **Diagnostics are state, not events.** The server republishes a URI whenever its set changes and
  sends an empty list to clear one, so counting notifications double-counts every file it touched
  twice. The last publish for each URI wins, and the totals come from that map at the end.
- **The gem half is not covered and the job says so.** Resolving lobsters' bundle needs
  `bundle install`, which needs Ruby 4.0.0 and a hand-built `sqlite3` — a large amount of CI for a
  project whose headline is that it needs no Ruby. The run leaves the gem settings at their
  defaults, so it finds 53 of 176 gems on the machine this was written on and none at all on CI,
  and *nothing asserted depends on which*: `collect_diagnostics` filters to own code before it
  groups, and the index line is written before gem indexing starts. The gem numbers stay manual —
  `benchmarking.md`. Do not read a green tick here as covering them.
- **It is cloned, never vendored, and that is what makes the licence free.** lobsters is
  BSD-3-Clause; its conditions are notice retention on redistribution. No artifact ya-lsp ships
  contains any of it, so no notice is owed — `licensing.md`'s "the rule is per artifact, not per
  repository". That is a property of *how it is used*, so it has to be written where the clone
  happens: copy one file out of it into `tests/`, or cache a tarball in this repository, and the
  obligation attaches and `THIRD-PARTY-NOTICES.txt` is where it goes.
- **The SHA is pinned for the same reason the tool versions are.** An unpinned target makes every
  number it asserts meaningless across runs, and turns the canary into a flake the first time
  upstream commits.
- **Ask git about `$(CANARY_DIR)/.git`, never about `git -C $(CANARY_DIR)`.** The workspace lives
  under `tmp/`, inside this repository, and **git searches upwards**: `git -C tmp/x rev-parse
  --git-dir` in an empty `tmp/x` succeeds and answers about *ya-lsp*. The obvious spelling of "is
  this a repository yet?" therefore skips the `init`, adds a remote to ya-lsp, fetches lobsters
  into ya-lsp's object store, and runs `checkout --detach` on the working tree being developed in.
  It was written that way once and what stopped it was an unrelated dirty tree, not the check.
  `[ -e "$dir/.git" ]` cannot walk up; the toplevel comparison refuses the root itself.
- **`make canary` is deliberately not in `make ci`.** Everything in `ci` is hermetic — the source
  tree and nothing else — and a target that fails on a plane teaches people to skip it. CI runs the
  canary as its own job, where the name in the checks list says what it covers.
