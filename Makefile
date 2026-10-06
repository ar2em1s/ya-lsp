# ya-lsp: the commands, in one place.
#
# `make` on its own lists them. Everything here is what CI runs, spelled the same way, so a green
# `make ci` locally means a green CI run.
#
# Two tool versions are pinned below. They are cargo *subcommands*, which cargo cannot express as
# dependencies (it never builds a dependency's binaries), so `make setup` installs them, and this
# file is the one place their versions are written down.

CARGO         ?= cargo
NIGHTLY       ?= +nightly
ARGS          ?=
EXTENSION_DIR := editors/vscode

# The coverage bars, enforced by `scripts/coverage.sh`: cargo-llvm-cov can fail a build on lines
# but has no `--fail-under-branches`, which is why that script exists.
#
# One bar for both. What stays uncovered is conditions no input reaches: `let ... else` on a
# rubydex lookup a consistent graph cannot fail, a non-UTF-8 filename APFS will not create, a
# panicked analysis thread. Raise these as real arms get covered; never meet one by deleting a
# guard, which is the same edit as widening `#[coverage(off)]`. `make coverage-branches` names
# every arm still untaken.
MIN_LINES     ?= 95
MIN_BRANCHES  ?= 95

# A floor every file clears on its own, so one bad file cannot hide inside a good average: a new
# module landing at 80% barely moves the project number. Deliberately *below* the project bar:
# - some files' remaining lines are continuations of multi-line `tracing::info!` calls, which run
#   only when the level is on, and covering them means raising the log level, which asserts
#   nothing;
# - a small file like `main.rs` moves several points per uncovered line, so a high uniform bar
#   measures file size more than testing.
# Named modules that must be complete go in COVERAGE_FLOORS below.
MIN_FILE_LINES ?= 90

