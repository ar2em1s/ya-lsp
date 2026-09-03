---
paths:
  - "Makefile"
  - "scripts/**"
  - ".github/workflows/**"
---

# Coverage

- **Three bars with three different jobs, all in the `Makefile`, all enforced by
  `scripts/coverage.sh`.** `MIN_LINES`/`MIN_BRANCHES` at **95** is the project bar and catches
  aggregate regression. `MIN_FILE_LINES` at **90** is a floor every file clears on its own, so one
  bad file cannot hide inside a good average — the project sits at 97% over 4,300 lines, and a new
  200-line module landing at 80% would move that by less than a point. `COVERAGE_FLOORS` names the
  modules held to **100**. Regions and functions print off the same profile and are not gated.
  Currently 98.46% lines and 96.91% branches.
- **The per-file bar is deliberately below the project bar, because the denominators are small.**
  `main.rs` is 36 lines, so one uncovered line is five points; `analysis/signatures.rs` is 40 lines
  and sits at exactly 95.00%, one line from failing a bar of 95. A high uniform bar measures file
  size more than it measures testing. **There is no uniform per-file *branch* bar at all, for the
  same reason** — the smallest files carry ten to fourteen branches in total, which makes one
  untaken arm worth seven to ten points. Where a file has to be complete, say so by name in
  `COVERAGE_FLOORS`. Raise `MIN_FILE_LINES` when the weakest file has real margin above it; it is a
  ratchet like the others.
