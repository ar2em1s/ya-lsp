---
paths:
  - "scripts/canary.py"
  - "Makefile"
  - ".github/workflows/**"
---

# The canary

- **It is a canary, not a benchmark, and that decides every number in it.** A benchmark tracks a
  value and gets edited when the value moves; a canary answers one question — does opening a real
  application still work at all? — and fails only on a change in kind. So the index ceiling is a
  large multiple of the measurement, not a tight bound: the failures it exists to catch (an
  accidental quadratic, a discovery rule that stops matching, a parser regression) move the number
  by a factor, while a shared CI runner with a cold page cache moves it by a small multiple. **A
  canary that flakes is worse than no canary** — the first thing a flaky check earns is a habit of
  ignoring it.
- **The asserted counts live in the `Makefile`, and nowhere else.** Do not restate them in prose;
  a second copy goes stale and the stale copy is the one somebody reads. The file count has moved
  twice, and both times it was the point of the change: once when templates were indexed
  (`erb.md`), once when the default `index.include` grew to cover Ruby not named `.rb` —
  `Rakefile`, `Gemfile`, `config.ru`, `lib/tasks/*.rake`, a `.gemspec`. Moving this number is a
  decision, argued on its own terms, never adjusted to make a run go green.
- **Counts are exact, timing is not.** The file count, the parse-error count, the parse-warning
  count and *no other code at all* are properties of the pinned commit and of ya-lsp, identical on
  a laptop and a shared runner, so there is no reason for slack. Time is the only quantity a runner
  can change, and the only one with a ceiling rather than an equality.
- **The exact-code assertion catches a decision, not a defect.** `parse-error` and `parse-warning`
  are the only rules shipping on; the other eight are rubydex saying *it* gave up on legal Ruby.
  Turning `dynamic-ancestor` on in this workspace produces hundreds of warnings across working
  code, which is why the default is `Off`. If a code the canary has never seen starts appearing,
  re-decide the default, not the number in the `Makefile`.
- **The file count is read out of a log line, because no request answers it.** `workspace/symbol`
  answers with symbols and a cap; nothing on the wire says how many files are indexed. The driver
  reads `analysis::Analysis::index_workspace`'s `indexed N files in T` at INFO, which also carries
  the cold index time. That makes a format string a contract, so the driver pins its shape and
  fails with `LOG SHAPE` naming the function — a reword produces a legible failure instead of a
  canary that quietly measures nothing.
- **Diagnostics are state, not events.** The server republishes a URI whenever its set changes and
  sends an empty list to clear one, so counting notifications double-counts every file touched
  twice. The last publish per URI wins; totals come from that map at the end.
- **The gem half is not covered, and the job says so.** Resolving the canary's bundle needs
  `bundle install`, which needs a matching Ruby and a hand-built native extension — a lot of CI for
  a project whose headline is that it needs no Ruby. The run leaves gem settings at their defaults,
  finding some gems on a developer machine and none on CI, and *nothing asserted depends on which*:
  `collect_diagnostics` filters to own code before grouping, and the index line is written before
  gem indexing starts. Gem numbers stay manual — see `benchmarking.md`.
- **It is cloned, never vendored, and that is what makes the licence free.** The canary repository
  is BSD-3-Clause, whose condition is notice retention on redistribution. No artifact ya-lsp ships
  contains any of it, so no notice is owed (`licensing.md`: per artifact, not per repository). That
  is a property of *how it is used*: copy one file into `tests/` or cache a tarball here and the
  obligation attaches, in `THIRD-PARTY-NOTICES.txt`.
- **The SHA is pinned for the same reason the tool versions are.** An unpinned target makes every
  asserted number meaningless across runs, and flakes the first time upstream commits.
- **Ask git about `$(CANARY_DIR)/.git`, never `git -C $(CANARY_DIR)`.** The workspace lives under
  `tmp/`, inside this repository, and **git searches upwards**: `git -C tmp/x rev-parse --git-dir`
  in an empty `tmp/x` succeeds and answers about *ya-lsp*. The obvious spelling of "is this a
  repository yet?" therefore skips the `init`, adds a remote to ya-lsp, fetches the canary into
  ya-lsp's object store, and runs `checkout --detach` on the working tree being developed in. It
  was written that way once; what stopped it was an unrelated dirty tree, not the check.
  `[ -e "$dir/.git" ]` cannot walk up, and the toplevel comparison refuses the root itself.
- **`make canary` is deliberately not in `make ci`.** Everything in `ci` is hermetic; a target that
  fails on a plane teaches people to skip it. CI runs the canary as its own job, where the name in
  the checks list says what it covers.
