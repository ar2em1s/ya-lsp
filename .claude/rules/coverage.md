---
paths:
  - "Makefile"
  - "scripts/**"
  - ".github/workflows/**"
---

# Coverage

## The three bars

All in the `Makefile`, all enforced by `scripts/coverage.sh`.

| Bar | Value | Job |
| --- | --- | --- |
| `MIN_LINES` / `MIN_BRANCHES` | 95 | project-wide; catches aggregate regression |
| `MIN_FILE_LINES` | 90 | every file on its own, so one bad file cannot hide in a good average |
| `COVERAGE_FLOORS` | 100 | 31 named modules that must be complete |

Regions and functions print off the same profile and are not gated. The project sits comfortably
above both project bars; `make coverage` prints where.

The number goes **up** across a release that adds features, for two reasons worth knowing: a new
file arriving at 100 raises the aggregate without closing any gap, and a refactor splitting one
file into six turns aggregate gaps into named ones that then get closed. One gap was closed by
**dropping a check rather than writing a test**: a `def`'s rest parameter cannot be Prism's
`ImplicitRestNode`, so asking which kind it was put an arm in the file no Ruby reaches.

- **The per-file bar is deliberately below the project bar, because the denominators are small.**
  the smallest files are a few dozen lines, so one uncovered line is several points and the
  smallest of them sit one line above a bar of 95. A high uniform bar measures file size more than
  testing. **There is no uniform per-file *branch* bar at all**, for the same reason: the smallest
  files carry barely a dozen branches in total, so one untaken arm is worth most of a bar. Where a
  file has to be complete, name it in `COVERAGE_FLOORS`. Raise `MIN_FILE_LINES` when the weakest
  file has real margin above it; it is a ratchet like the others.

## What goes on the 100 list

Two questions, both must answer yes.

1. **Is being wrong here silent and wide?** A visibly broken hover is found in a day; a mis-parsed
   `Gemfile.lock` is not.
2. **Is 100 structurally reachable?** A file whose gap is `usize::try_from` on a 64-bit build can
   never hold the bar however important it is, and listing it would only teach people to edit the
   list.

The entries and why:

- `analysis/position.rs` — offsets and incremental edits; silently corrupts the user's file.
- `workspace/uri.rs` — the workspace root and the one spelling of a document key.
- `workspace/config.rs` — every default.
- `server/capabilities.rs` — the wire contract.
- `analysis/diagnostics.rs` — the rule → severity table, and the rule names the user's config keys
  match on.
- `licenses.rs` — wrong here is a licence nobody granted.
- `workspace/ruby_version.rs` — which Ruby, therefore which stdlib.
- `workspace/bundler.rs` — pure text with no I/O, so there is nothing it cannot be asked.
- `analysis/render.rs` — the one place a construct is spelled for a human, shared by hover, the
  outline and the picker, including RDoc markup → markdown, where a swallowed `<vowel>` looks like a
  sentence with a word missing rather than like a bug.
- `analysis/references.rs` — a truncated answer looks exactly like a complete one.
- `analysis/ranges.rs` — advertising a folding provider takes the editor's indentation guess *out of
  play*, so an unrecognised construct is a chevron that never appears. Lines only:
  `make coverage-branches F=ranges` finds no untaken arm while the summary reads short, from two
  merged regions counted apart.
- `messages.rs` — every sentence a user reads. Lines only; see below.
- `analysis/rename.rs` — the first module that *writes*, where a rule that stops firing edits the
  user's files. Ruby 3.1's `{ x:, y: }` renames the hash key along with the value and still parses,
  the widest and quietest failure ya-lsp can have.
- `analysis/scopes.rs` — which variable is which. Listed for rename rather than the highlighting it
  was written for: a scope bug lighting up the wrong occurrences is seen at once; the same bug
  behind a rename writes over the wrong variable, and `code_actions` is a second caller.
- `analysis/code_actions.rs` — the second module that writes, listed for `rename.rs`'s reason: every
  guard in it is a *refusal*, so an untested one is an action still offered where it should not be.
- `analysis/erb.rs` — the Ruby view of a template, where a moved byte puts every answer below on the
  wrong column, silently, and only for people who do not write their markup in English.
- `analysis/structs.rs` — `Struct.new` and `Data.define`.
- Every file of `workspace/rails/` — the *whole* of what ya-lsp knows about Rails, where a
  convention reaching the wrong class answers confidently and wrongly about the file the user is
  looking at. Each file is listed rather than the directory, because a floor whose path is stale
  passes forever while measuring nothing.
- `analysis/synthesized.rs` — all that stands between a generated declaration and a jump into a file
  that does not exist: a mapping pointing at the wrong span opens the wrong line confidently, and
  one answering where it should have withheld opens nothing.
- `generated.rs` — the fact table every generator ends at, where a precedence rule picking the wrong
  one of two colliding declarations is a type nothing reports, and `render`'s spans are what a jump
  lands on.

## Deliberate exclusions

- **`analysis/annotations.rs`** answers question 1 yes and is still off the list. Pure text like
  `rails/`, silent when wrong — but its one residual line is a match arm over Prism's
  keyword-parameter list, which holds exactly two node kinds and cannot hold a third. It is one
  line short of complete and full on branches, and the `Makefile` says so beside the other
  refusals.
- **`analysis/types.rs`** fails question 2. Being wrong in it is silent and wide (a wrong return type
  is a confidently wrong completion list), but its residual arms are `_ =>` cases over rubydex and
  rbs node kinds the parsers cannot produce — a namespace path segment that is not a symbol, an
  overload list entry that is not an overload — the same shape as `signatures`' `try_from` guards.
  **What carries it instead is the policy fixture**: the whole of `class_of` drawn in one document,
  plus a first-N over three real files of `vendor/rbs/core` whose count is pinned — which is also
  what makes an rbs bump the visible behaviour change `rbs-signatures.md` says it is.
