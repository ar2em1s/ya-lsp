---
paths:
  - "Makefile"
  - "scripts/**"
  - ".github/workflows/**"
---

# Coverage

## Before you trust a number

1. **Run `make coverage-clean`** whenever a number moves in a direction the diff doesn't explain.
   `cargo llvm-cov report` reads every test binary left in `target/llvm-cov-target`, including ones
   built before your edit, at *their* line numbers. The symptom is every branch listed twice, at two
   different lines. Only a toolchain change wipes it automatically.
2. **Trust the per-file percentage in `make coverage` over `make coverage-missing`.**
   `--show-missing-lines` never lists a closure that no test constructed.
3. **Read `make coverage-missing` once the gate is green.** Branches hide guards; missing lines hide
   whole answers. Uncovered lines that are not `tracing::` continuations are answers nobody has
   checked.

## The three bars

All three live in the `Makefile` and are enforced by `scripts/coverage.sh`.

| Bar | Value | Job |
| --- | --- | --- |
| `MIN_LINES` / `MIN_BRANCHES` | 95 | Project-wide |
| `MIN_FILE_LINES` | 90 | Every file on its own. Lower than 95 because small files swing several points per line |
| `COVERAGE_FLOORS` | 100 | 39 named modules |

- **There is no per-file branch bar.** Small files have about a dozen branches. Put a file that must
  be complete in `COVERAGE_FLOORS`.
- **Raise `MIN_FILE_LINES` when the weakest file has real margin.** Every bar only ratchets up.

## Must

1. **Every `mod tests` carries `#[cfg_attr(coverage_nightly, coverage(off))]` above `#[cfg(test)]`.**
   Otherwise the tests count as covered code.
2. **Never raise the log level to cover `tracing::` lines.** In a floored file, compute the arguments
   into locals first and keep the macro on one line.
3. **Lift process-wide state to the edge.** Put the syscall at the boundary and the decision in a
   pure function (`gems::split_path_list`, `uri::root_from`). A test that sets an env var or deletes
   the cwd changes it for every test.
4. **List each floored file by path, never by directory.** A stale path passes forever.
5. **Never add `--ignore-filename-regex`.** llvm-cov reports only files compiled into the crate.

## What goes on the 100 list

**Both answers must be yes:**

1. Is being wrong here **silent and wide**?
2. Is 100% **structurally reachable**?

The listed files and why:

- **Writes or corrupts:** `position.rs`, `rename.rs`, `code_actions.rs`, `scopes.rs` (the last is
  there for rename, not highlighting).
- **Contracts:** `server/capabilities.rs`, `diagnostics.rs` (rule names match config keys),
  `workspace/uri.rs`, `workspace/config.rs`, `licenses.rs`.
- **Silent wrong answers:** `ruby_version.rs`, `bundler.rs`, `render.rs`, `references.rs`,
  `ranges.rs` (lines only), `erb.rs`, `structs.rs`, `synthesized.rs`, `generated.rs`, and every file
  of `workspace/rails/`.
- **`messages.rs`, lines only.** It has zero branch regions. `messages::tests` checks the set of
  messages.
- **Also floored:** `features.rs`, `environment.rs` and `logging.rs` (`features.md`, `logging.md`).

## Deliberately not listed

| File | Why not |
|---|---|
| `annotations.rs` | One unreachable arm over a two-kind Prism list |
| `types.rs` | `_ =>` arms over node kinds the parsers can't produce. The policy fixture carries it |
| `signatures.rs`, `symbols.rs` | Highest blast radius, but `try_from` guards and a 64-deep nesting walk can't be reached. `ANCESTRY` and the first-ten lists carry them |
| `progress.rs`, `hover.rs` | Wrong here is visible. Being at 100 today is not a reason to be listed |

## What a green gate cannot see

1. **Timing.** `threaded_tests.rs` asserts order, not duration.
2. **Ranking and composition.** Whole-answer fixtures carry these: `ANCESTRY`, `SIGILS`, `PICKER`,
   `GALLERY`, and the signature-help, highlight and folding lists.
3. **A real Rails app.** `make canary` covers that (`canary.md`).
4. **Whether an answer is right.** A wrong reading can execute the same line as the right one.
