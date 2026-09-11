# ya-lsp — the commands, in one place.
#
# `make` on its own lists them. Everything here is what CI runs, spelled the same way, so a
# green `make ci` locally means a green CI run.
#
# Two tool versions are pinned below. They are cargo *subcommands*, which cargo cannot express
# as dependencies — it never builds a dependency's binaries — so `make setup` installs them and
# this file is the single place their versions are written down.

CARGO         ?= cargo
NIGHTLY       ?= +nightly
ARGS          ?=
EXTENSION_DIR := editors/vscode

# The coverage bars, both enforced by `scripts/coverage.sh` — cargo-llvm-cov can fail a build on
# lines but has no `--fail-under-branches`, which is the whole reason that script exists.
#
# One bar for both, and branches is the one that took the work: it started at 79.08%. What is
# left is ~28 conditions that no input reaches — `let ... else` on a rubydex lookup a consistent
# graph cannot fail, a non-UTF-8 filename APFS will not create, a panicked analysis thread. Raise
# these as real arms get covered; never meet one by deleting a guard, which is the same edit as
# widening `#[coverage(off)]`. `make coverage-branches` names every arm still untaken.
MIN_LINES     ?= 95
MIN_BRANCHES  ?= 95

# And a floor every file clears on its own, so one bad file cannot hide inside a good average:
# the project sits at 97%, and a new 200-line module landing at 80% would move that by less than
# a point. It is deliberately *below* the project bar rather than equal to it. Two reasons, both
# from the numbers. `workspace/rbs.rs` is the binding file at ~94%, and what is left there is
# continuation lines of multi-line `tracing::info!` calls, which only run when the level is
# enabled — covering them means raising the log level, which asserts nothing. And a 36-line file
# like `main.rs` moves five points per uncovered line, so a high uniform bar measures file size
# more than it measures testing. Named modules that must be complete go in COVERAGE_FLOORS below.
MIN_FILE_LINES ?= 90