- **`analysis/signatures.rs` and `analysis/symbols.rs` have the highest blast radius in the crate
  and are deliberately not listed.** A `signatures` bug indexed a handful of `Array`'s methods
  instead of all of them through a green suite, and VS Code *throws* on a bad `selectionRange`,
  discarding a whole outline. Neither
  can reach 100: four of `signatures`' fourteen branches are `try_from` guards that cannot fail on a
  64-bit build, and `symbols`' residual gap is a 64-deep nesting walk no Ruby file produces. **Where
  a floor cannot go, the fixture carries the weight** — that is what `ANCESTRY` and the first-ten
  lists are for.
- **Being at 100 today is not a reason to be listed.** `analysis/progress.rs` is at 100, and a
  stream left open is a visible spinner rather than a silent wrong answer. `analysis/hover.rs` stays
  off for the same kind of reason — this list's own worked example of a *no* is "a visibly broken
  hover is found in a day" — and cannot reach 100 anyway, its residual line being
  `Namespace::Todo`, rubydex's placeholder. What carries hover is the four cards pinned whole in
  `analysis::tests`.
- **`messages.rs` is lines only, and that is the right gate there.** The file has **zero branch
  regions** — its one `match` is on a tuple and compiles to no branch llvm-cov counts.
  `messages::tests` already checks the *set* of messages in both directions, so a new `pub fn`
  cannot ship unenumerated; what it cannot see is a new arm **inside** an existing message, and an
  unexercised arm is an uncovered line. `workspace/config.rs` is the worked example: complete on
  lines and branches, with every one of its messages provably executing under test, and not one of
  those tests reading a word of any of them.

## Reading the report

- **Read `make coverage-missing` too, after the gate is green.** Branches are where the guards hide;
  *lines* are where whole answers hide. With both bars passing, `hover::signature` still had four
  arms nothing had asked for — a module, `class << self`, the visibility prefix on a private method,
  a constant — each rendering a card a user reads. What is left there now is dominated by
  continuation lines of multi-line `tracing::` calls, so a file whose uncovered lines are *not*
  logging is a file with an answer nobody has checked.
- **`--show-missing-lines` under-reports**, which is the second reason the per-file floors exist. It
  lists uncovered lines inside *instantiated* functions, so a closure no test ever constructed is
  invisible: `workspace/uri.rs` sat below the per-file bar with three uncovered lines and did not
  appear in `make coverage-missing` at all. Those three were the deprecated `rootPath` rung of
  `workspace_root` and both arms of its working-directory fallback — the code deciding which tree
  gets indexed. The per-file percentage in `make coverage` is the honest number.
- **A report read over a `target/llvm-cov-target` holding more than one version of the source is
  fiction**, and it fails in the direction that wastes a day. `cargo llvm-cov report` reads every
  test binary left there, so a build from before an edit still contributes its coverage mapping — at
  *its* line numbers. The symptom is a branch list where every entry appears twice at two different
  lines, and a total quietly short on branches. It has cost a wrong conclusion at least once: a
  change that read as a branch *regression* measured, on a clean tree, as an improvement. Run `make coverage-clean` whenever a number moves in a direction the diff
  does not explain — the toolchain stamp in `coverage-run` wipes on a *toolchain* change and nothing
  wipes on a source one, because always wiping would make every coverage run a full rebuild.
- **`--ignore-filename-regex` is not needed and should not be added.** llvm-cov reports only files
  compiled into the crate, so a reference clone under `tmp/` never enters the denominator. (This is
  where tarpaulin differed: it walks every `.rs` file under the project root, so a reference
  checkout in `tmp/` roughly halved the number it reported.)

## What the number cannot see

None of these is a coverage gap; do not treat a green gate as covering them.

1. **The run loop is covered for order, not for time.** `analysis/threaded_tests.rs` drives the real
   thread over the real channel — which is what took the last two arms in `analysis/mod.rs` — and
   every assertion in it is a position in the message stream rather than a duration.
2. **Ranking and composition are pinned by fixtures, not by the percentage.** `ANCESTRY`, `SIGILS`,
   `PICKER`, `GALLERY` and the signature-help, highlight and folding lists are whole-answer
   assertions, every one written over lines already covered, two of them finding a defect as they
   were written.
3. **No test in this denominator opens a real Rails app.** `make canary` does, on a pinned public
   one, as a separate target and a separate CI job precisely because it is not part of this number.
   (`scripts/canary.py` is Python and never enters the report; `canary.md` is its rule,
   `concurrency.md` the harness's.)
4. **Nothing here says an answer is *right*.** The return-type policy was fully covered by the tests
   that found `self` being resolved to the declaring class instead of the receiver, because both
   readings execute the same line.

Also: **a `tracing::` line is covered only when the run enables its level, and raising the level to
cover them is gaming the number.** About thirty uncovered lines are continuation lines of multi-line
`info!`/`debug!` calls; `YA_LSP_LOG=trace` would execute every one and assert nothing about any.
**In a file held at 100, compute the arguments into locals first** and leave the macro one line with
inline captures — the value is then evaluated whether or not the level is on, which is where it
belongs anyway. Not a reason to reword the other thirty.

## Process-wide state

Lift it to the boundary rather than leaving it untestable. `gems::Env`'s `GEM_PATH` splitting is
`split_path_list(Option<OsString>)`, and `workspace_root`'s last rung is
`root_from(params, Option<PathBuf>)` — each with the syscall at the edge and the decision in a pure
function. Both were reached for the same reason: a test that sets an environment variable or deletes
the working directory changes it for every other test in the binary.
