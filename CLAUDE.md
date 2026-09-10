# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project

`ya-lsp` — a Language Server Protocol implementation for **Ruby**, written in Rust. Lightweight,
fast, standalone: no Ruby runtime dependency anywhere.

## Current state

18 LSP request methods are answered over a rubydex graph the server builds itself, plus
`publishDiagnostics`. The per-area rules in `.claude/rules/` hold the invariants and the
measurements; this section is only what the server does.

**Requests.** `definition`, `hover`, `documentSymbol`, `workspace/symbol`, `references`,
`prepareTypeHierarchy` + `supertypes` + `subtypes`, `documentHighlight`, `prepareRename` +
`rename`, `signatureHelp`, `foldingRange`, `selectionRange`, `semanticTokens/full`, `codeAction`,
`completion` + `completionItem/resolve`. The four refactorings are Prism rewrites; RuboCop serves
autocorrect over its own `codeAction`, so ya-lsp composes with it rather than competing.

**Indexed:** the workspace's Ruby and ERB, every gem's `lib/`, a Rails engine's `app/`, each gem's
`sig/`, `.gem_rbs_collection/`, the project's `sig/`, and Ruby's core and stdlib signatures — from
the installed `rbs` gem, or from the copy vendored into the binary. **No cache**: a cold open of a
real Rails application with a full bundle lands well inside a second, so there is nothing to
invalidate.

**Three tiers of answer, and every card says which.**

- *Resolved* — the code names the type.
- *Derived* — a signature, an assignment or a convention; a footnote names which.
- *Guessed* — matched on the method name alone. `[types] guess_from_names` turns it off.

A user who cannot tell them apart has lost the property that makes this server different.

**Five rungs under a receiver, in this order** (the ordering is the safety argument; `types.md`
has the rules and the ceiling): rubydex naming it → a signature or an assignment → the controller
a template's path names → the receiver's own spelling → the name-based list.

**Types come from RBS.** rubydex models no types, so `analysis/types.rs` is a table beside the
graph: a method's return by how the call was written (which arm the block picks, how many
positional arguments), and what a block is handed. Two receiver-relative returns let
ActiveRecord's query interface be written once per project rather than once per model.

**Rails knowledge is one directory and its only output is text.** `workspace/rails/` reads
`db/*schema.rb`, `db/*structure.sql`, the association macros, `enum`, `attribute`, `delegate`, 17
further macro families, `config/routes.rb`, mailers, jobs, Sidekiq workers and concern bodies.
Every reader ends at `generated::Facts`, which renders RBS that `indexing::index_source` and
`Types::harvest` consume like any other signature — so nothing outside that directory learns a
Rails word. `analysis/structs.rs` reads `Struct.new` and `Data.define` (plain Ruby);
`analysis/annotations.rs` reads a Sorbet `sig` or a YARD `@return`.

**A generated declaration needs a source, or it has nowhere to jump to.**
`analysis/synthesized.rs` is the side table, consulted at exactly one place — `locator::site`, the
single point a `Definition` becomes a `Site`. **No mapping means no place, never a guess.** The
generated URI is deliberately not a `file:` URI, so a generated document cannot become a
`Location` even with an empty table.

**ERB templates are indexed like any other file.** `analysis/erb.rs` replaces markup with spaces,
one per byte, newlines kept — so rubydex records the template's own offsets and 15 of the 18
requests need nothing extra. A template is *read* as that view and *addressed* as the text the
client has. `foldingRange` is declined and `completion` is gated. `analysis/views.rs` is what a
bare word in a template can call: `app/helpers` and the `helper_method` proxies of the controller
the path implies.

**A keystroke does not index and does not regenerate.** `didChange` records the edit and indexes
nothing; `completion`, `hover` and `definition` answer against the last settled graph, with
`position::Rebase` translating the cursor and **refusing** any offset it cannot translate — so a
deferred answer is never less than an eager one. The generator pass is gated twice (would the walk
build the same projection; has any file a generator reads changed) and memoises every parse.

**Three seams contain a rubydex panic**: indexing per *file* (not per worker, which would lose a
whole stolen batch), request handling per request, and resolution. A contained index panic leaves
a known state, so recovery is a self-clearing skip list rather than a rebuild.

**Coverage is gated three ways**, all enforced by CI: 95% of lines and branches project-wide, 90%
of lines in every file, 100% in 31 named modules. `make canary` opens a pinned commit of a real
Rails application and asserts exact counts.

## Layout