# Modules held to 100%, above the project-wide bar. `cargo-llvm-cov`'s own
# `--fail-under-file-lines` cannot express this — it applies one number to every file, which in
# practice is whatever the weakest file can pass — so `scripts/coverage.sh` gates them. Each entry
# is `path=lines[:branches]`; a path missing from the report is an error, not a pass.
#
# Two questions decide membership and both have to answer yes. **Is being wrong here silent and
# wide?** — a visibly broken hover is found in a day, a mis-parsed lockfile is not. **Is 100
# structurally reachable?** — a file whose gap is `usize::try_from` on a 64-bit build can never
# hold the bar however important it is, and listing it would only teach people to edit this list.
#
#   position.rs      LSP position <-> byte offset and incremental edits. A wrong answer here
#                    silently corrupts the user's file, and no other test would notice.
#   uri.rs           the workspace root, and the one spelling of a document key. Get it wrong
#                    and `didOpen` forks a second document, or the whole wrong tree is indexed.
#   config.rs        decides every default, so it decides every other answer.
#   capabilities.rs  the wire contract, fixed at `initialize` and never renegotiated.
#   diagnostics.rs   the rule -> severity table; a transposed row squiggles working code, and a
#                    renamed rule silently stops matching the user's ya-lsp.toml keys.
#   licenses.rs      what `--licenses` prints. Wrong here is a licence nobody granted.
#   features.rs      which bodies of knowledge apply to a project. On this list for `config.rs`'s
#                    reason: it decides what every later answer is made of, and being wrong is
#                    silent in both directions — a family switched off answers nothing with no
#                    error anywhere, and `auto` guessing wrong brings Rails' conventions to a
#                    project that is not Rails and cites a controller that does not exist. Pure
#                    decisions over a path and a lockfile, like bundler.rs.
#   logging.rs       where the log goes. On this list for `config.rs`'s reason rather than a new
#                    one: it decides whether anything is written at all, and being wrong there is
#                    silent by construction — a filter that came out one level too quiet, or a
#                    file sink that never opened, looks exactly like a server with nothing to
#                    say. It is also the module a bug report is assembled from, so a defect in
#                    it is a defect in every later diagnosis. The one line that cannot be tested
#                    — installing the global subscriber — is deliberately in `main.rs` instead.
#   ruby_version.rs  which Ruby, and therefore which stdlib. Pure text; the failure it exists to
#                    prevent is macOS's vestigial 2.6 answering for a 4.0 project.
#   bundler.rs       Gemfile.lock -> sources and specs. Pure text, no I/O, so there is nothing it
#                    cannot be asked; mis-parse a line and those gems are silently not indexed.
#   code_actions.rs  the second module that writes, and it is on this list for `rename.rs`'s
#                    reason rather than for a new one: the failure is not a bad answer but a
#                    broken buffer. Every guard in it is a *refusal*, so an untested one is an
#                    action still being offered where it should not be — which is invisible
#                    until somebody applies it.
#   render.rs        the one place a construct is spelled for a human, shared by hover, the
#                    outline and the picker. `is_nameable` shipped a page of anonymous classes.
#                    It now also owns RDoc's markup -> markdown, which is the same shape of
#                    failure one layer down: a `<vowel>` a renderer eats does not look broken,
#                    it looks like a sentence with a word missing.
#   references.rs    a truncated find-all-references looks exactly like a complete one, so the
#                    cap and the synthetic-reference filter are both silent when wrong.
#   ranges.rs        folding and expand-selection. Advertising a folding provider takes the
#                    editor's indentation guess *out of play*, so a construct this module fails
#                    to recognise is not a visible bug — it is folding that quietly stops
#                    existing for that shape, everywhere, which is the first question answering
#                    yes. Lines only: `make coverage-branches F=ranges` finds no arm untaken,
#                    while the summary reports 98% because two merged regions are counted apart.
#   messages.rs      every sentence a user reads. Pure formatting with no I/O, so like
#                    bundler.rs there is nothing it cannot be asked. Lines are the whole gate
#                    here — the file has no branch regions at all — and lines are exactly what
#                    catches the failure it exists to prevent: an arm of a message that
#                    ships without one test having read it. The enumeration test guards the set
#                    of messages; only this guards the insides of one.
#   rename.rs        the only module in the crate that *writes*. Every other wrong answer shows
#                    the user something unhelpful; a rule here that stops firing edits their
#                    files — Ruby 3.1's `{ x:, y: }` renames the hash key along with the value
#                    and still parses, which is the widest and quietest failure ya-lsp can have.
#                    Pure decisions over a string and a resolution, so every arm is reachable.
#   scopes.rs        which variable is which, and listed for rename rather than for the
#                    highlighting it was written for: a scope bug that lights up the wrong
#                    occurrences is seen the first time anybody looks, and the same bug behind a
#                    rename writes over the wrong one. The second question was already yes; this
#                    release is what turned the first one.
#   erb.rs           the Ruby view of a template. Every offset in the file depends on it, and
#                    the way it goes wrong is that a byte moves: ruby-lsp's own scanner pads by
#                    character, which shortens the buffer once per accent and three times per
#                    emoji and puts every answer below on the wrong column — silently, and only
#                    for people who do not write their markup in English. Pure bytes in, bytes
#                    out, no I/O and no platform, so like bundler.rs there is nothing it cannot
#                    be asked; it landed at 100 of lines *and* branches on its first release.
#   synthesized.rs   where a declaration ya-lsp wrote itself was really declared. The whole of
#                    what stands between a generated declaration and a jump into a file that
#                    does not exist, and both ways it goes wrong are silent: a mapping that
#                    points at the wrong span opens the wrong line confidently, and one that
#                    answers where it should have withheld opens nothing at all. Every
#                    generated declaration in the release goes through one function here. Pure
#                    lookups over a map, no I/O and no platform, like bundler.rs — and
#                    `generated_uri` is deliberately total rather than fallible, so there is no
#                    unreachable arm to make 100 impossible. It landed at 100 of lines *and*
#                    branches on its first release.
#   generated.rs     the RBS this crate writes, and where each declaration in it came from.
#                    Thirty lines of builder, and `append` shifts every span in one generator's
#                    output by the length of another's — off by one there is a jump that opens
#                    the wrong line confidently, which is the same failure synthesized.rs is on
#                    this list for, one layer earlier. Pure string building, no I/O.
#   structs.rs       `Struct.new` and `Data.define`, and the same argument rails/ is here for
#                    with the Rails word taken out: it decides which constant a member hangs on
#                    and which name it is, both of them by reading a literal, and both failures
#                    are silent — a member on the wrong constant answers confidently about a
#                    class the user is not looking at, and a call it declines looks exactly like
#                    a project that writes no structs. Pure text and no I/O, like bundler.rs.
#   hints.rs         the one answer nobody asked for, which is what turns the first question
#                    yes: a label is painted into the margin of every line whether anybody
#                    wanted it or not and is read as fact, so a guess that reaches one is the
#                    widest and quietest thing this crate can be wrong about — and nobody files
#                    a bug saying "this type was inferred from six letters". The guard is one
#                    `retain` on the tier and one predicate on the shape, both of them
#                    *refusals*, which is `code_actions.rs`' argument: an untested refusal is a
#                    label still being drawn where it should not be. Pure decisions over a
#                    buffer and a graph, no I/O; it landed at 100 of lines and branches on its
#                    first release.
#   environment.rs   the test-tree tag and the one place the rule is written down, read by five
#                    surfaces of which three must never act on it. Both ways it goes wrong are
#                    silent: a rule that fires too widely deletes a row nobody knows to look
#                    for — its first spelling dropped every declaration with no definitions,
#                    which is the top of the object model — and one that stops firing restores
#                    a leak whose whole symptom is a list that is a little longer than it
#                    should be. No lane of the audit scores it either, so the suite is the only
#                    instrument there is. Pure decisions over a path and a set, no I/O, like
#                    bundler.rs; it landed at 100 of lines and branches on its first release.
#   rails/           the *whole* of what ya-lsp knows about Rails — which is the reason it is
#                    one directory and the reason every file of it is on this list: a
#                    convention that reaches the wrong class answers confidently and wrongly
#                    about the file the user is looking at, and a convention that reaches
#                    nothing looks exactly like a project that does not follow it. Pure text
#                    and no I/O, like bundler.rs, so there is nothing any of them cannot be
#                    asked. `rails/mod.rs` is deliberately *not* listed: it is the convention
#                    tables and the `pub use` list, so it has no executable line and
#                    `llvm-cov` emits no row for it — and a floor whose path is not in the
#                    report is an error here, on purpose.
#
# Deliberately *not* here, so the next reader does not re-litigate it:
#   signatures.rs    highest blast radius in the crate — a bug indexed 7 of Array's 197 methods
#                    through a green suite — but four of its fourteen branches are `try_from`
#                    guards that cannot fail on a 64-bit build. Not reachable, so not listed.
#   symbols.rs       VS Code *throws* on a bad selectionRange, discarding the whole outline; its
#                    residual gap is a 64-deep nesting walk no Ruby file produces.
#   progress.rs      already at 100, but a stream left open is a visible spinner, not a silent
#                    wrong answer. Being at 100 is not by itself a reason to be on this list.
#   analysis/synthesize.rs  the pass itself. Its residual is three lines and one arm, none
#                    reachable: `workspace_relative`'s `to_path()` failing, which a `DocUri`
#                    cannot do by construction, and the two arguments of the settle's
#                    `tracing::debug!`, which the macro evaluates only when that level is on.
#                    98.75 of lines and 98.00 of branches.
#   annotations.rs   pure text like rails/ and it answers the first question yes — but its
#                    residual line is a match arm over Prism's keyword-parameter list, which
#                    holds exactly two node kinds and cannot hold a third. Not reachable, so
#                    not listed; it sits at 99.7 of lines and 100 of branches.
COVERAGE_FLOORS ?= \
  analysis/position.rs=100:100 \
  analysis/ranges.rs=100 \
  analysis/diagnostics.rs=100 \
  analysis/references.rs=100:100 \
  analysis/render.rs=100:100 \
  messages.rs=100 \
  analysis/rename.rs=100:100 \
  analysis/code_actions.rs=100:100 \
  analysis/scopes.rs=100:100 \
  analysis/erb.rs=100:100 \
  analysis/environment.rs=100:100 \
  analysis/structs.rs=100:100 \
  analysis/hints.rs=100:100 \
  workspace/rails/conventions.rs=100:100 \
  workspace/rails/inflect.rs=100:100 \
  workspace/rails/schema.rs=100:100 \
  workspace/rails/structure.rs=100:100 \
  workspace/rails/attributes.rs=100:100 \
  workspace/rails/concerns.rs=100:100 \
  workspace/rails/associations.rs=100:100 \
  workspace/rails/models.rs=100:100 \
  workspace/rails/relations.rs=100:100 \
  workspace/rails/delegates.rs=100:100 \
  workspace/rails/enums.rs=100:100 \
  workspace/rails/entrypoints.rs=100:100 \
  workspace/rails/framework.rs=100:100 \
  workspace/rails/routes.rs=100:100 \
  workspace/rails/syntax.rs=100:100 \
  workspace/rails/tail.rs=100:100 \
  analysis/synthesized.rs=100:100 \
  generated.rs=100:100 \
  workspace/uri.rs=100 \
  workspace/config.rs=100:100 \
  workspace/bundler.rs=100:100 \
  workspace/ruby_version.rs=100:100 \
  server/capabilities.rs=100 \
  licenses.rs=100 \
  logging.rs=100:100 \
  workspace/features.rs=100:100