# Modules held to 100%, above the project-wide bar. cargo-llvm-cov's `--fail-under-file-lines`
# cannot express this (it applies one number to every file, in practice whatever the weakest file
# can pass), so `scripts/coverage.sh` gates them. Each entry is `path=lines[:branches]`; a path
# missing from the report is an error, not a pass.
#
# Both questions must answer yes:
# 1. **Is being wrong here silent and wide?** A visibly broken hover is found in a day; a
#    mis-parsed lockfile is not.
# 2. **Is 100 structurally reachable?** A file whose gap is `usize::try_from` on a 64-bit build can
#    never hold the bar, and listing it would only teach people to edit this list.
#
#   position.rs      LSP position <-> byte offset and incremental edits. Wrong here silently
#                    corrupts the user's file, and no other test would notice.
#   uri.rs           the workspace root and the one spelling of a document key. Wrong, and
#                    `didOpen` forks a second document, or the wrong tree is indexed.
#   config.rs        decides every default, so it decides every other answer.
#   capabilities.rs  the wire contract, fixed at `initialize` and never renegotiated.
#   diagnostics.rs   the rule -> severity table. A transposed row squiggles working code; a renamed
#                    rule silently stops matching the user's ya-lsp.toml keys.
#   licenses.rs      what `--licenses` prints. Wrong here is a licence nobody granted.
#   features.rs      which bodies of knowledge apply, so what every later answer is made of. Silent
#                    both ways: a family switched off answers nothing, and `auto` guessing wrong
#                    cites a controller that does not exist.
#   logging.rs       whether anything is written at all. A filter one level too quiet, or a file
#                    sink that never opened, looks exactly like a server with nothing to say, and
#                    every later bug report is assembled from it. The one untestable line
#                    (installing the global subscriber) is in `main.rs`.
#   ruby_version.rs  which Ruby, and so which stdlib. Pure text; it stops macOS's vestigial 2.6
#                    answering for a 4.0 project.
#   bundler.rs       Gemfile.lock -> sources and specs. Pure text; mis-parse a line and those gems
#                    are silently not indexed.
#   code_actions.rs  writes, like rename.rs: the failure is a broken buffer. Every guard is a
#                    *refusal*, so an untested one is an action offered where it should not be,
#                    invisible until somebody applies it.
#   render.rs        the one place a construct is spelled for a human (hover, outline, picker),
#                    plus RDoc markup -> markdown: a `<vowel>` a renderer eats does not look broken,
#                    it looks like a sentence with a word missing.
#   references.rs    a truncated find-all-references looks exactly like a complete one, so the cap
#                    and the synthetic-reference filter are both silent when wrong.
#   ranges.rs        folding and expand-selection. Advertising a folding provider takes away the
#                    editor's indentation guess, so a construct this misses quietly loses folding
#                    everywhere. Lines only: `make coverage-branches F=ranges` finds no untaken
#                    arm, but the summary counts two merged regions apart.
#   messages.rs      every sentence a user reads. Pure formatting with no branch regions, so lines
#                    are the whole gate, and exactly what catches a message arm that ships unread.
#                    The enumeration test guards the set; this guards each message.
#   rename.rs        writes the user's files. A rule that stops firing edits them wrongly: Ruby
#                    3.1's `{ x:, y: }` renames the hash key with the value and still parses, the
#                    widest and quietest failure ya-lsp can have.
#   scopes.rs        which variable is which, listed for rename, not highlighting: a scope bug that
#                    lights the wrong occurrences is seen at once; behind a rename it overwrites
#                    the wrong one.
#   erb.rs           the Ruby view of a template; every offset depends on it. Padding by character
#                    instead of byte shifts every answer below an accent or emoji, silently, and
#                    only for people who do not write their markup in English.
#   synthesized.rs   where a generated declaration was really declared. Silent both ways: a wrong
#                    mapping opens the wrong line confidently, a missing one opens nothing.
#                    `generated_uri` is total, so no unreachable arm blocks 100.
#   generated.rs     the RBS this crate writes, and where each declaration came from. `append`
#                    shifts every span by another generator's length; off by one is a jump to the
#                    wrong line, synthesized.rs' failure one layer earlier.
#   structs.rs       `Struct.new` and `Data.define`: rails/'s argument without Rails. A member on
#                    the wrong constant answers confidently about the wrong class, and a declined
#                    call looks exactly like a project with no structs.
#   hints.rs         the answer nobody asked for: a label painted on every line and read as fact,
#                    so a guess reaching one is the widest, quietest wrong answer. Its guards are
#                    refusals, `code_actions.rs`' argument.
#   environment.rs   the test-tree tag, read by several surfaces, some of which must never act on
#                    it. Too wide deletes rows nobody knows to look for; too narrow restores a leak
#                    whose only symptom is a slightly longer list. No audit lane scores it, so the
#                    suite is the only instrument.
#   rails/           *everything* ya-lsp knows about Rails, which is why it is one directory and
#                    every file is listed: a convention reaching the wrong class answers wrongly
#                    and confidently, and one reaching nothing looks like a project that does not
#                    follow it. `rails/mod.rs` is *not* listed: it has no executable line, so
#                    llvm-cov emits no row, and a floor missing from the report is an error.
#
# Deliberately *not* here:
#   signatures.rs    highest blast radius in the crate, but some of its branches are `try_from`
#                    guards that cannot fail on a 64-bit build. Not reachable, so not listed.
#   symbols.rs       VS Code *throws* on a bad selectionRange, discarding the whole outline; its
#                    residual gap is a 64-deep nesting walk no Ruby file produces.
#   progress.rs      at 100, but a stream left open is a visible spinner, not a silent wrong
#                    answer. Being at 100 is not by itself a reason to be listed.
#   analysis/synthesize.rs  the pass itself. Its residual is unreachable: `workspace_relative`'s
#                    `to_path()` failing, which a `DocUri` cannot do, and the arguments of a
#                    `tracing::debug!` the macro evaluates only when that level is on.
#   annotations.rs   pure text and silent when wrong, but its residual line is a match arm over
#                    Prism's two-kind keyword-parameter list. Not reachable, so not listed.
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
  workspace/rails/adapters.rs=100:100 \
  workspace/rspec.rs=100:100 \
  workspace/factories.rs=100:100 \
  workspace/singletons.rs=100:100 \
  workspace/defines.rs=100:100 \
  workspace/mixins.rs=100:100 \
  workspace/i18n.rs=100:100 \
  workspace/rails/conventions.rs=100:100 \
  workspace/rails/inflect.rs=100:100 \
  workspace/rails/layouts.rs=100:100 \
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
  workspace/rails/blocks.rs=100:100 \
  workspace/rails/migrations.rs=100:100 \
  workspace/rails/routes.rs=100:100 \
  workspace/rails/request.rs=100:100 \
  workspace/rails/targets.rs=100:100 \
  workspace/rails/actions.rs=100:100 \
  workspace/rails/renders.rs=100:100 \
  workspace/rails/syntax.rs=100:100 \
  workspace/rails/tail.rs=100:100 \
  workspace/rails/callbacks.rs=100:100 \
  workspace/rails/current.rs=100:100 \
  workspace/rails/mod.rs=100 \
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
# there merges nightly counters against a stable covmap and reports a plausible, entirely wrong
# number, so the toolchain that produced it is stamped, and a change wipes it.
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

