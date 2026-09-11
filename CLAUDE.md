# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project

`ya-lsp` — a Language Server Protocol implementation for **Ruby**, written in Rust. Lightweight,
fast, standalone: no Ruby runtime dependency anywhere.

## Current state

24 LSP request methods are answered over a rubydex graph the server builds itself, plus
`publishDiagnostics`. The per-area rules in `.claude/rules/` hold the invariants and the
measurements; this section is only what the server does.

**Requests.** `definition`, `hover`, `documentSymbol`, `workspace/symbol`, `references`,
`prepareTypeHierarchy` + `supertypes` + `subtypes`, `prepareCallHierarchy` + `incomingCalls` +
`outgoingCalls`, `documentHighlight`, `prepareRename` + `rename`, `signatureHelp`, `foldingRange`,
`selectionRange`, `semanticTokens/full`, `codeAction`, `completion` + `completionItem/resolve`,
`documentLink`, `inlayHint` + `inlayHint/resolve`. The four refactorings are Prism rewrites; RuboCop serves autocorrect over its own
`codeAction`, so ya-lsp composes with it rather than competing.

**Indexed:** the workspace's Ruby and ERB, every gem's `lib/`, a Rails engine's `app/`, each gem's
`sig/`, `.gem_rbs_collection/`, the project's `sig/`, and Ruby's core and stdlib signatures — from
the installed `rbs` gem, or from the copy vendored into the binary. **No cache**: a cold open of a
real Rails application with a full bundle lands well inside a second, so there is nothing to
invalidate.

**Three tiers of answer, and every card says which.**

- *Resolved* — the code names the type.
- *Derived* — a signature, an assignment or a convention; a footnote names which.
- *Guessed* — matched on the method name alone. `[types] guess_from_names` turns it off.

A user who cannot tell them apart has lost the property that makes this server different. An
inlay hint is the one answer nobody asked for, so the tier decides what may be **drawn** there
rather than only how it is labelled: a guess is never painted into a margin, and the tier that
needs no footnote turns out to be unreachable — `hints.md`.

**Five rungs under a receiver, in this order** (the ordering is the safety argument; `types.md`
has the rules and the ceiling): rubydex naming it → a signature or an assignment → the class a
template's path names, a controller or a mailer → the receiver's own spelling → the name-based
list.

**Types come from RBS.** rubydex models no types, so `analysis/types.rs` is a table beside the
graph: a method's return by how the call was written (which arm the block picks, how many
positional arguments), and what a block is handed. Two receiver-relative returns let
ActiveRecord's query interface be written once per project rather than once per model.

**Rails knowledge is one directory and its only output is text.** `workspace/rails/` reads
`db/*schema.rb`, `db/*structure.sql`, the association macros, `enum`, `attribute`, `delegate`, 17
further macro families, `config/routes.rb`, mailers, jobs, Sidekiq workers, concern bodies and what the framework's
own singletons return.
Every reader ends at `generated::Facts`, which renders RBS that `indexing::index_source` and
`Types::harvest` consume like any other signature — so nothing outside that directory learns a
Rails word, and since 2026-09-15 there is no exception: the concern edge `locator` used to walk
behind a `const CLASS_METHODS` is a declaration now. `analysis/structs.rs` reads `Struct.new` and
`Data.define` (plain Ruby); `analysis/annotations.rs` reads a Sorbet `sig` or a YARD `@return`.

**A generated declaration needs a source, or it has nowhere to jump to.**
`analysis/synthesized.rs` is the side table, consulted at exactly one place — `locator::site`, the
single point a `Definition` becomes a `Site`. **No mapping means no place, never a guess.** The
generated URI is deliberately not a `file:` URI, so a generated document cannot become a
`Location` even with an empty table. A source is usually the line the generator read — a column's
`t.string "title"` — and for ActiveRecord's query interface it is a `def` in the bundle, because
no file in the project declares `Story.where` and one in activerecord does. That place is
**looked up by name in Ruby's own ancestry, after the resolve**, since a generator cannot ask a
graph it is still writing; found or not found, and never matched.

**ERB templates are indexed like any other file.** `analysis/erb.rs` replaces markup with spaces,
one per byte, newlines kept — so rubydex records the template's own offsets and 21 of the 24
requests need nothing extra. A template is *read* as that view and *addressed* as the text the
client has. `foldingRange` is declined and `completion` is gated. `analysis/views.rs` is what a
bare word in a template can call: `app/helpers`, the `helper_method` proxies of the controller
the path implies, and the helper modules ActionView itself ships.