# cargo-llvm-cov builds into its own target directory. Mixing stable- and nightly-built objects
# in it merges nightly counters against a stable covmap and reports a plausible, entirely wrong
# number, so the toolchain that produced it is stamped and a change wipes it.
COV_TARGET    := target/llvm-cov-target
COV_STAMP     := $(COV_TARGET)/.ya-lsp-toolchain

CARGO_ABOUT_VERSION   := ^0.9
CARGO_LLVM_COV_VERSION := 0.9.0

# Recipes that share the profraw directory must not interleave.
.NOTPARALLEL:

.DEFAULT_GOAL := help

## help: list the targets
.PHONY: help
help:
	@printf 'ya-lsp\n\n'
	@grep -hE '^## ' $(MAKEFILE_LIST) \
	  | sed -e 's/^## //' \
	  | awk -F': *' '{ printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 }'

# ---------------------------------------------------------------------------- build and run

## build: debug build
.PHONY: build
build:
	$(CARGO) build

## release: optimized build
.PHONY: release
release:
	$(CARGO) build --release

## run: run the server (make run ARGS="--stdio")
.PHONY: run
run:
	$(CARGO) run -- $(ARGS)

# ---------------------------------------------------------------------------- check and test

## check: type-check only
.PHONY: check
check:
	$(CARGO) check --all-targets

