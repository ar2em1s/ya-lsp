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
#   ruby_version.rs  which Ruby, and therefore which stdlib. Pure text; the failure it exists to
#                    prevent is macOS's vestigial 2.6 answering for a 4.0 project.
#   bundler.rs       Gemfile.lock -> sources and specs. Pure text, no I/O, so there is nothing it
#                    cannot be asked; mis-parse a line and those gems are silently not indexed.
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
#                    catches the failure item 6 exists to prevent: an arm of a message that
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
#
# Deliberately *not* here, so the next reader does not re-litigate it:
#   signatures.rs    highest blast radius in the crate — a bug indexed 7 of Array's 197 methods
#                    through a green suite — but four of its fourteen branches are `try_from`
#                    guards that cannot fail on a 64-bit build. Not reachable, so not listed.
#   symbols.rs       VS Code *throws* on a bad selectionRange, discarding the whole outline; its
#                    residual gap is a 64-deep nesting walk no Ruby file produces.
#   progress.rs      already at 100, but a stream left open is a visible spinner, not a silent
#                    wrong answer. Being at 100 is not by itself a reason to be on this list.
COVERAGE_FLOORS ?= \
  analysis/position.rs=100:100 \
  analysis/ranges.rs=100 \
  analysis/diagnostics.rs=100 \
  analysis/references.rs=100:100 \
  analysis/render.rs=100:100 \
  messages.rs=100 \
  analysis/rename.rs=100:100 \
  analysis/scopes.rs=100:100 \
  workspace/uri.rs=100 \
  workspace/config.rs=100:100 \
  workspace/bundler.rs=100:100 \
  workspace/ruby_version.rs=100:100 \
  server/capabilities.rs=100 \
  licenses.rs=100

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
# numbers stay manual — `.claude/rules/benchmarking.md` — and a green canary does not cover them.
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
CANARY_REPO     ?= https://github.com/lobsters/lobsters.git
CANARY_SHA      ?= 6d15d8f118e305b1de5190662a9651bf90132784
CANARY_DIR      ?= tmp/lobsters
CANARY_FILES    ?= 476
CANARY_WARNINGS ?= 14
CANARY_MAX_MS   ?= 500

## canary: open a real Rails app (lobsters, pinned) and check the answers
.PHONY: canary
canary: release canary-clone
	python3 scripts/canary.py \
	  --repo $(CANARY_DIR) --server target/release/ya-lsp \
	  --files $(CANARY_FILES) --parse-warnings $(CANARY_WARNINGS) \
	  --max-index-ms $(CANARY_MAX_MS)

# Fetches one commit rather than cloning a history, and never deletes what is already there: a
# working tree with local edits fails the checkout instead of losing them. `--depth 1` on a
# 12 MB repository, and a no-op once the pin is present.
#
# **Every question here is asked of `$(CANARY_DIR)/.git` and never of `git -C`'s answer, because
# the canary workspace lives inside this repository and git searches *upwards*.** `git -C
# tmp/x rev-parse --git-dir` in an empty `tmp/x` succeeds and answers about **ya-lsp** — so the
# obvious spelling of "is this a repo yet?" skips the `init`, adds a remote to ya-lsp, fetches
# lobsters into ya-lsp's object store and then runs `checkout --detach` on the working tree
# being developed in. It was written that way once; what stopped it was an unrelated dirty tree.
# The `-e` test cannot walk up, and the toplevel comparison refuses the case where somebody
# points `CANARY_DIR` at the repository root itself.
## canary-clone: fetch the pinned commit of the canary workspace
.PHONY: canary-clone
canary-clone:
	@set -e; \
	dir='$(CANARY_DIR)'; \
	if [ -e "$$dir/.git" ] \
	   && [ "$$(git -C "$$dir" rev-parse HEAD 2>/dev/null)" = "$(CANARY_SHA)" ]; then \
	  echo "canary: $$dir is at $(CANARY_SHA)"; \
	else \
	  mkdir -p "$$dir"; \
	  if [ "$$(cd "$$dir" && pwd -P)" = "$$(pwd -P)" ]; then \
	    echo "canary: CANARY_DIR is the ya-lsp working tree; refusing"; exit 2; \
	  fi; \
	  echo "canary: fetching $(CANARY_SHA) into $$dir"; \
	  [ -e "$$dir/.git" ] || git -C "$$dir" init -q; \
	  git -C "$$dir" remote get-url canary >/dev/null 2>&1 \
	    || git -C "$$dir" remote add canary $(CANARY_REPO); \
	  git -C "$$dir" fetch -q --depth 1 canary $(CANARY_SHA); \
	  git -C "$$dir" checkout -q --detach FETCH_HEAD; \
	fi

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
ci: fmt-check lint test notices-check coverage

## clean: cargo clean
.PHONY: clean
clean:
	$(CARGO) clean
