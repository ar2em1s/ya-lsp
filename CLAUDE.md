# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`ya-lsp` — "Yet Another LSP": a Language Server Protocol implementation for **Ruby**, written in Rust. The stated goals are lightweight, fast, and standalone (no Ruby runtime dependency).

## Current state

**M0–M7 are complete** — M0 (walking skeleton), M1 (diagnostics), M2 (navigation core), M3 (gem
indexing), M4 (workspace symbols & references), M5 (completion), M6 (the VS Code extension), and
M7 (Ruby's own core and stdlib). The cache question is **closed — no cache**: 0.77 s cold on a
151-gem Rails app is twenty times inside the threshold where one would begin to pay for itself.

**M8 — coverage — is complete.** Every feature milestone before it shipped without any test asking
whether the code it added was reached at all. The suite now runs under `cargo-llvm-cov` on every CI
push and is gated three ways: **95% of lines and branches** across the project, **90% of lines in
every file on its own**, and **100% in ten critical modules** — measured 97.87% and 95.58%, against
a baseline of 91.86% and 79.08%. All three live in the `Makefile`; `scripts/coverage.sh` enforces
them. `.claude/rules/coverage.md` has the rules, what the remaining gap is made of, and the
three moves that closed sixteen points of branch coverage.

```
src/
  main.rs          CLI (--stdio, --licenses, -V, -h), stderr-only tracing init
  lib.rs           module root (a lib target exists so tests can drive the server in-process)
  licenses.rs      what `--licenses` prints; concatenates LICENSE.txt, NOTICE.txt, notices
  messages.rs      every sentence a user reads, one `pub fn` each, and the rule they meet
  server/          LSP lifecycle, capability negotiation, dispatch loop
    capabilities.rs
  analysis/        the analysis thread; owns the rubydex Graph
    completion.rs  what the graph offers at the cursor; ranking, cap, resolve
    cursor.rs      what the cursor is shaped like; the second direct Prism use
    diagnostics.rs rule -> severity table, rubydex Diagnostic -> LSP
    locator.rs     cursor -> target -> declaration -> where it lives
    symbols.rs     documentSymbol, nested and flat
    search.rs      workspace/symbol: rubydex filters, we rank and cap
    references.rs  textDocument/references: exact constants, name-based methods
    hover.rs       hover markdown
    requires.rs    `require "..."` under the cursor
    render.rs      how Ruby constructs are spelled for humans
    progress.rs    $/progress streams (gem indexing)
    signatures.rs  blanks RBS `interface` blocks before a file is indexed
    position.rs    LSP position <-> byte offset, UTF-8/16/32, incremental edits
  workspace/       workspace root, discovery
    config.rs      ya-lsp.toml
    bundler.rs     Gemfile.lock -> sources and specs (pure text, no I/O)
    ruby_version.rs which Ruby, without running ruby
    gems.rs        gem roots, spec -> directory, require_paths, Ruby's own lib, file walk
    rbs.rs         which copy of Ruby's signatures to index; the vendored fallback
    uri.rs         canonical document URIs
Makefile           every command, and the two cargo subcommands' pinned versions
scripts/
  coverage.sh      the coverage gate: both bars, and every untaken branch arm
build.rs           embeds vendor/rbs into the binary
vendor/rbs/        Ruby's own RBS signatures, vendored (BSD-2-Clause/Ruby, see its README)
tests/lifecycle.rs end-to-end against a spawned binary
editors/vscode/    the VS Code extension (TypeScript)
  src/extension.ts activation, one client per workspace folder, commands
  src/config.ts    VS Code settings -> the server's wire format; no `vscode` import
  src/server.ts    which binary to run; no `vscode` import
.github/workflows/ CI, and the per-platform VSIX matrix
tmp/               directory for temporary files; gitignored
```

Settled choices: **`lsp-server` 0.10** (sync, from rust-analyzer) as the transport, **`rubydex`
pinned at `=0.2.5`** for indexing and static analysis, **`ruby-prism` `=1.9.0`** (must match
rubydex's exactly or `ruby-prism-sys` links twice), no async runtime.

## Rules

The invariants that used to live here are now `.claude/rules/*.md`, each carrying a `paths:`
frontmatter list so it loads only while you are working on the files it governs. `mood.md` and
`standards.md` have no `paths:` and are always in context.

| Rule | Loads when you touch |
|---|---|
| `core-invariants.md` | `src/**`, `tests/**`, `build.rs` |
| `navigation.md` | `analysis/` locator, symbols, hover, render, requires, search |
| `completion.md` | `analysis/completion.rs`, `analysis/cursor.rs` |
| `search-references.md` | `analysis/search.rs`, `analysis/references.rs` |
| `gems.md` | `workspace/` gems, bundler, ruby_version, config, mod |
| `rbs-signatures.md` | `workspace/rbs.rs`, `analysis/signatures.rs`, `vendor/rbs/`, `build.rs` |
| `messages.md` | `src/messages.rs` and the five files that raise its messages |
| `coverage.md` | `Makefile`, `scripts/`, `.github/workflows/` |
| `benchmarking.md` | `tests/`, `src/main.rs`, `src/server/` |
| `vscode-extension.md` | `editors/vscode/**` |
| `licensing.md` | `LICENSE.txt`, `NOTICE.txt`, `THIRD-PARTY-NOTICES.txt`, `about.*`, `src/licenses.rs`, `vendor/rbs/`, `CHANGELOG.md` |

A rule fires on `path_glob_match` — when a matching file is actually read or edited. If you are
about to reason about a module without opening it, or to create a new file under a governed path,
read its rule first; `.claude/rules/` is small enough to `cat` in full.

## Commands

**Everything runs through the `Makefile`; `make` on its own lists the targets.** It is the single
place the commands and the two pinned tool versions live — `cargo-llvm-cov` and `cargo-about` are
cargo *subcommands*, which cargo cannot express as dev-dependencies because it never builds a
dependency's binaries, so `make setup` installs them and the Makefile is where their versions are
written down. CI runs the same targets, so a local run and a CI run cannot disagree about the tool,
the bars, or the flags.

```bash
make                   # list every target
make setup             # nightly + llvm-tools, cargo-llvm-cov, cargo-about
make build             # debug build           make release       # optimized build
make test              # the Rust suite        make test-one T=x  # one test, with stdout
make check             # type-check only       make lint          # clippy, warnings denied
make fmt               # format                make fmt-check     # fail if unformatted
make ci                # fmt-check, lint, test, coverage — what CI checks

make coverage          # run the suite instrumented, then check all three bars
make coverage-branches # every branch arm no test took (F=gems to filter)
make coverage-missing  # every line no test ran
make coverage-html     # the browsable report
make coverage-clean    # drop the profiles and the nightly objects

make notices           # regenerate THIRD-PARTY-NOTICES.txt, after any dependency change
make notices-check     # fail if the committed copy is stale, as CI does
make ext-install       # the extension's dependencies (yarn 1's `npm ci`)
make ext-test          # compile, bundle and run the extension's 24 tests
make ext-lint          # type-check the extension
```

`make coverage` splits into `coverage-run` (the instrumented suite, once) and `coverage-check` (the
gate), so every other coverage target reads that same run rather than re-running the tests per
format. `coverage-run` stamps the toolchain that produced `target/llvm-cov-target` and wipes the
directory when it changes, which is the `rm -rf` rule above automated rather than remembered.

```bash
# The extension, when a target does not cover it (from editors/vscode)
yarn bundle                      # dist/extension.js, what actually ships
yarn vsce package --target darwin-arm64   # a VSIX; needs server/ya-lsp staged first
```

The extension's `contract.test.ts` talks to a real `ya-lsp`; it looks for
`editors/vscode/server/ya-lsp`, then `target/release`, then `target/debug`, and **skips itself**
if it finds none — so build the server before trusting a green run. Press F5 in the repo root to
launch an extension host with everything built; `.vscode/tasks.json` does the staging.

What the targets above run, for when one needs a flag it does not carry:

```bash
cargo run -- --stdio         # the server, as an editor spawns it
cargo test <name>            # tests whose path/name contains <name>
cargo test <name> -- --exact --nocapture   # one exact test, with stdout shown
```

Driving the binary against a real Ruby repository by hand, and the number that matters for each
milestone (M3–M7), is `.claude/rules/benchmarking.md`.

## Toolchain

Rust **1.93.1**, pinned in `.tool-versions` (asdf/mise). Crate uses **edition 2024** — `unsafe_op_in_unsafe_fn`, `gen` as a keyword, and the new `impl Trait` capture rules apply; don't write edition-2021 idioms that the 2024 lints reject.