## fmt: format
.PHONY: fmt
fmt:
	$(CARGO) fmt --all

## fmt-check: fail if anything is unformatted
.PHONY: fmt-check
fmt-check:
	$(CARGO) fmt --all -- --check

## lint: clippy over every target, warnings denied
.PHONY: lint
lint:
	$(CARGO) clippy --all-targets -- -D warnings

# Only the **broken** class is denied, and the other two rustdoc lints are allowed here on
# purpose. `private_intra_doc_links` fires 101 times and every one of them is correct: this crate
# is private modules almost end to end, and the only ways to silence one are to make an internal
# item `pub` or to downgrade a link that works in an editor into plain text — both worse than the
# warning. `redundant_explicit_links` is 13 more of pure style. What is left is the class that
# points at **nothing**, and that is a rename whose comment did not follow it: `List::Concerns`
# outlived the enum it named by three commits, and of the 17 found the day this target was added,
# 8 were names moved by the last three refactors and 2 had never resolved at all. A grep finds
# those only if somebody already suspects them; rustdoc finds them every run.
## docs-check: fail on a doc link that points at nothing
.PHONY: docs-check
docs-check:
	RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links \
	  -A rustdoc::private_intra_doc_links -A rustdoc::redundant_explicit_links" \
	  $(CARGO) doc --no-deps --quiet