**A client is told where the answers are.** A gem's source, Ruby's stdlib and the RBS beside them
live outside every workspace folder, and a document selector is the only gate on what a client ever
sends — so after the handshake the server registers its own roots for every request it answers,
derived from the capabilities it advertised, over the channel the file watcher uses. The editor's
side claims the folder and only the folder, and never guesses where a bundle is; the extension's one
job here is deciding which of several servers takes each root, since two folders on one Ruby resolve
to the same ones. A client that declines dynamic registration keeps what it had and is told once.

**A keystroke does not index and does not regenerate.** `didChange` records the edit and indexes
nothing; `completion`, `hover` and `definition` answer against the last settled graph, with
`position::Rebase` translating **every offset that becomes a graph key** — the cursor, and the
`cursor::Receiver` an assignment or a chain produced, in whichever document it was read from — and
**refusing** any it cannot translate, so a deferred answer is never less than an eager one. The generator pass is gated twice (would the walk
build the same projection; has any file a generator reads changed) and memoises every parse.

**Three seams contain a rubydex panic**: indexing per *file* (not per worker, which would lose a
whole stolen batch), request handling per request, and resolution. A contained index panic leaves
a known state, so recovery is a self-clearing skip list rather than a rebuild.

**Coverage is gated three ways**, all enforced by CI: 95% of lines and branches project-wide, 90%
of lines in every file, 100% in 39 named modules. `make canary` opens a pinned commit of a real
Rails application and asserts exact counts.

## Layout