# Only the **broken** class is denied; the other two rustdoc lints are allowed on purpose.
# - `private_intra_doc_links` fires often, and correctly: this crate is private modules almost end
#   to end, and the only fixes are making an internal item `pub` or turning a link that works in
#   an editor into plain text, both worse than the warning.
# - `redundant_explicit_links` is pure style.
# What is left is a link that points at **nothing**: a rename its comment did not follow. A grep
# finds those only if somebody already suspects them; rustdoc finds them every run.
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
# run instead of re-running the tests per format.
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

# The six benchmark corpora, pinned. `scripts/corpora.toml` is the table (repository, commit,
# Ruby, licence), and `scripts/corpora.py` makes a machine match it. The reasoning lives in both
# files and in `.claude/rules/corpora.md`; only the entry point belongs here.
#
# **Needs a network and a Ruby toolchain, so it is in neither `make ci` nor CI.** The canary is
# one small clone; this is six applications and their bundles.
#
# **It never installs a Ruby.** Each corpus declares one, and the script resolves it against what
# asdf has, printing the `asdf install ruby X` line when it cannot. `--allow-nearest` accepts
# another patch of the same MAJOR.MINOR, preferring one whose bundle already resolves, and says
# so in the manifest.
#
# `make corpora-status ARGS=--json` is the manifest a measurement carries beside its numbers:
# without it, an absolute count from today cannot be compared with one from last week, because
# nothing recorded the commit or how much of the bundle was installed.

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
# reasoning; these are the numbers, kept here for the coverage bars' reason: a local run and the
# CI run cannot disagree about them.
#
# **It does not cover gems**, for cost, not oversight: resolving lobsters' bundle needs
# `bundle install`, a recent Ruby and a hand-built `sqlite3`, a lot of CI for a project whose
# headline is that it needs no Ruby. A green canary says nothing about gems.
#
# lobsters is BSD-3-Clause, (c) 2012-2019 Joshua Stein. It is **cloned, never vendored**: no
# artifact this project ships contains any of it, so no notice is owed (`licensing.md`: the rule
# is per artifact, not per repository). That is a property of how it is used, not of the licence:
# copy one file into `tests/`, or cache a tarball here, and the obligation attaches.
#
# The commit is pinned because an unpinned target turns a canary into a flake and makes its
# numbers meaningless across runs. The ceiling is an order of magnitude above a typical local
# index: a shared runner with a cold page cache is slower, and what this catches (an accidental
# quadratic, a discovery rule that stops matching) moves the number by a factor, not a percent.
#
# The repository, commit and Ruby live in `scripts/corpora.toml`, which pins all six corpora. They
# are deliberately not repeated here: `canary.md`'s rule is that the asserted *counts* live in the
# `Makefile` and nowhere else, and a second copy of a SHA goes stale. `CANARY_DIR` is the one path
# both need.
CANARY_DIR      ?= tmp/corpora/lobsters
CANARY_FILES    ?= 612
CANARY_WARNINGS ?= 14
CANARY_MAX_MS   ?= 500

## canary: open a real Rails app (lobsters, pinned) and check the answers
.PHONY: canary
canary: release canary-clone
	python3 scripts/canary.py \
	  --repo $(CANARY_DIR) --server target/release/ya-lsp \
	  --files $(CANARY_FILES) --parse-warnings $(CANARY_WARNINGS) \
	  --max-index-ms $(CANARY_MAX_MS)