## test: the Rust suite
.PHONY: test
test:
	$(CARGO) test

## test-one: one test, with stdout (make test-one T=name)
.PHONY: test-one
test-one:
	@test -n "$(T)" || { echo "usage: make test-one T=<test name>"; exit 2; }
	$(CARGO) test $(T) -- --exact --nocapture

# ---------------------------------------------------------------------------- coverage

## coverage: run the suite under instrumentation and check both bars
.PHONY: coverage
coverage: coverage-run coverage-check

# Runs the suite once and leaves the raw profiles behind, so every report below reads the same
# run instead of re-running the tests for each format.
.PHONY: coverage-run
coverage-run:
	@mkdir -p $(COV_TARGET)
	@want="$$(rustc $(NIGHTLY) -vV | sed -n 's/^host: //p')-$$(rustc $(NIGHTLY) -vV | sed -n 's/^release: //p')"; \
	 if [ "$$(cat $(COV_STAMP) 2>/dev/null)" != "$$want" ]; then \
	   echo "coverage: toolchain changed, wiping $(COV_TARGET)"; \
	   rm -rf $(COV_TARGET); mkdir -p $(COV_TARGET); \
	 fi; \
	 printf '%s' "$$want" > $(COV_STAMP)
	$(CARGO) $(NIGHTLY) llvm-cov --no-report --branch

## coverage-check: the gate — lines and branches, per file and in total
.PHONY: coverage-check
coverage-check:
	@$(CARGO) $(NIGHTLY) llvm-cov report --branch --summary-only \
	  | scripts/coverage.sh --min-lines $(MIN_LINES) --min-branches $(MIN_BRANCHES) \
	    --min-file-lines $(MIN_FILE_LINES) --floors "$(COVERAGE_FLOORS)"

## coverage-branches: name every branch arm the suite never took (make coverage-branches F=gems)
.PHONY: coverage-branches
coverage-branches:
	@$(CARGO) $(NIGHTLY) llvm-cov report --branch --text \
	  | scripts/coverage.sh --branches $(if $(F),--file $(F),)

## coverage-missing: name every line the suite never ran
.PHONY: coverage-missing
coverage-missing:
	$(CARGO) $(NIGHTLY) llvm-cov report --branch --show-missing-lines

## coverage-html: the browsable report
.PHONY: coverage-html
coverage-html:
	$(CARGO) $(NIGHTLY) llvm-cov report --branch --html --open

## coverage-clean: drop the profiles and the nightly objects
.PHONY: coverage-clean
coverage-clean:
	rm -rf $(COV_TARGET) target/llvm-cov

# ---------------------------------------------------------------------------- the corpora

# The six benchmark corpora, pinned. `scripts/corpora.toml` is the table — repository, commit,
# Ruby and licence — and `scripts/corpora.py` is what makes a machine match it. The reasoning
# lives in both of those files and in `.claude/rules/corpora.md`; what belongs here is only the
# entry point.
#
# **This target needs a network and a Ruby toolchain, so it is not in `make ci` and is not a CI
# job either.** The canary is one 12 MB clone; this is six applications and their bundles.
#
# **It never installs a Ruby.** Each corpus declares one and the script resolves it against what
# asdf has, printing the `asdf install ruby X` line when it cannot. `--allow-nearest` accepts
# another patch of the same MAJOR.MINOR, preferring one whose bundle already resolves, and says
# so in the manifest.
#
# `make corpora-status ARGS=--json` is the manifest a measurement carries beside its numbers:
# without it an absolute count taken today cannot be compared with one taken last week, because
# nothing recorded which commit or how much of the bundle was installed.