```
src/
  main.rs          CLI (--stdio, --licenses, -V, -h), stderr-only tracing init
  lib.rs           module root (a lib target exists so tests can drive the server in-process)
  generated.rs     the fact table every generator ends at: declarations, overloads, collision
                   rules within and across documents, which names are writable, which
                   namespaces may be joined onto or opened as a body, what a body includes and
                   inherits, and the RBS and spans it renders
  licenses.rs      what `--licenses` prints: LICENSE.txt + NOTICE.txt + notices
  messages.rs      every sentence a user reads, one `pub fn` each
  server/          LSP lifecycle, capability negotiation, dispatch loop
    capabilities.rs
  analysis/        the analysis thread; owns the rubydex Graph
    completion.rs  what the graph offers at the cursor: ranking, cap, resolve, members of an
                   edge no file writes, and the coordinate change a suggestion list needs
    cursor.rs      what the cursor is shaped like; which of a variable's assignments may speak
                   for it; the boundary between the buffer read here and the graph keyed by it
    diagnostics.rs rule -> severity table, rubydex Diagnostic -> LSP
    erb.rs         the Ruby view of a template, and where in one the Ruby is
    indexer.rs     the only way into rubydex's indexer: a pool whose failure unit is one file,
                   not one worker
    locator.rs     cursor -> target -> declaration -> location; the Rails edge nobody's file
                   writes; which candidates a call on a class object could have meant
    symbols.rs     documentSymbol, nested and flat
    search.rs      workspace/symbol: rubydex filters, we rank and cap
    references.rs  textDocument/references: exact constants, name-based methods
    hierarchy.rs   prepareTypeHierarchy: ancestors, and rubydex's reverse index
    highlight.rs   documentHighlight: scope walk first, then the graph, one file
    scopes.rs      which variable is which, and what a byte range borrows from around it
    code_actions.rs the four refactorings, what each refuses, and the re-parse that decides
    signature_help.rs which overload the call fits, and which parameter it is on
    rename.rs      the first module that writes; what it will not rename, and why
    ranges.rs      what folds, and what expanding the selection reaches
    hover.rs       hover markdown
    requires.rs    `require "..."` under the cursor
    render.rs      how Ruby constructs are spelled for humans; RDoc read as RDoc
    progress.rs    $/progress streams (gem indexing)
    signatures.rs  blanks RBS `interface` blocks before a file is indexed
    types.rs       what RBS says a method returns (by how the call was written) and what it
                   hands a block; the two returns naming a receiver's model rather than a
                   class; the view->controller rung, the name guess, and the fall-through order
    views.rs       what a template can call: the two halves of a view context, which a mailer
                   gets, and the rung whose answer is a member rather than a type
    synthesized.rs where a generated declaration was really declared, and what is not a place
    synthesize.rs  the pass: what one document contributes and where those merge, which
                   generator asks for which documents, which class is a model, the membership
                   decidable only after the walk, which classes a concern's macros land on, the
                   one graph-wide question and why it reads definitions not declarations, the
                   second phase, the render that computes each span once, and the two gates
    structs.rs     `Struct.new` and `Data.define`: which spelling names a class, what each
                   installs, the `def` in the block that keeps its member, the unspellable
                   namespace
    annotations.rs a Sorbet `sig` and a YARD `@return`, and the one spelling of "anything" that
                   is a class
    tokens.rs      semanticTokens: the identifiers a TextMate grammar cannot classify
    position.rs    LSP position <-> byte offset, UTF-8/16/32, incremental edits, the document
                   addressed in text it does not hold, and how buffer offsets map onto indexed
                   ones
    threaded_tests.rs the run loop: real thread, real channel, order
  workspace/       workspace root, discovery
    config.rs      ya-lsp.toml; the default include, which is every shape Ruby is written in
    rails/         every Rails word in the crate; its only output is text
      mod.rs       the convention tables, and the only public surface
      conventions.rs which controller renders a template, which class a mailer's views hang
                   off, which files Rails puts in every view context, which files are schemas
      inflect.rs   Rails' inflector, minus everything this corpus did not need
      syntax.rs    the half-dozen Prism shapes every reader starts from
      schema.rs    `db/*schema.rb`: tables and column types
      structure.rs `db/*structure.sql`: the same tables from a database's own dump; the six
                   things that hide text from a scanner; the one word three dialects differ on
      models.rs    association macros and which bodies may host one; which blocks are still the
                   class body; every member one association line installs and the four
                   declined; the relation class every model gets; the query interface as Rails'
                   own list, written once per project and once per base; the four names whose
                   answer the call decides; callbacks with no `def`; the macro a module reads
                   and every including class owns; the two that hand a name to the view context
      enums.rs     `enum`, both spellings, and the 3 + 4N names one call installs
      attributes.rs `attribute`: the cast type that is the only evidence the call is Rails'
                   rather than a serializer gem's, and the host that is the only evidence left
      tail.rs      the 17 remaining macros as one table: where each takes its names from, what
                   it installs, the four that install nothing, and the `def` that wins
      delegates.rs `delegate`: the name written down, the two hops to a type, and why what is
                   declined is the type and never the member
      routes.rs    `config/routes.rb`: which helpers Rails names, the module they go in, and
                   every class that `include`s it
      entrypoints.rs mailers, jobs and Sidekiq workers: what recognises each, the class methods
                   each installs, and what a mailer action hands back
    bundler.rs     Gemfile.lock -> sources and specs (pure text, no I/O)
    ruby_version.rs which Ruby, without running ruby
    gems.rs        gem roots, spec -> directory, require_paths, each gem's `sig/`, an engine's
                   `app/`, the curated collection, Ruby's own lib, file walk
    rbs.rs         which copy of Ruby's signatures to index; the vendored fallback
    uri.rs         canonical document URIs
Makefile           every command, and the two cargo subcommands' pinned versions
scripts/
  coverage.sh      the coverage gate: both bars, and every untaken branch arm
  canary.py        opens a real Rails app the way an editor does; counts, then a ceiling
build.rs           embeds vendor/rbs into the binary
vendor/rbs/        Ruby's own RBS signatures, vendored (BSD-2-Clause/Ruby, see its README)
tests/lifecycle.rs end-to-end against a spawned binary
tests/vscode_manifest.rs the extension's settings, against the server they configure
editors/vscode/    the VS Code extension (TypeScript)
  src/extension.ts activation, one client per workspace folder, commands
  src/config.ts    VS Code settings -> the server's wire format; no `vscode` import
  src/server.ts    which binary to run; no `vscode` import
  src/manifest.test.ts the manifest, against the settings `config.ts` reads
.github/workflows/ CI, and the per-platform VSIX matrix
tmp/               temporary files; gitignored
```

## Settled choices

- **`lsp-server` 0.10** (sync, from rust-analyzer) as the transport.
- **`rubydex` pinned by git revision** — not by branch, because a `cargo update` would take
  upstream changes silently.
- **`ruby-prism` `=1.9.0`** — must match rubydex's exactly, or `ruby-prism-sys` links twice.
- No async runtime.
- Dev-dependencies are `tempfile`, `url` and **`proptest`** (which holds `analysis/position.rs`'s
  three properties). `about.toml` ignores dev-dependencies, so none owes a notice.

## Rules

Every invariant lives in `.claude/rules/*.md`. Each carries a `paths:` frontmatter list, so it
loads only while you are working on the files it governs. `mood.md` and `standards.md` have no
`paths:` and are always in context.

| Rule | Loads when you touch |
|---|---|
| `core-invariants.md` | `src/**`, `tests/**`, `build.rs` |
| `navigation.md` | `analysis/` locator, symbols, hover, render, requires, search |
| `completion.md` | `analysis/completion.rs`, `cursor.rs`, `signature_help.rs` |
| `code-actions.md` | `analysis/code_actions.rs` |
| `types.md` | `analysis/types.rs`, `cursor.rs`, `hover.rs`, `workspace/rails/` |
| `synthesized.md` | `analysis/synthesized.rs`, `synthesize.rs`, `locator.rs`, `annotations.rs`, `structs.rs`, `workspace/rails/`, `generated.rs` |
| `erb.md` | `analysis/erb.rs` |
| `views.md` | `analysis/views.rs`, `workspace/rails/conventions.rs` |
| `tokens.md` | `analysis/tokens.rs`, `server/capabilities.rs` |
| `hierarchy.md` | `analysis/hierarchy.rs` |
| `concurrency.md` | `analysis/threaded_tests.rs`, `mod.rs`, `indexer.rs` |
| `highlighting.md` | `analysis/highlight.rs`, `scopes.rs` |
| `renaming.md` | `analysis/rename.rs` |
| `ranges.md` | `analysis/ranges.rs` |
| `search-references.md` | `analysis/search.rs`, `references.rs` |
| `gems.md` | `workspace/` gems, bundler, ruby_version, config, mod |
| `rbs-signatures.md` | `workspace/rbs.rs`, `analysis/signatures.rs`, `vendor/rbs/`, `build.rs` |
| `messages.md` | `src/messages.rs` and the five files that raise its messages |
| `coverage.md` | `Makefile`, `scripts/`, `.github/workflows/` |
| `canary.md` | `scripts/canary.py`, `Makefile`, `.github/workflows/` |
| `benchmarking.md` | `tests/`, `src/main.rs`, `src/server/` |
| `vscode-extension.md` | `editors/vscode/**` |
| `licensing.md` | `LICENSE.txt`, `NOTICE.txt`, `THIRD-PARTY-NOTICES.txt`, `about.*`, `src/licenses.rs`, `vendor/rbs/`, `CHANGELOG.md` |

Rules fire on `path_glob_match` — when a matching file is actually read or edited. If you are
about to reason about a module without opening it, or to create a new file under a governed path,
read its rule first. `.claude/rules/` is small enough to `cat` in full.

## Commands

**Everything runs through the `Makefile`; `make` on its own lists the targets.** It is the single
place the commands and the two pinned tool versions live. `cargo-llvm-cov` and `cargo-about` are
cargo *subcommands*, which cargo cannot express as dev-dependencies (it never builds a
dependency's binaries), so `make setup` installs them and the Makefile pins their versions. CI
runs the same targets, so a local run and a CI run cannot disagree.

```bash
make                   # list every target
make setup             # nightly + llvm-tools, cargo-llvm-cov, cargo-about
make build             # debug build           make release       # optimized build
make test              # the Rust suite        make test-one T=x  # one test, with stdout
make check             # type-check only       make lint          # clippy, warnings denied
make fmt               # format                make fmt-check     # fail if unformatted
make ci                # fmt-check, lint, test, notices-check, coverage

make coverage          # instrumented suite, then all three bars
make coverage-branches # every branch arm no test took (F=gems to filter)
make coverage-missing  # every line no test ran
make coverage-html     # the browsable report
make coverage-clean    # drop the profiles and the nightly objects

make canary            # open a real Rails app (lobsters, pinned) and check the answers
make canary-clone      # just fetch the pinned commit

make notices           # regenerate THIRD-PARTY-NOTICES.txt after any dependency change
make notices-check     # fail if the committed copy is stale (CI and tag builds do this)
make ext-install       # the extension's dependencies (yarn 1's `npm ci`)
make ext-test          # compile, bundle and run the extension's tests
make ext-lint          # type-check the extension
```

`make coverage` splits into `coverage-run` (the instrumented suite, once) and `coverage-check` (the
gate), so every other coverage target reads that same run. `coverage-run` stamps the toolchain that
produced `target/llvm-cov-target` and wipes the directory when it changes.

`make canary` is **the one target that needs a network**, which is why it is not in `make ci`. It
fetches one pinned commit of a real Rails application into `tmp/` and opens it the way an editor
does, asserting the file count, the diagnostic counts and *no other code at all* exactly — the
numbers live in the `Makefile` — and time only against a ceiling well above the measurement. The
counts are properties of the commit, the timing a property of the runner. It does not cover gems. CI runs it as its own job; see
`canary.md`.

```bash
# The extension, when no target covers it (from editors/vscode)
yarn bundle                      # dist/extension.js, what actually ships
yarn vsce package --target darwin-arm64   # a VSIX; needs server/ya-lsp staged first
```

The extension's `contract.test.ts` talks to a real `ya-lsp`: it looks for
`editors/vscode/server/ya-lsp`, then `target/release`, then `target/debug`, and **skips itself** if
it finds none — so build the server before trusting a green run. Press F5 in the repo root to
launch an extension host with everything built; `.vscode/tasks.json` does the staging.

What the targets run, for when one needs a flag it does not carry:

```bash
cargo run -- --stdio         # the server, as an editor spawns it
cargo test <name>            # tests whose path/name contains <name>
cargo test <name> -- --exact --nocapture   # one exact test, with stdout
```

Driving the binary against a real Ruby repository by hand is `.claude/rules/benchmarking.md`.

## Toolchain

Rust **1.93.1**, pinned in `.tool-versions` (asdf/mise). Crate uses **edition 2024** —
`unsafe_op_in_unsafe_fn`, `gen` as a keyword, and the new `impl Trait` capture rules apply. Do not
write edition-2021 idioms that the 2024 lints reject.