```
src/
  main.rs          CLI (--stdio, --licenses, -V, -h), stderr-only tracing init
  lib.rs           module root (a lib target exists so tests can drive the server in-process)
  generated.rs     the fact table every generator ends at: declarations, overloads, collision
                   rules within and across documents, which names are writable, which
                   namespaces may be joined onto or opened as a body, what a body includes and
                   inherits, and the RBS it renders — with the spans for the members a file
                   declared, the names for the one family no file does, and the one *body*
                   that is a place rather than only a type
  knowledge/       what a body of knowledge is, and the only place core names one: the list
                   ids a module registers, the predicate vocabulary its rows are written in,
                   which switch gates which list, what one document contributes to a module's
                   own projection and what merges them, the three phases a generator declares
                   in and the two hooks that run where only the whole walk can decide, the
                   parse memo each module keeps, and the registry the pass is handed instead
                   of a module — a build with nothing registered compiles, runs and declares
                   nothing, and a test says so
    rails.rs       Rails' seven lists, their rows, the projection it folds out of the walk and
                   the eight generators that read it — the orchestration, never the reading,
                   which stays pure in `workspace/rails/`
    annotations.rs a Sorbet `sig` and a YARD tag: one row, one memo of facts, one generator
    structs.rs     `Struct.new` and `Data.define`: one row and one generator, no memo
    rspec.rs       test-only, and the proof: a second body of knowledge that touches no file
                   outside its own
  logging.rs       where the log goes and what may be written there: the filter a level means,
                   the second sink and the one handle that re-points both, and why the layers
                   are registered once and never replaced
  licenses.rs      what `--licenses` prints: LICENSE.txt + NOTICE.txt + notices
  messages.rs      every sentence a user reads, one `pub fn` each
  testing.rs       test-only, crate-wide: the tracing capture, which is what asserts that the
                   log a user reads when nothing works says the right thing
  server/          LSP lifecycle, capability negotiation, dispatch loop
    capabilities.rs
  analysis/        the analysis thread; owns the rubydex Graph
    requests.rs    the LSP request layer: one handler per method, the table that picks it,
                   and the bulkhead, the settle policy and the deferred retry, which are
                   properties of answering a request rather than of any one method
    completion.rs  what the graph offers at the cursor: ranking, cap, resolve, members of an
                   edge no file writes, the two kinds of tree a cursor outside them is not
                   offered a name from, and the coordinate change a suggestion list needs
    cursor.rs      what the cursor is shaped like; which of a variable's assignments may speak
                   for it; what counts as a macro without knowing one macro's name; the
                   boundary between the buffer read here and the graph keyed by it
    diagnostics.rs rule -> severity table, rubydex Diagnostic -> LSP
    environment.rs whether the application can load a document at all, and the table of what
                   each surface may do about it: which drop a name only the suite loads, which
                   merely rank it down, and which must never ask; the one list that is read of
                   a cursor rather than of a target, and why it is wider; the two halves a
                   surface fences with — the cursor gate and the layout that tells a gem's
                   library from a suite — why they travel as one value, and the one rung that
                   reads the directory names and nothing else; and the two trees in neither
                   environment — the one a generator copies out of and the one a rake task runs
                   a single file of — whose tags need no layout and are read of the whole graph
                   rather than of the project's own documents
    erb.rs         the Ruby view of a template, and where in one the Ruby is
    indexer.rs     the only way into rubydex's indexer: a pool whose failure unit is one file,
                   not one worker
    locator.rs     cursor -> target -> declaration -> location; the one mixin rubydex does not
                   linearize, repaired by name and holding no convention of anybody's; which
                   candidates a call on a class object could have meant; the two
                   things the graph does not model — the instance variable and the macro's
                   symbol — and the opposite orders their answers are asked in; which half of a
                   variable's answer crosses into the graph's coordinates and which may not; the
                   three rungs a test tree is fenced on — the guess, the one precise answer
                   that is a member of everything, and the list of places itself — the two
                   tiers a wide namespace's places are ordered by and how little the second
                   one promises; which of a name's several places a single row points at; and
                   why one `def` several declarations name is still one place
    symbols.rs     documentSymbol, nested and flat
    search.rs      workspace/symbol: rubydex filters, we rank and cap — the user's own code,
                   then how well the query matched, then whether the application loads it —
                   and the generated row a declaration in the file itself already covers
    references.rs  textDocument/references: exact constants, name-based methods, and the
                   same by a name that came from the buffer rather than from a target
    hierarchy.rs   prepareTypeHierarchy: ancestors, and rubydex's reverse index; the call
                   hierarchy beside it: which `def` a call site is written in, asked from both
                   directions, and the two different kinds of answer each may give
    hints.rs       inlayHint: the three families worth a label, the tier that may never be
                   drawn, the tier that cannot occur, and the range that bounds the work
    highlight.rs   documentHighlight: scope walk first, then the graph, then the macro's
                   symbol; one file
    scopes.rs      which variable is which, and what a byte range borrows from around it
    code_actions.rs the four refactorings, what each refuses, and the re-parse that decides
    signature_help.rs which overload the call fits, which parameter it is on, and the card that
                   is not drawn rather than drawn from a test tree
    rename.rs      the first module that writes; what it will not rename, and why
    ranges.rs      what folds, and what expanding the selection reaches
    hover.rs       hover markdown
    requires.rs    `require "..."`: the one under the cursor, and every one in the file
    render.rs      how Ruby constructs are spelled for humans; RDoc read as RDoc
    progress.rs    $/progress streams (gem indexing)
    signatures.rs  blanks RBS `interface` blocks before a file is indexed
    types.rs       what RBS says a method returns (by how the call was written) and what it
                   hands a block; the two returns naming a receiver's model rather than a
                   class; the coordinates a receiver has to arrive in; the view->renderer
                   rung, which carries the other document's map; the name guess, and the
                   fall-through order
    views.rs       what a template can call: the three halves of a view context — two an
                   application writes and one ActionView ships — which of them a mailer gets,
                   which a helper file is inside rather than reading, the rung whose answer is
                   a member rather than a type, and which class a path renders through, which
                   is the one question two modules ask and neither may answer twice
    synthesized.rs where a generated declaration was really declared, what is not a place, and
                   the one kind whose place is a name in a gem rather than a span in the file
                   the generator read
    synthesize.rs  the pass, and it names no body of knowledge at all: the one walk, what one
                   document contributes and where those merge, the one graph-wide question and
                   why it reads definitions not declarations, the render that computes each
                   span once and the split into one document per body, the two gates, and the
                   step that runs *after* the resolve because a generator cannot ask a graph it
                   is still writing where a gem declares a name. Which documents a generator
                   gets, what it declares from them and in what order is `knowledge/`
    structs.rs     `Struct.new` and `Data.define`: which spelling names a class, what each
                   installs, the `def` in the block that keeps its member, the unspellable
                   namespace
    annotations.rs a Sorbet `sig` and a YARD `@return`, and the one spelling of "anything" that
                   is a class
    tokens.rs      semanticTokens: the identifiers a TextMate grammar cannot classify
    position.rs    LSP position <-> byte offset, UTF-8/16/32, incremental edits, the document
                   addressed in text it does not hold, and how buffer offsets map onto indexed
                   ones
    testing.rs     test-only: the `Harness` every end-to-end test in the crate drives the
                   server through, and the fixtures more than one module's tests read. It is
                   inside `analysis/` because a `Harness` holds an `Analysis`, and it hands
                   the rest of the crate JSON, strings and counts rather than a graph
    threaded_tests.rs the run loop: real thread, real channel, order
  workspace/       workspace root, discovery
    config.rs      ya-lsp.toml; the default include, which is every shape Ruby is written in;
                   which tables a file actually moved; the switches a project turns a body of
                   knowledge off with, and the two tree lists it replaces rather than extends
    rails/         every Rails word in the crate; its only output is text
      mod.rs       the convention tables, and the only public surface
      conventions.rs which controller renders a template, which class a mailer's views hang
                   off, which files Rails puts in every view context, which files are schemas,
                   and which namespaces a directory conjures — the chain the path proposes and
                   the spelling the file confirms, which is where an acronym is read off the
                   answer rather than out of the inflector
      inflect.rs   Rails' inflector, minus everything this corpus did not need
      syntax.rs    the half-dozen Prism shapes every reader starts from
      schema.rs    `db/*schema.rb`: tables and column types
      structure.rs `db/*structure.sql`: the same tables from a database's own dump; the six
                   things that hide text from a scanner; the one word three dialects differ on
      models.rs    one model file: which bodies it holds and which blocks are still a class
                   body; which of them may declare at all; the order the families come back
                   together in, and the two macros that hand a name to the view context
      associations.rs `belongs_to`, `has_one`, `has_many`, `has_and_belongs_to_many` and
                   `scope`: every member one line installs and the four declined; the calls
                   that name no class and say so, against the lookup that merely came back
                   empty; the two keywords that name a class outright, and the neighbouring
                   one that only names a member; the macro a module reads and every including
                   class owns
      relations.rs the relation class every model gets and the two classes a scope goes on,
                   one of which waits until the document is done; the query interface as
                   Rails' own list, written once per project and once per base; the four names
                   whose answer the call decides; callbacks with no `def`
      concerns.rs  the class side of a concern, in its three spellings — a `class_methods do`
                   block, a hand-written `module ClassMethods`, and an `included do` that
                   extends a module declared somewhere else — written onto the singleton of
                   every class that includes it, because the edge that would carry it is one
                   nothing may declare
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
      framework.rs the framework's own singletons and the fixed class each returns, the four
                   declined and the measurement that declined them, which keyword opens each
                   owner's body, and the application's own class `Rails.application` is
    bundler.rs     Gemfile.lock -> sources and specs (pure text, no I/O), and whether one locks
                   a named gem
    features.rs    which bodies of knowledge apply to this project: what each switch gates, the
                   umbrella folded into all five of them, and the one detection `auto` runs —
                   which reads the lockfile itself rather than through the gem discovery a
                   different switch can turn off
    ruby_version.rs which Ruby, without running ruby
    gems.rs        gem roots, spec -> directory, require_paths, each gem's `sig/`, an engine's
                   `app/`, the curated collection, Ruby's own lib, file walk
    rbs.rs         which copy of Ruby's signatures to index; the vendored fallback
    uri.rs         canonical document URIs
Makefile           every command, and the two cargo subcommands' pinned versions
scripts/
  coverage.sh      the coverage gate: both bars, and every untaken branch arm
  canary.py        opens a real Rails app the way an editor does; counts, then a ceiling
  corpora.py       the six reference repositories: clone at pin, Ruby, gems, the two LSPs,
                   solargraph's composed bundle and its doc cache; `status` against the pin
  corpora.toml     the pin table: repository, commit, Ruby, licence, holder and role, one row
                   per corpus, and the only place a corpus' commit is written down
  audit/           is an answer right? a stratified sample of real cursors over six corpora,
                   scored in three lanes — keys that say *wrong* against the source, checks
                   that say *inconsistent* against the server's other answers, and the residue
                   a person rules on once
audit/             what the audit commits: the adjudicated ledger and the baseline the next run
                   is diffed against. Integers, paths and hashes; never a word of corpus source
build.rs           embeds vendor/rbs into the binary
vendor/rbs/        Ruby's own RBS signatures, vendored (BSD-2-Clause/Ruby, see its README)
tests/lifecycle.rs end-to-end against a spawned binary
tests/vscode_manifest.rs the extension's settings, against the server they configure
editors/vscode/    the VS Code extension (TypeScript)
  src/extension.ts activation, one client per workspace folder, commands
  src/config.ts    VS Code settings -> the server's wire format; no `vscode` import
  src/server.ts    which binary to run; no `vscode` import
  src/claims.ts    which of several servers answers about a root outside every folder, and what
                   a client stopping releases; no `vscode` import
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
| `navigation.md` | `analysis/` locator, cursor, symbols, hover, render, requires, search |
| `completion.md` | `analysis/completion.rs`, `cursor.rs`, `signature_help.rs` |
| `code-actions.md` | `analysis/code_actions.rs` |
| `types.md` | `analysis/types.rs`, `cursor.rs`, `hover.rs`, `workspace/rails/` |
| `synthesized.md` | `analysis/synthesized.rs`, `synthesize.rs`, `locator.rs`, `annotations.rs`, `structs.rs`, `workspace/rails/`, `generated.rs`, `knowledge/` |
| `erb.md` | `analysis/erb.rs` |
| `views.md` | `analysis/views.rs`, `workspace/rails/conventions.rs` |
| `tokens.md` | `analysis/tokens.rs`, `server/capabilities.rs` |
| `hierarchy.md` | `analysis/hierarchy.rs` |
| `environment.md` | `analysis/environment.rs` and every surface that fences on a tree the application does not load |
| `hints.md` | `analysis/hints.rs` |
| `features.md` | `workspace/features.rs`, `workspace/config.rs`, `analysis/synthesize.rs`, `types.rs`, `views.rs`, `workspace/rails/`, `knowledge/` |
| `logging.md` | `src/logging.rs`, `main.rs`, `analysis/requests.rs`, `analysis/mod.rs`, `workspace/config.rs` |
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
| `corpora.md` | `scripts/corpora.py`, `scripts/corpora.toml`, `scripts/canary.py`, `Makefile` |
| `audit.md` | `scripts/audit/**`, `audit/**`, `Makefile` |
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

make corpora           # set the six reference corpora up, end to end (ARGS=--only lobsters)
make corpora-status    # every corpus against its pin: Ruby, bundle, solargraph, cleanliness

make audit             # sweep all six corpora, then diff against the committed baseline
make audit-sample      # the draw and its mix; no server, no requests
make audit-cost        # what one position costs, and the sample size the budget buys
make audit-ledger      # what is in audit/ledger.json, and whether it still applies
make audit-baseline    # record the last sweep as the committed baseline

make notices           # regenerate THIRD-PARTY-NOTICES.txt after any dependency change
make notices-check     # fail if the committed copy is stale (CI and tag builds do this)
make ext-install       # the extension's dependencies (yarn 1's `npm ci`)
make ext-test          # compile, bundle and run the extension's tests
make ext-lint          # type-check the extension
```

`make coverage` splits into `coverage-run` (the instrumented suite, once) and `coverage-check` (the
gate), so every other coverage target reads that same run. `coverage-run` stamps the toolchain that
produced `target/llvm-cov-target` and wipes the directory when it changes.

`make canary` is **the one target in CI's reach that needs a network**, which is why it is not in
`make ci`. It fetches one pinned commit of a real Rails application into `tmp/corpora/` and opens
it the way an editor does, asserting the file count, the diagnostic counts and *no other code at all* exactly — the
numbers live in the `Makefile` — and time only against a ceiling well above the measurement. The
counts are properties of the commit, the timing a property of the runner. It does not cover gems. CI runs it as its own job; see
`canary.md`.

`make corpora` needs a network too, and is not run by CI at all. It sets up the six reference
corpora — six clones at pinned commits, each with its own
Ruby, its bundle, ruby-lsp and solargraph — into `tmp/corpora/`, which is gitignored. The canary's
lobsters is one of the six, so `make canary-clone` is that corpus' clone step. `corpora.md` is the
rule, and its first line is the one that can actually be broken: **no corpus source text is ever
committed into this repository.**

`make audit` needs the corpora on disk and is not in `make ci` for that reason. It sweeps all six
corpora — discourse included since 2026-09-12 — and asks a stratified sample of real cursors whether the
answer is right, which is the question `make canary` and a timing measurement do not ask: a fast
wrong answer passes both. It is a **measurement, not a gate** — no target fails on a number — and its two
committed files, `audit/ledger.json` and `audit/baseline.json`, hold integers, paths and hashes and
never a word of corpus source. `audit.md` is the rule; `corpora.md` is the licence rule it obeys.

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

## Toolchain

Rust **1.93.1**, pinned in `.tool-versions` (asdf/mise). Crate uses **edition 2024** —
`unsafe_op_in_unsafe_fn`, `gen` as a keyword, and the new `impl Trait` capture rules apply. Do not
write edition-2021 idioms that the 2024 lints reject.