## corpora: clone, pin, bundle and configure all six benchmark corpora
.PHONY: corpora
corpora:
	python3 scripts/corpora.py setup $(ARGS)

## corpora-clone: fetch every corpus at its pinned commit, and nothing else
.PHONY: corpora-clone
corpora-clone:
	python3 scripts/corpora.py clone $(ARGS)

## corpora-gems: bundle install each corpus under its own Ruby
.PHONY: corpora-gems
corpora-gems:
	python3 scripts/corpora.py gems $(ARGS)

## corpora-lsps: install ruby-lsp, solargraph and solargraph-rails into each corpus' Ruby
.PHONY: corpora-lsps
corpora-lsps:
	python3 scripts/corpora.py lsps $(ARGS)

## corpora-solargraph: write .solargraph.yml and compose the bundle solargraph is measured from
.PHONY: corpora-solargraph
corpora-solargraph:
	python3 scripts/corpora.py solargraph $(ARGS)

## corpora-docs: cache solargraph's gem documentation and warm ruby-lsp's composed bundle
.PHONY: corpora-docs
corpora-docs:
	python3 scripts/corpora.py docs $(ARGS)

## corpora-status: what is on disk against what is pinned (ARGS=--json for the manifest)
.PHONY: corpora-status
corpora-status:
	@python3 scripts/corpora.py status $(ARGS)

# ---------------------------------------------------------------------------- the canary

# A real Rails application, opened the way an editor opens it. `scripts/canary.py` carries the
# reasoning; these are the numbers, and they live here for the same reason the coverage bars do
# — so a local run and the CI run cannot disagree about them.
#
# Nothing automated had ever opened a real application before this target existed. Every
# performance number in three plans came from a private repository, which makes them
# unreproducible by anyone else and unrunnable by CI, and the behaviours that only appear in a
# real app were pinned by fixtures imitating their shape.
#
# **It does not cover gems**, and the reason is a cost rather than an oversight: resolving
# lobsters' bundle needs `bundle install`, which needs Ruby 4.0.0 and a hand-built `sqlite3`.
# That is a large amount of CI for a project whose headline is that it needs no Ruby. The gem
# numbers stay manual, and a green canary does not cover them.
#
# lobsters is BSD-3-Clause, (c) 2012-2019 Joshua Stein. It is **cloned, never vendored**: no
# artifact this project ships contains any of it, so no notice is owed, which is
# `licensing.md`'s "the rule is per artifact, not per repository". That is a property of how it
# is used and not of the licence — copy one file out of it into `tests/`, or cache a tarball in
# this repository, and the obligation attaches.
#
# The SHA is pinned because an unpinned target turns a canary into a flake and makes every
# number it asserts meaningless across runs. The ceiling is an order of magnitude above the
# 20.99 ms measured on 2026-09-03: a shared runner with a cold page cache is not that machine,
# and what this catches — the accidental quadratic, the discovery rule that stops matching —
# moves the number by a factor rather than by a percent.
# The repository, the commit and the Ruby live in `scripts/corpora.toml`, which pins all six
# corpora and is where `make corpora` reads them from. They are deliberately not repeated here:
# `canary.md`'s rule is that the asserted *counts* live in the `Makefile` and nowhere else, and a
# second copy of a SHA is a second copy that goes stale. `CANARY_DIR` is the one path both need.
CANARY_DIR      ?= tmp/corpora/lobsters
CANARY_FILES    ?= 606
CANARY_WARNINGS ?= 14
CANARY_MAX_MS   ?= 500

