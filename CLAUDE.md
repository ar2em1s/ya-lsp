# CLAUDE.md

`ya-lsp` is a Ruby language server written in Rust. It never runs Ruby.

## Before you edit

1. Read the `.claude/rules/*.md` file that governs the path (see [Rules](#rules)). Rules load
   automatically only once a matching file is read, so read the rule first when creating a file.
2. Run everything through `make`. Plain `make` lists the targets.
3. Before a PR, run `make ci`, which is what CI runs.

## What the server does

It answers 29 LSP request methods, plus `publishDiagnostics`, from a rubydex graph it builds itself.

**Requests:** `definition`, `implementation`, `typeDefinition`, `declaration`, `hover`,
`documentSymbol`, `workspace/symbol`, `references`, type hierarchy (3), call hierarchy (3),
`documentHighlight`, `prepareRename` + `rename`, `signatureHelp`, `foldingRange`,
`selectionRange`, `semanticTokens/full`, `codeAction`, `completion` + `resolve`, `documentLink`,
`inlayHint`, `workspace/textDocumentContent`, `workspace/executeCommand`,
`workspace/willRenameFiles`.

**What gets indexed:**

- The workspace's Ruby and ERB files.
- Every gem's `lib/`, plus a Rails engine's `app/`.
- RBS signatures: each gem's `sig/`, `.gem_rbs_collection/`, the project's `sig/`, and Ruby's
  core and stdlib (from the installed `rbs` gem, or the copy vendored in the binary).
- Translations are read, not indexed: the main locale's keys in Rails' own locale files, every
  gem's `config/locales` and the project's (`synthesized.md`).

There is **no cache**, because a cold open of a real Rails app takes well under a second.

## Invariants that span files

Each bullet names the rule file that holds the detail.

1. **Three tiers inside, two on screen.** *Resolved* means the code names the type. *Derived*
   means a signature, an assignment or a convention does. *Guessed* means only a name matched,
   and `[types] guess_from_names` turns guessing off. A card says one thing about its confidence,
   *Guessed from name alone.*, and nothing about how an answer was found, so Derived reads as
   Resolved (decided 2026-09-29; `navigation.md`). The audit scores the same two tiers.
   - A guess is never drawn as an inlay hint (`hints.md`).
   - `implementation`, `typeDefinition` and `declaration` decline a guessed answer through one
     shared gate (`hierarchy.md`, `navigation.md`).
2. **Receiver rungs, in this order:** rubydex names the type → a signature or an assignment → the
   class a template path names → the receiver's own spelling → the name-based list. The order is
   the safety argument (`types.md`).
3. **Rails knowledge lives only in `workspace/rails/`, and its output is text.** Every reader
   ends at `generated::Facts`, which renders RBS that is indexed like any other signature. RSpec's
   words live the same way in `workspace/rspec.rs`, FactoryBot's in `workspace/factories.rs`,
   i18n's in `workspace/i18n.rs`. The one
   exception is the view conventions (which class renders a template, what a render call hands a
   partial, and what a controller surely runs before an action), which `analysis/views.rs` and
   `types` ask of that directory's pure functions.
   Nothing outside it
   spells a Rails word; what both sides of the RBS must agree on sits in `generated.rs`
   (`synthesized.md`).
4. **A generated declaration can never become a `Location`.** It lives under the
   `ya-lsp-generated:` scheme, and `DocUri` refuses that scheme. Its source place comes only from
   `analysis/synthesized.rs`: no mapping means no place, never a guess.
   `workspace/textDocumentContent` serves the text, and the URI travels only as a command
   argument (`synthesized.md`).
5. **A keystroke neither indexes nor regenerates.** `didChange` records the edit. Requests answer
   against the last settled graph, and `position::Rebase` translates every offset that becomes a
   graph key, or refuses when it cannot (`completion.md`, `navigation.md`).
6. **A document outside the project reads the project, but the project never sees it.**
   `environment::Layout::is_outside` is a prefix test, and its verdict is *drop* on every surface.
   `untitled:` buffers follow the same rule, and reach only the project and themselves
   (`environment.md`).
7. **rubydex panics are contained at three seams:** indexing (per file), request handling (per
   request) and resolution (`concurrency.md`).

Area-specific facts (types, ERB, watchers, renames, completion ranking) live in their rule files,
not here.

## Layout

```
src/
  main.rs, lib.rs    CLI; the lib target lets tests drive the server in-process
  generated.rs       the fact table every generator ends at, and the RBS it renders
  knowledge/         bodies of knowledge (rails, annotations, structs, rspec, factories, singletons,
                     defines, mixins, i18n) and their registry; core names no body of knowledge
  logging.rs         log sinks and filters
  licenses.rs        what `--licenses` prints
  messages.rs        every sentence a user reads, one `pub fn` each
  testing.rs         test-only tracing capture
  server/            lifecycle, capabilities, dispatch, and the fallback file watcher
  analysis/          the analysis thread; owns the rubydex Graph
    requests.rs      one handler per LSP method, the bulkhead, settle and retry policy
    locator.rs       cursor → target → declaration → location
    cursor.rs        what shape the cursor is in
    types.rs         RBS return types, overloads, receiver rungs, the name guess
    synthesize.rs    the generator pass (names no body of knowledge)
    synthesized.rs   where a generated declaration really lives, and its text
    environment.rs   test/migration/outside-project fences, per surface
    indexed.rs       the graph plus the member and document indexes; the single write door
    indexer.rs       the indexing pool, one failure unit per file
    position.rs      LSP positions ↔ byte offsets, edits, rebasing
    testing.rs       test-only `Harness` that drives the server end to end
    coverage.rs      `ya-lsp coverage`: the share of a project's calls it types, sampled
    …                one file per feature: completion, hover, hints, hierarchy, references,
                     rename, code_actions, highlight, scopes, ranges, symbols, search, tokens,
                     signature_help, requires, render, erb, views, structs, annotations,
                     diagnostics, signatures, progress, threaded_tests
  workspace/         root discovery, config, gems, bundler, Ruby version, RBS, URIs
    rspec.rs         every RSpec word in the crate: example groups, lets, configure, shared
    factories.rs     FactoryBot's words: which class each factory builds
    singletons.rs    Ruby's `Singleton`: the classes that answer `instance`
    defines.rs       Ruby's `define_method`: the methods a class body makes by a literal name
    mixins.rs        Ruby's `include`/`prepend` called on a class from outside its body
    i18n.rs          i18n's words and the locale files' reader: what each key holds, and where
    rails/           every Rails word in the crate: schema, structure.sql, models,
                     associations, relations, concerns, enums, attributes, delegates,
                     the tail of 17 macro families, routes, entrypoints, framework,
                     migrations, layouts, conventions, renders (partials and their locals),
                     callbacks (a controller's before_action), current attributes,
                     connection adapters, blocks, inflector
scripts/             coverage.sh, canary.py, corpora.py + corpora.toml, audit/
audit/               committed audit ledger and baseline (integers, paths, hashes only)
vendor/rbs/          Ruby's RBS signatures, embedded by build.rs
tests/               lifecycle.rs, vscode_manifest.rs, claude_plugin.rs
editors/vscode/      the VS Code extension (TypeScript)
editors/claude-code/ the Claude Code plugin (three JSON files and a setup skill)
.claude-plugin/      the repository's own plugin marketplace
tmp/                 scratch space and corpora; gitignored
```

## Settled choices

- **`lsp-server` 0.10** is the transport. There is no async runtime.
- **`rubydex` is pinned by git revision**, never by branch.
- **`ruby-prism` is pinned at `=1.9.0`** to match rubydex exactly, or `ruby-prism-sys` links twice.
- **`notify` 8.2** watches files. Its licence is CC0-1.0, which is argued in `about.toml`.
- The dev-dependencies are `tempfile`, `url` and `proptest`.

## Rules

Each file in `.claude/rules/` loads when you touch a matching path. `mood.md` and `standards.md`
always load.

| Rule | Loads when you touch |
|---|---|
| `core-invariants.md` | `src/**`, `tests/**`, `build.rs` |
| `navigation.md` | `analysis/` locator, cursor, symbols, hover, render, requires, search |
| `completion.md` | `analysis/completion.rs`, `cursor.rs`, `signature_help.rs` |
| `code-actions.md` | `analysis/code_actions.rs` |
| `types.md` | `analysis/types.rs`, `cursor.rs`, `hover.rs`, `workspace/rails/`, `workspace/rspec.rs`, `workspace/i18n.rs` |
| `synthesized.md` | `analysis/synthesized.rs`, `synthesize.rs`, `locator.rs`, `annotations.rs`, `structs.rs`, `workspace/rails/`, `workspace/rspec.rs`, `workspace/factories.rs`, `workspace/singletons.rs`, `workspace/defines.rs`, `workspace/mixins.rs`, `workspace/i18n.rs`, `generated.rs`, `knowledge/` |
| `erb.md` | `analysis/erb.rs` |
| `views.md` | `analysis/views.rs`, `workspace/rails/conventions.rs` |
| `tokens.md` | `analysis/tokens.rs`, `server/capabilities.rs` |
| `hierarchy.md` | `analysis/hierarchy.rs`, `requests.rs` |
| `environment.md` | `analysis/environment.rs` and every surface that fences |
| `hints.md` | `analysis/hints.rs` |
| `features.md` | `workspace/features.rs`, `workspace/config.rs`, `analysis/synthesize.rs`, `types.rs`, `views.rs`, `workspace/rails/`, `workspace/rspec.rs`, `workspace/factories.rs`, `workspace/i18n.rs`, `knowledge/` |
| `logging.md` | `src/logging.rs`, `main.rs`, `analysis/requests.rs`, `analysis/mod.rs`, `workspace/config.rs` |
| `concurrency.md` | `analysis/threaded_tests.rs`, `mod.rs`, `indexer.rs`, `server/watcher.rs`, `server/mod.rs` |
| `highlighting.md` | `analysis/highlight.rs`, `scopes.rs` |
| `renaming.md` | `analysis/rename.rs` |
| `ranges.md` | `analysis/ranges.rs` |
| `search-references.md` | `analysis/search.rs`, `references.rs` |
| `gems.md` | `workspace/` gems, bundler, ruby_version, config, mod |
| `rbs-signatures.md` | `workspace/rbs.rs`, `analysis/signatures.rs`, `vendor/rbs/`, `build.rs` |
| `messages.md` | `src/messages.rs` and the five files that raise its messages |
| `coverage.md` | `Makefile`, `scripts/`, `.github/workflows/` |
| `canary.md` | `scripts/canary.py`, `Makefile`, `.github/workflows/` |
| `corpora.md` | `scripts/corpora.py`, `scripts/corpora.toml`, `scripts/canary.py`, `Makefile` |
| `audit.md` | `scripts/audit/**`, `audit/**`, `Makefile` |
| `vscode-extension.md` | `editors/vscode/**` |
| `claude-code-plugin.md` | `editors/claude-code/**`, `.claude-plugin/**`, `tests/claude_plugin.rs` |
| `type-coverage.md` | `analysis/coverage.rs`, `main.rs` |
| `licensing.md` | `LICENSE.txt`, `NOTICE.txt`, `THIRD-PARTY-NOTICES.txt`, `about.*`, `src/licenses.rs`, `vendor/rbs/`, `CHANGELOG.md` |

## Commands

```bash
make setup             # nightly + llvm-tools, cargo-llvm-cov, cargo-about (pinned versions)
make build / release   # debug / optimized build
make test              # the Rust suite
make test-one T=x      # one test, with stdout
make check / lint      # type-check / clippy with warnings denied
make fmt / fmt-check   # format / fail if unformatted
make ci                # fmt-check, lint, test, notices-check, coverage

make coverage          # instrumented suite, then all three bars
make coverage-clean    # RUN THIS FIRST whenever a coverage number surprises you
make coverage-branches # untaken branch arms (F=gems to filter)
make coverage-missing  # uncovered lines
make coverage-html     # browsable report

make canary            # open a pinned real Rails app, assert exact counts (needs network)
make corpora           # set up the six reference corpora (needs network; ARGS=--only <name>)
make corpora-status    # each corpus against its pin
make audit             # score sampled answers over all six corpora; a measurement, not a gate
make audit-baseline    # record the last sweep as the committed baseline

make notices           # regenerate THIRD-PARTY-NOTICES.txt after a dependency change
make notices-check     # fail if it is stale
make ext-install / ext-test / ext-lint   # the VS Code extension
```

Three facts about these targets:

1. **Coverage gates** are 95% of lines and branches project-wide, 90% of lines per file, and 100%
   in 53 named modules. `coverage.md` has the details.
2. **No corpus source text is ever committed.** `make canary`, `make corpora` and `make audit`
   read corpora under `tmp/corpora/`, which is gitignored (`corpora.md`).
3. **The extension's `contract.test.ts` skips itself when no built `ya-lsp` exists**, so build the
   server before trusting a green run.

## Toolchain

- **Rust 1.93.1**, pinned in `.tool-versions`.
- **Edition 2024**: `unsafe_op_in_unsafe_fn` applies, `gen` is a keyword, and the new
  `impl Trait` capture rules apply. Do not write edition-2021 idioms.