- **Fourteen modules are held to 100% on top of that, and the list is `COVERAGE_FLOORS` in the
  `Makefile`.** Two questions decide membership and both have to answer yes. **Is being wrong here
  silent and wide?** — a visibly broken hover is found in a day; a mis-parsed `Gemfile.lock` is
  not. **Is 100 structurally reachable?** — a file whose gap is `usize::try_from` on a 64-bit build
  can never hold the bar however important it is, and listing it would only teach people to edit
  the list. The entries: `analysis/position.rs` (offsets and incremental edits — silently corrupts
  the user's file), `workspace/uri.rs` (the workspace root and the one spelling of a document key),
  `workspace/config.rs` (every default), `server/capabilities.rs` (the wire contract),
  `analysis/diagnostics.rs` (the rule → severity table, and the rule names the user's config keys
  match on), `licenses.rs`, `workspace/ruby_version.rs` (which Ruby, therefore which stdlib),
  `workspace/bundler.rs` (pure text with no I/O, so there is nothing it cannot be asked),
  `analysis/render.rs` (the one place a construct is spelled for a human, shared by hover, the
  outline and the picker — and, since v0.2.0, RDoc's markup → markdown, where a swallowed
  `<vowel>` does not look broken but looks like a sentence with a word missing),
  `analysis/references.rs` (a truncated answer looks exactly like a complete one),
  `analysis/ranges.rs` (advertising a folding provider takes the editor's indentation guess *out
  of play*, so a construct the walk does not recognise is not a visible bug but a chevron that
  never appears — lines only, because `make coverage-branches F=ranges` finds no untaken arm while
  the summary reads 98% from two merged regions counted apart),
  `messages.rs` (every sentence a user reads), `analysis/rename.rs` (the only module in the crate
  that *writes*, where a rule that stops firing edits the user's files — Ruby 3.1's
  `{ x:, y: }` renames the hash key along with the value and still parses, which is the widest and
  quietest failure ya-lsp can have), and `analysis/scopes.rs` (which variable is which, listed for
  rename rather than for the highlighting it was written for: a scope bug that lights up the wrong
  occurrences is seen the first time anybody looks, and the same bug behind a rename writes over
  the wrong one — the second question was already yes, and v0.3.0's item 5 is what turned the
  first).
- **`analysis/signatures.rs` and `analysis/symbols.rs` have the highest blast radius in the crate
  and are deliberately *not* on the list.** A `signatures` bug indexed 7 of `Array`'s 197 methods
  through a green suite, and VS Code *throws* on a bad `selectionRange`, discarding a whole
  outline. Neither can reach 100: four of `signatures`' fourteen branches are `try_from` guards
  that cannot fail on a 64-bit build, and `symbols`' residual gap is a 64-deep nesting walk no Ruby
  file produces. **Where a floor cannot go, the fixture has to carry the weight instead** — that is
  what `ANCESTRY` and the first-ten lists are for. Being at 100 today is likewise not a reason to
  be listed: `analysis/progress.rs` is, and a stream left open is a visible spinner rather than a
  silent wrong answer. `analysis/hover.rs` stays off for the same kind of reason and is worth
  saying out loud, since v0.2.0's item 6 landed on it: this list's own worked example of a *no* is
  "a visibly broken hover is found in a day", and the file cannot reach 100 anyway — its residual
  line is `Namespace::Todo`, rubydex's placeholder. What carries hover instead is the four cards
  pinned whole in `analysis::tests`, which is the fixture rule doing its job.

- **`messages.rs` earned a floor for a reason worth keeping straight: *lines* are the whole gate
  there.** The file has **zero branch regions** — its one `match` is on a tuple and compiles to no
  branch llvm-cov counts — so `messages.rs=100` names lines only. That is not a weaker gate here,
  it is the right one. `messages::tests` already checks the *set* of messages in both directions,
  so a new `pub fn` cannot ship unenumerated; what it cannot see is a new arm **inside** an
  existing message, and an unexercised arm is an uncovered line. Which is precisely item 6's
  finding turned into a gate: `workspace/config.rs` sat at 100% of lines and branches with all
  seven of its messages provably executing under test and not one test reading a word of any of
  them. The other half of the pair — is 100 structurally reachable? — is the same answer
  `workspace/bundler.rs` gives: pure text, no I/O, no platform in it, so there is nothing it
  cannot be asked.
- **Read `make coverage-missing` too, and read it after the gate is green.** Branches are where the
  guards hide; *lines* are where whole answers hide. With both bars passing, `hover::signature`
  still had four arms nothing had ever asked for — a module, `class << self`, the visibility prefix
  on a private method, a constant — each of which renders a card a user reads. What is left there
  now is dominated by continuation lines of multi-line `tracing::` calls, so a file whose uncovered
  lines are *not* logging is a file with an answer nobody has checked.
- **`--show-missing-lines` under-reports, which is the second reason the per-file floors exist.**
  It lists uncovered lines inside *instantiated* functions, so a closure no test ever constructed
  is invisible to it: `workspace/uri.rs` sat at 94.34% with three uncovered lines and did not
  appear in `make coverage-missing` at all. Those three were the deprecated `rootPath` rung of
  `workspace_root` and both arms of its working-directory fallback — the code that decides which
  tree gets indexed. The per-file percentage in `make coverage` is the honest number; the missing
  lines list is a convenience that can be silently short.
- **Process-wide state gets lifted to the boundary rather than left untestable.** `gems::Env`'s
  `GEM_PATH` splitting is `split_path_list(Option<OsString>)` and `workspace_root`'s last rung is
  `root_from(params, Option<PathBuf>)`, each with the syscall at the edge and the decision in a
  pure function. Both were reached for the same reason: a test that sets an environment variable
  or deletes the working directory changes it for every other test in the binary.
- **Three things the number cannot see, and none of them is a coverage gap:**
  **the run loop is covered for order and not for time** — `analysis/threaded_tests.rs` drives the
  real thread over the real channel, which is what took the last two arms in `analysis/mod.rs`, and
  every assertion in it is a position in the message stream rather than a duration, so nothing
  anywhere in this denominator says an answer arrived *quickly*; **ranking and composition are
  pinned by fixtures and not by the percentage** — `ANCESTRY`, `SIGILS`, `PICKER`, `GALLERY` and
  the signature-help, highlight and folding lists are whole-answer assertions, and every one of
  them was written over lines that were already covered, two of them finding a defect as they
  were written; and **no test in this denominator opens a real Rails app** — `make
  canary` does, on a pinned public one, and it is a separate target and a separate CI job precisely
  because it is not part of this number. Do not treat a green gate as covering any of the three.
  (The canary's own file, `scripts/canary.py`, is Python and never enters the report; `canary.md`
  is its rule, and `concurrency.md` is the harness's.)
- **A `tracing::` line is covered only when the run enables its level, and raising the level to
  cover them is gaming the number.** About thirty of the uncovered lines are continuation lines of
  multi-line `info!`/`debug!` calls. `YA_LSP_LOG=trace` would execute every one of them and assert
  nothing about any of them. **In a file being held at 100 the way around it is to compute the
  arguments into locals first** and leave the macro one line with inline captures — the value is
  then evaluated whether or not the level is on, which is where it belongs anyway if it is worth
  logging. Not a reason to reword the other thirty.
- **A report read over a `target/llvm-cov-target` that has built more than one version of the
  source is fiction, and it fails in the direction that wastes a day.** `cargo llvm-cov report`
  reads every test binary left in that directory, so a build from before an edit still
  contributes its coverage mapping — at *its* line numbers. The symptom is a branch list where
  every entry appears twice at two different lines, and a total that has quietly lost half a
  point of branches to arms nothing can take because the code they belong to no longer exists.
  It cost a wrong conclusion here: item 0 read as a 0.5-point branch regression and was
  measured, on a clean tree, as a 0.3-point improvement. `make coverage-clean` first whenever a
  number moves in a direction the diff does not explain — the toolchain stamp in `coverage-run`
  wipes on a *toolchain* change and nothing wipes on a source one, because always wiping would
  make every coverage run a full rebuild.
- **`--ignore-filename-regex` is not needed and should not be added.** llvm-cov reports only files
  compiled into the crate, so a reference clone under `tmp/` never enters the denominator. (This is
  where tarpaulin differed: it walks every `.rs` file under the project root, and a rubydex checkout
  in `tmp/` made it read 43.32% where the crate read 90.50%.)