## canary: open a real Rails app (lobsters, pinned) and check the answers
.PHONY: canary
canary: release canary-clone
	python3 scripts/canary.py \
	  --repo $(CANARY_DIR) --server target/release/ya-lsp \
	  --files $(CANARY_FILES) --parse-warnings $(CANARY_WARNINGS) \
	  --max-index-ms $(CANARY_MAX_MS)

# One implementation of "fetch a pinned commit", in `scripts/corpora.py`, which this delegates
# to. It fetches one commit rather than cloning a history, and never deletes what is already
# there: a working tree with local edits fails the checkout instead of losing them.
#
# **Every question it asks is asked of `<dir>/.git` and never of `git -C`'s answer, because the
# corpus workspaces live inside this repository and git searches *upwards*.** `git -C tmp/x
# rev-parse --git-dir` in an empty `tmp/x` succeeds and answers about **ya-lsp** — so the obvious
# spelling of "is this a repo yet?" skips the `init`, adds a remote to ya-lsp, fetches lobsters
# into ya-lsp's object store and then runs `checkout --detach` on the working tree being
# developed in. It was written that way once; what stopped it was an unrelated dirty tree.
## canary-clone: fetch the pinned commit of the canary workspace
.PHONY: canary-clone
canary-clone:
	@python3 scripts/corpora.py clone --only lobsters

# ---------------------------------------------------------------------------- the audit

# `scripts/audit/` opens every sweepable corpus, asks a stratified sample of real cursors, and
# scores the answers. `.claude/rules/audit.md` is the rule; what belongs here is the entry point
# and the one path both halves of it need.
#
# **Two commands and not one, because the second needs no server.** `score` sweeps and records;
# `report` diffs that recording against the committed baseline. Splitting them is what lets a diff
# be re-read, re-cut and re-run in CI from an artifact long after the machine that swept is gone —
# and a report that had to re-sweep to say what moved could only ever be run where the corpora are.
#
# **It needs the corpora, so it is not in `make ci` and is not a CI job.** `make corpora` is six
# applications and their bundles; the audit itself needs only the clones, and a corpus that is
# missing or has drifted from its pin is skipped by name rather than measured. That is the same
# reason `canary` is out of `ci`, one order of magnitude further along.
#
# The baseline is committed and the ledger is committed, and **neither holds a word of corpus
# source** — `corpora.md`'s licence rule, which is blanket. A finding travels into the baseline as
# a path and a byte offset, a ledger row as `sha256(line)`. The identifier under the cursor reaches
# the terminal and stops there.
AUDIT_RUN ?= tmp/audit-run.json

# `ARGS` reaches `score` and **not** `report`, because the two take different flags — `-n` is not
# a thing you can report on — and because it would be the wrong knob anyway: the record is already
# only the corpora that were swept, so `ARGS=--only lobsters` restricts the diff by restricting
# what there is to diff.
## audit: sweep all six corpora, then diff against the committed baseline
.PHONY: audit
audit: release
	python3 scripts/audit score --record $(AUDIT_RUN) $(ARGS)
	@python3 scripts/audit report $(AUDIT_RUN)

## audit-sample: the draw only, and its mix against the stated one; no server, no requests
.PHONY: audit-sample
audit-sample:
	@python3 scripts/audit sample $(ARGS)

## audit-cost: what one position costs, and the sample size the budget buys
.PHONY: audit-cost
audit-cost: release
	@python3 scripts/audit cost $(ARGS)

## audit-prefix: what the untyped completion list costs and buys at each prefix length
.PHONY: audit-prefix
audit-prefix: release
	@python3 scripts/audit prefix $(ARGS)

## audit-rank: where the member sits in a typed completion list, at each prefix length
.PHONY: audit-rank
audit-rank: release
	@python3 scripts/audit rank $(ARGS)