# One implementation of "fetch a pinned commit", in `scripts/corpora.py`. It fetches one commit,
# not a history, and never deletes what is there: a working tree with local edits fails the
# checkout instead of losing them.
#
# **Every question is asked of `<dir>/.git`, never of `git -C`'s answer, because the corpus
# workspaces live inside this repository and git searches *upwards*.** `git -C tmp/x rev-parse
# --git-dir` in an empty `tmp/x` succeeds and answers about **ya-lsp**. The obvious spelling of
# "is this a repo yet?" would then skip the `init`, add a remote to ya-lsp, fetch lobsters into
# ya-lsp's object store, and run `checkout --detach` on the working tree being developed in.
## canary-clone: fetch the pinned commit of the canary workspace
.PHONY: canary-clone
canary-clone:
	@python3 scripts/corpora.py clone --only lobsters

# ---------------------------------------------------------------------------- the audit

# `scripts/audit/` opens every sweepable corpus, asks a stratified sample of real cursors, and
# scores the answers. `.claude/rules/audit.md` is the rule; only the entry point and the one path
# both halves need belong here.
#
# **Two commands, because the second needs no server.** `score` sweeps and records; `report`
# diffs that record against the committed baseline. So a diff can be re-read, re-cut and re-run in
# CI from an artifact long after the sweeping machine is gone; a report that had to re-sweep could
# only run where the corpora are.
#
# **It needs the corpora, so it is in neither `make ci` nor CI.** The audit needs only the clones,
# and a corpus that is missing or has drifted from its pin is skipped by name, not measured.
#
# The baseline and the ledger are committed, and **neither holds a word of corpus source**
# (`corpora.md`'s licence rule, which is blanket). A finding enters the baseline as a path and a
# byte offset, a ledger row as `sha256(line)`. The identifier under the cursor reaches the
# terminal and stops there.
AUDIT_RUN ?= tmp/audit-run.json

# `ARGS` reaches `score`, **not** `report`: the two take different flags (`-n` means nothing to a
# report), and the record already holds only the corpora that were swept, so
# `ARGS=--only <name>` restricts the diff by restricting what there is to diff.
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

## audit-latency: what one request makes a person wait — p50/p95/max, and the empty count
.PHONY: audit-latency
audit-latency: release
	@python3 scripts/audit latency $(ARGS)

## audit-ledger: what is in audit/ledger.json, and whether it still applies
.PHONY: audit-ledger
audit-ledger:
	@python3 scripts/audit ledger $(ARGS)

# Blessing the last sweep as the baseline is a **separate** target, never a flag on `audit`: it is
# the one step that changes what future runs are judged against. It merges: a run over one corpus
# keeps the other corpora's recorded numbers and says which it carried.
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

# Split out, like `setup-coverage`, so CI installs cargo-about at the version written above, with
# no second copy of the pin. A notice checked with a different tool than the one that wrote it is
# the failure the check exists to catch.
## setup-notices: install just cargo-about
.PHONY: setup-notices
setup-notices:
	$(CARGO) install cargo-about --locked --features cli --version "$(CARGO_ABOUT_VERSION)"

# Split out so CI installs exactly what a local `make coverage` needs, at exactly the version
# written above, with no second copy of the number in the workflow.
## setup-coverage: install just the nightly toolchain and cargo-llvm-cov
.PHONY: setup-coverage
setup-coverage:
	rustup toolchain install nightly --component llvm-tools-preview
	$(CARGO) install cargo-llvm-cov --locked --version $(CARGO_LLVM_COV_VERSION)

# `docs-check` is here for `fmt-check`'s reason: it is hermetic, costs a `cargo doc` over an
# already-built tree, and catches something invisible to every other target. A doc link that
# points at nothing breaks no build, fails no test and moves no coverage number.
#
# `notices-check` is here because the `server` job runs it, and the header promises a green
# `make ci` means a green CI run. It needs `make setup` (cargo-about), as `coverage` needs nightly
# and cargo-llvm-cov, and it is the target most likely to fail on a branch that touched
# `Cargo.toml`: exactly when nobody thinks to run it.
#
# `canary` is deliberately not here. Everything above is hermetic: it needs the source tree and
# nothing else. The canary clones from GitHub, so folding it in would make every local `make ci`
# need a network, and a target that fails on a plane teaches people to skip it. CI runs it as its
# own job, where its name in the checks list says what it covers.
## ci: everything CI checks about the server
.PHONY: ci
ci: fmt-check lint docs-check test notices-check coverage

## clean: cargo clean
.PHONY: clean
clean:
	$(CARGO) clean
