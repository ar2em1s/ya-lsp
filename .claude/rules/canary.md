---
paths:
  - "scripts/canary.py"
  - "Makefile"
  - ".github/workflows/**"
---

# The canary

`make canary` opens a pinned real Rails app (lobsters) and asserts exact counts. It answers one
question: does opening a real app still work?

## Must

1. **Keep the asserted counts only in the `Makefile`** (`CANARY_FILES`, `CANARY_WARNINGS`). Never
   restate them in prose, because a second copy goes stale.
2. **Change a count only as a decision you argue for**, never to make a run go green. If a new
   diagnostic code shows up, reconsider the rule's default instead of the number.
3. **Keep the SHA only in `scripts/corpora.toml`.** `make canary-clone` delegates to
   `scripts/corpora.py clone --only lobsters`.
4. **Check for `$dir/.git` with `[ -e "$dir/.git" ]`, never `git -C $dir`.** The corpora live under
   `tmp/`, inside this repo, and git searches upwards. On an empty directory, `git -C` answers
   about ya-lsp, and the clone step then fetches into ya-lsp and checks out over your working tree.
   `scripts/corpora.py` enforces this.
5. **Nothing on the canary's path may need Ruby.** CI installs no Ruby for this job.
   `scripts/corpora.py` checks for `asdf` per command, using the `needs_ruby` column of `STEPS`,
   and `clone` needs none. Never move that check to the top of `main`.

## How it measures

- **Counts are exact; only time has a ceiling.** The file count, parse errors, parse warnings and
  the absence of any other code depend only on the pin and on ya-lsp. The time ceiling is a large
  multiple of the measured value, so a slow shared runner cannot flake it. A flaky canary teaches
  people to ignore it.
- **The file count comes from a log line**, because no request reports it: the INFO line
  `indexed N files in T` from `Analysis::index_workspace`. That format string is a contract. If it
  changes, the driver fails with `LOG SHAPE`.
- **Diagnostics are state, not events.** The last publish per URI wins, and an empty list clears
  it. Counting notifications would double-count.
- **Gems are not covered.** CI does not `bundle install`, and nothing asserted depends on gems:
  `collect_diagnostics` keeps only the project's own files, and the index line is written before
  gems are indexed.

## Settled

- **Only `parse-error` and `parse-warning` ship on.** Turning `dynamic-ancestor` on here produces
  hundreds of warnings on working code.
- **Clone lobsters; never vendor it.** It is BSD-3-Clause. Copying one file into this repo would
  create a notice obligation (`licensing.md`).
- **`make canary` is not in `make ci`.** `ci` must be hermetic. The canary runs as its own CI job.