## audit-ledger: what is in audit/ledger.json, and whether it still applies
.PHONY: audit-ledger
audit-ledger:
	@python3 scripts/audit ledger $(ARGS)

# Blessing the last sweep as the new baseline is a **separate** target and never a flag on
# `audit`, because it is the one step that changes what a future run is judged against. It merges:
# a run over one corpus keeps the other four's recorded numbers and says which it carried.
## audit-baseline: record the last `make audit` sweep as the committed baseline
.PHONY: audit-baseline
audit-baseline:
	python3 scripts/audit report $(AUDIT_RUN) --save

# ---------------------------------------------------------------------------- notices

## notices: regenerate THIRD-PARTY-NOTICES.txt
.PHONY: notices
notices:
	$(CARGO) about generate about.hbs -o THIRD-PARTY-NOTICES.txt

## notices-check: fail if the committed notices are stale
.PHONY: notices-check
notices-check:
	@mkdir -p target
	@$(CARGO) about generate about.hbs -o target/notices.txt
	@diff -u THIRD-PARTY-NOTICES.txt target/notices.txt \
	  || { echo "THIRD-PARTY-NOTICES.txt is stale; run: make notices"; exit 1; }

# ---------------------------------------------------------------------------- extension

## ext-install: install the extension's dependencies
.PHONY: ext-install
ext-install:
	cd $(EXTENSION_DIR) && yarn install --frozen-lockfile

## ext-test: compile, bundle and test the extension
.PHONY: ext-test
ext-test:
	cd $(EXTENSION_DIR) && yarn test

## ext-lint: type-check the extension
.PHONY: ext-lint
ext-lint:
	cd $(EXTENSION_DIR) && yarn lint

# ---------------------------------------------------------------------------- meta

## setup: install the cargo subcommands and the nightly used for coverage
.PHONY: setup
setup: setup-coverage setup-notices

# Split out for the reason `setup-coverage` is, and it was the one that needed it: `ci.yml` used
# to spell `cargo install cargo-about --locked --features cli --version ^0.9` itself, beside a
# hand-written `cargo about generate | diff`. So the pin lived in two places, and the committed
# notice could be checked against a different tool than the one that wrote it — which is the
# failure the whole check exists to catch, one level up.
## setup-notices: install just cargo-about
.PHONY: setup-notices
setup-notices:
	$(CARGO) install cargo-about --locked --features cli --version "$(CARGO_ABOUT_VERSION)"

# Split out so CI installs exactly what a local `make coverage` needs, at exactly the version
# written above, without the workflow carrying a second copy of the number.
## setup-coverage: install just the nightly toolchain and cargo-llvm-cov
.PHONY: setup-coverage
setup-coverage:
	rustup toolchain install nightly --component llvm-tools-preview
	$(CARGO) install cargo-llvm-cov --locked --version $(CARGO_LLVM_COV_VERSION)

# `docs-check` is in here for the reason `fmt-check` is: it is hermetic, it costs a `cargo doc`
# over a tree that is already built, and the thing it catches is invisible to every other target.
# A doc link that points at nothing breaks no build, fails no test and moves no coverage number.
#
# `notices-check` is in here because the `server` job runs it, and the header above promises a
# green `make ci` means a green CI run. It costs `make setup` — cargo-about — the same way
# `coverage` costs nightly and cargo-llvm-cov, and it is the target most likely to fail on a
# branch that touched `Cargo.toml`, which is exactly when nobody thinks to run it.
#
# `canary` is deliberately not in here. Everything above is hermetic: it needs the source tree
# and nothing else. The canary clones 12 MB from GitHub, so folding it in would make every local
# `make ci` need a network — and a target that fails on a plane teaches people to skip it. CI
# runs it as its own job, where the name in the checks list says what it covers.
## ci: everything CI checks about the server
.PHONY: ci
ci: fmt-check lint docs-check test notices-check coverage

## clean: cargo clean
.PHONY: clean
clean:
	$(CARGO) clean
