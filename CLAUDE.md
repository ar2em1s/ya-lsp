# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`ya-lsp` — "Yet Another LSP": a Language Server Protocol implementation for **Ruby**, written in Rust. The stated goals are lightweight, fast, and standalone (no Ruby runtime dependency).

## Current state

**All eight milestones are complete** — M0 (walking skeleton), M1 (diagnostics), M2 (navigation
core), M3 (gem indexing), M4 (workspace symbols & references), M5 (completion), M6 (the VS
Code extension), and M7 (Ruby's own core and stdlib). See `PLAN.md` for the plan and the status
notes recorded under each milestone. The cache question §8 left open is now **closed — no
cache**, with the numbers.

```
src/
  main.rs          CLI (--stdio, --licenses, -V, -h), stderr-only tracing init
  lib.rs           module root (a lib target exists so tests can drive the server in-process)
  licenses.rs      what `--licenses` prints; concatenates LICENSE.txt, NOTICE.txt, notices
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
    position.rs    LSP position <-> byte offset, UTF-8/16/32, incremental edits
  workspace/       workspace root, discovery
    config.rs      ya-lsp.toml
    bundler.rs     Gemfile.lock -> sources and specs (pure text, no I/O)
    ruby_version.rs which Ruby, without running ruby
    gems.rs        gem roots, spec -> directory, require_paths, Ruby's own lib, file walk
    rbs.rs         which copy of Ruby's signatures to index; the vendored fallback
    uri.rs         canonical document URIs
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

### Rules that matter when editing (the VS Code extension)

- **`config.ts` and `server.ts` must never import `vscode`.** That is the only reason they can be
  tested at all — there is no extension host here, and they are where the platform-specific
  mistakes live. Anything needing the editor goes in `extension.ts` and is tested through the
  bundle-loading smoke test instead.
- **Settings are read with `inspect`, never `get`.** `get` returns package.json's default when
  nobody has set anything, and sending that would make package.json a second source of truth for
  every default in `workspace::config` — and would quietly outrank the server's own. Only
  explicitly-set values are sent, which is also what keeps `ya-lsp.toml` on top.
- **Setting names are translated, not passed through.** The server deserializes
  `initializationOptions` with `deny_unknown_fields`, which does not degrade: one camelCase key
  rejects the *entire* layer and every other setting silently stops working. `contract.test.ts`
  spawns the real binary to check this, and carries its own guard case so it cannot pass vacuously.
- **The document selector's pattern is the protocol's relative-pattern shape, never
  `vscode.RelativePattern`.** It is the only thing stopping one folder's client from claiming a
  sibling folder's files. `vscode-languageclient` 10 runs every selector through
  `asDocumentSelector`, and its `asGlobPattern` recognises exactly two things — a plain string,
  and LSP 3.18's `{ baseUri, pattern }` where `baseUri` is a **URI string**. Everything else
  becomes `undefined`, including a `vscode.RelativePattern`, whose `baseUri` is a `Uri` object
  and so fails `URI.is`. An undefined pattern does not narrow to nothing, it *widens*:
  `languages.match` then scores on language and scheme alone, so every folder's client claims
  every folder's Ruby files. Given the protocol shape the client constructs the
  `vscode.RelativePattern` itself, which is what makes the separator right on Windows by
  construction. `activation.test.ts` pins both halves, the second as an explicit guard so it
  cannot pass vacuously.
- **`engines.vscode` decides three other versions, and they are not independent.**
  `@types/vscode` is pinned *exactly* to it — a higher one compiles happily against APIs the
  oldest supported editor does not have, and fails at a user's runtime rather than in CI.
  `vscode-languageclient` has its own floor (10.x wants 1.91). And the Node that `@types/node`
  and esbuild's `target` describe is the one the editor's Electron ships, not the one in
  `.tool-versions`: 1.108 is Electron 39.2.7, which is Node 22.21. Derive it from
  `microsoft/vscode`'s `.npmrc` at the matching `release/*` branch rather than guessing.
- **`yarn test` runs the bundle, not the sources.** `dist/extension.js` is what ships, and the
  failures worth catching — a dropped import, a command declared but never registered — only
  exist there.
- **Anything the server reads once at startup needs a restart, not a notification.** `logLevel` is
  `EnvFilter::try_from_env` and `serverPath` decided which process was spawned. `RESTART_REQUIRED`
  is the list, and the extension performs the restart rather than leaving the setting inert.
- **A watcher passed through `synchronize.fileEvents` belongs to the caller.** The client does not
  dispose it, so every restart would leak one.
- **`LICENSE.txt` is MIT and nothing else; the rbs notice lives in `NOTICE.txt`.** They were one
  file, split on a `---`, and the cost was measurable: `cargo about` could not recognise the
  appended file as MIT, so ya-lsp's own crate was filed under a generic template instead of its
  own copyright line, and GitHub's detector works the same way. A test asserts `LICENSE.txt` has
  no `---` and no `Soutaro` in it, and CI asserts the same of the copy inside the VSIX —
  re-appending is the obvious way to undo this.

- **`NOTICE.txt` covers *both* pieces of rbs material, and it is the only place the copyright
  holder is named.** One is the vendored signatures `build.rs` embeds; the other is the
  `ruby-rbs`/`ruby-rbs-sys` crates that compile rbs's C parser in, which have been there since
  M0. They share a holder and a licence, so they share a notice. `THIRD-PARTY-NOTICES.txt` does
  **not** substitute for it: `cargo about` only walks the Cargo graph, so it never sees the
  signature files at all, and neither rbs crate publishes a licence file for it to harvest — so
  the generated file reproduces the SPDX template with its unfilled `Copyright (c) <year>
  <owner>` line. Delete `NOTICE.txt` and no artifact names Soutaro Matsumoto anywhere.

- **`NOTICE.txt` reproduces `vendor/rbs/BSDL` and `COPYING` verbatim, and a test enforces it.**
  It is a hand-written file holding copies of two that a `vendor/rbs` bump can rewrite, and a
  copy that has drifted is a licence nobody granted. Re-assemble it from the files rather than
  editing the licence text in place. A second test pins the rbs version named in it.

  **The rule is per artifact, not per repository**, which is why the binary carries the notice
  itself: `ya-lsp --licenses` (`src/licenses.rs`) concatenates all three files, so a bare binary
  attached to a release, rehosted, or installed with `cargo install` is self-contained without
  anyone having to remember a second file. The VSIX ships all three because the Marketplace shows
  the licence and nobody runs `--licenses` on an extension, and the standalone `.tar.gz` / `.zip`
  ships them beside the binary for the same reason. Each is checked in CI, in the shell of the
  platform that built it. `licenses.rs` used to parse `LICENSE.txt` apart on its separator; it
  now just `include_str!`s three files, and that seam is gone.

  **`THIRD-PARTY-NOTICES.txt` covers the ~100 crates linked into the binary.** It is generated by
  `cargo about` from `about.toml` + `about.hbs`, **committed rather than generated at build time**
  so that `cargo build` and `cargo install` need no extra tool — CI regenerates it and fails on
  any difference. `about.toml`'s `accepted` list is ordered: the first entry matching a crate's
  expression is the licence elected, which is why `MIT` leads `Apache-2.0`. The repository holds
  the only copy of it, of `NOTICE.txt`, and of `CHANGELOG.md`: the extension's
  `vscode:prepublish` runs `stage-shared`, so every `vsce package` — CI's `yarn vsce package
  --target ...` included — gets all three without a separate step, and a missing root file fails
  packaging rather than shipping a VSIX without the notice. `editors/vscode/.gitignore` lists the
  staged copies so nobody edits one and loses it at the next package. It copies through `node -e`,
  not `cp`, because two of the six release runners are Windows. The changelog rides along for the
  same reason the notices do — the rule is per artifact — and `vsce` lowercases it to
  `changelog.md` in the VSIX, which is what the Marketplace reads for its Changelog tab.

### Rules that matter when editing (Ruby's own core and stdlib)

- **rubydex indexes RBS natively; the milestone was never about indexing.** `LanguageId::Rbs`,
  dispatched off the `.rbs` extension by `index_files`. What `workspace::rbs` owns is *which*
  copy of the signatures to use: `[rbs] path`, then the highest-versioned `rbs-*` gem on disk,
  then the copy `build.rs` embedded. Each rung has a test, and the bottom one is the only reason
  built-ins survive a machine with no Ruby.
- **The vendored copy is extracted to `~/.cache/ya-lsp/rbs-<version>/`, not indexed from memory.**
  rubydex keys documents by `Url::from_file_path` and go-to-definition has to answer with a URI
  the editor can open. The `.complete` marker is written last and holds the version, so a killed
  extraction is redone rather than half-trusted.
- **Indexing Ruby's core made the server faster, and the reason matters.** The steady resolve on a
  Rails app went 15.9 ms → 6.4 ms when 89 core signature files were added, because a bundle's
  references to `String`/`Hash`/`Kernel` had nothing to resolve against and pending work is what
  the resolver redoes. Adding names with nothing to resolve *against* them still costs: Ruby's own
  library is 727 files and +1 ms. Do not reason about graph size alone.
- **`ruby_lib_dirs` returns exactly one directory, requires a known Ruby version, and checks the
  whole path shape.** Taking every gem root's sibling would index macOS's Ruby 2.6 stdlib beside
  the project's 4.0. The ABI must match the resolved version and nothing is returned if none does
  — *including when no version resolves at all*, because macOS ships a vestigial 2.6 that
  `gem_roots` finds on every machine, and falling back to "the first root" made `"hello".u` offer
  `unspace` from a 2.6 `bigdecimal` patch on a machine with no Ruby. RVM's root is
  `~/.rvm/gems/ruby-4.0.1` — parent also named `gems` — so `lib/ruby/gems/<abi>` is checked in
  full.
- **A default gem's directory under `gems/` exists and is empty.** That is why they were silently
  dropped rather than reported unresolved: resolution succeeded and `load_paths_for` then found no
  `lib/`. The fix is Ruby's own library as a load path, held once on `Gems::ruby_lib` rather than
  attached to each of the forty gems inside it.
- **Every gem root reaches `gem_roots` through `Env`, the four absolute system paths included.**
  They used to be written into `gem_roots` itself, where nothing could steer them: a lockfile
  names an exact `name-version` directory a stranger's Ruby does not have, so they contributed
  nothing until Ruby's own library and rbs arrived with no such filter. `workspace::rbs`'s
  vendored-fallback tests then passed on a laptop with no Ruby and failed on CI, where a system
  Ruby's `rbs` gem answered `Discovered`. `Env::from_process` fills `system_roots` in;
  `Env::default` leaves it empty, which is what makes a fixture hermetic. The analysis harness
  still writes a `ya-lsp.toml` turning `default_gems` and `rbs` off, now only to skip ~800 files
  of signature work no test there asks about.
- **Which classes are "core" is rbs's call and it moves.** `Set` and `Pathname` are both in
  `core/` as of rbs 4.x. A test that needs a genuinely stdlib-only constant uses `OptionParser`.
- **Bumping `vendor/rbs` changes the answers ya-lsp gives.** Measured across rbs 3.10 → 4.1.3:
  76 declarations added, 82 removed out of ~3,640, and most removals were methods re-homed onto
  an ancestor. Treat it as a behaviour change, not a dependency update; `vendor/rbs/README.md`
  has the refresh procedure and the licence.
- **Nothing reads RBS's types.** Signatures are indexed as declarations only. `(?symbol) -> String`
  is sitting in the graph and using it is return-type inference, which PLAN.md §7 rules out.

### Rules that matter when editing (completion)

- **A literal's class is read off the parse, never from the text.** `Receiver::Literal` maps a
  Prism node kind to a core class name, so `4.2.` is a `Float` rather than an `Integer` with a
  message. `Foo.new.` becomes `Receiver::Instance`, which resolves to the class rather than its
  singleton. A local takes the type of the nearest preceding assignment whose *value* ends before
  the cursor — using the name instead would let `x = x.` type `x` from the statement it is part
  of. That local rule is the one place in the module that can be confidently wrong.
- **A literal receiver must degrade to the name-based list, not to silence.** With `[rbs]` off
  there is no `String` declaration, and `completion::declared` returning `None` is what lets
  `Context::MethodCall` fall through to `by_name`. An id built from a name that was never indexed
  is a well-formed id that answers nothing.
- **`cursor.rs` never sees a graph and `completion.rs` never sees syntax.** That is the only
  reason the awkward classifications are cheap to test: a trailing `.` on the line above an
  `end`, a cursor in the whitespace after a comma, `#{}` inside a string. Resolving what a
  receiver *is* belongs on the other side of the line.
- **Completion reads Prism's error recovery, not the text.** `Foo::` is a syntax error that
  recovers into a `ConstantPathNode` with an empty `name_loc` at the cursor; `foo.` into a
  `CallNode` with an empty `message_loc`. A backwards text scan is used for one thing — where the
  half-typed word starts, which decides what an accepted item *replaces* — and only after Prism
  has said the cursor is somewhere Ruby can be written. Without that, completion fires inside
  comments and strings.
- **A class or module body completes against the class, not an instance of it.** `self` in a
  class body is the class object, so `Scope::at` sets `self_id` to the nesting's singleton
  whenever the cursor is in a namespace with no enclosing method. Getting it wrong offers
  `valid?` where only `validates` is callable — measured, 49 suggestions that would all raise
  `NoMethodError`. The top level is *not* one of these: `self` is `main`, an ordinary `Object`.
- **`self_decl_id` must be stated for `MethodCall` and `NamespaceAccess`.** rubydex derives it
  only for `Expression`; the receiver contexts use it purely as the caller in their visibility
  check, and `None` there means "outsider" — a class would stop seeing its own private class
  methods. `Scope::caller` is the one place that decides.
- **Ranking is `(group, internal, tier, length, sequence, label)` and every field earns its
  place.** `internal` sinks names starting with punctuation or `_`, without which a Rails app
  opens `User.` on `__send` and `_fork`. `sequence` is *only* a keyword argument's position in
  its signature: rubydex's emission order within a namespace is a hash map's and reshuffles when
  a member is added.
- **Keyword arguments are never guessed from a name.** `Context::Argument` only becomes
  `MethodArgument` when `locator::resolve` was *precise*; otherwise it degrades to a plain
  expression. Another class's parameters would be a syntactically valid wrong answer.
- **Every completion list is `isIncomplete`, and nothing found is `null` rather than `[]`.** The
  list was filtered against one prefix, so the client must re-ask rather than narrow it; and a
  `null` is what tells the client to fall back to its own word list inside a comment.
- **`MAX_COMPLETION_ITEMS` is not a latency control** — unlike `MAX_WORKSPACE_SYMBOLS`. Measured
  over 128/256/512/1024 it moves the worst request by about a millisecond, because the cost is
  the graph work in front of it. It bounds the response size. Re-measure before assuming
  otherwise; PLAN.md's M5 status has the sweep.
- **`completionItem/resolve` carries the `DeclarationId` as a string.** It is a 64-bit hash and
  JSON numbers are doubles, so a round trip through a client silently corrupts a number.
- **`render::is_nameable` is the one test for "rubydex invented this name".** Angle brackets are
  its only punctuation for them — `Foo::<Foo>` and `<uri>:<offset><anonymous>` — and both
  completion and `workspace/symbol` go through it. A 17,557-file workspace answered `::` with a
  page of anonymous classes before it existed.

### Rules that matter when editing (project-wide search)

- **`is_own_code` is the one definition of "the user's code", and three features turn on it.**
  Diagnostics, `references`, and `workspace/symbol`'s ranking all ask it. It is the workspace URI
  prefix *minus* `gem_prefixes`, because a vendored bundle sits inside the workspace root. Any new
  "is this theirs?" test must go through it rather than re-deriving the prefix.
- **`workspace/symbol` ranks the user's own code above match quality, deliberately.** A project has
  ~3k declarations and its bundle ~150k, so ranking them together fills the picker with gems:
  measured, `"user"` returned four exact-matching gem methods above the project's own `Users`.
  `search::rank` is `(own, tier, simple_len, name_len, name)` — never by `DeclarationId`, which is
  a hash and would shuffle between runs.
- **rubydex's fuzzy score is unusable for ranking.** `match_score` returns `query.len()` for every
  match and 0 otherwise, so every hit ties. `declaration_search` is kept for its parallel filter;
  the ordering is `search::tier`.
- **Both caps are load-bearing and measured.** `MAX_WORKSPACE_SYMBOLS` exists because a subsequence
  match on one character hits most of a bundle on every keystroke; `MAX_REFERENCES` exists so a
  name-based match cannot send a multi-megabyte response, and reaching it tells the user, because
  a truncated find-all-references looks exactly like a complete one.
- **`references` costs the size of the workspace, not of the answer** (0.4 ms at 161 files, 53 ms
  at 17,557), and `workspace/symbol` costs the size of the whole graph, gems included.
- **rubydex never links a method reference to a declaration.** `record_resolved_reference` handles
  constants only, so `MethodDeclaration::references()` is always empty. Do not "fix" the
  name-based path by routing it through the resolution — and `Target::Call` deliberately skips the
  resolution entirely, so a `define_method`'d name with no declaration still lists its call sites.
- **The synthetic `<Foo>` references reach `references` too.** A cursor on `class << self` resolves
  to the singleton declaration, which is where rubydex files the fabricated references. Without
  `references::is_synthetic` that answers with the span of `Person.new` in another file.
- **`Ranges` exists because a project-wide answer names many spans in few files.** Converting a
  span means reading and line-indexing its whole file; do that per span and a file with fifty
  references is read fifty times.

### Rules that matter when editing (gems)

- **Gem discovery reads `gems::Env`, never `std::env` directly.** That is the only reason the
  layout fixtures can run against a temp directory instead of whatever Ruby the machine has.
  `Workspace::load_with_env` is the seam.
- **The ABI directory is globbed, never computed.** Ruby 4.0.1 installs into
  `lib/ruby/gems/4.0.0/`. `abi_of` exists only to *prefer* a globbed directory.
- **Gem roots are deduplicated by their canonical path but stored as spelled.** Storing the
  canonical form breaks a vendored bundle on any machine whose workspace root reaches disk
  through a symlink (every macOS temp directory): its files get a different URI spelling from
  everything else, forking a second document per file.
- **Diagnostics are filtered by URI prefix, and the workspace prefix alone is not enough.** A
  vendored bundle is inside the workspace root by construction, so `is_own_code` also
  excludes the gem roots, the RBS root, and Ruby's own library. Without it, opening a Rails app
  publishes 208 unfixable squiggles.
- **Background gem chunks set `dirty` but must not arm `resolve_at`.** Arming it per chunk pushes
  the user's own diagnostics out for the whole index. Conversely, `serve` must check `dirty`, not
  `resolve_at`, or requests answer against an unresolved graph during the index.
- **`GEM_FILES_PER_STEP` is measured, not chosen.** Its cost is the resolve the *next request*
  runs over what the step added — p90 goes 94/117/127/333 ms at 50/100/200/400. Re-measure before
  changing it; PLAN.md's M3 status has the table.
- **`require_paths` comes from `specifications/<full name>.gemspec`**, RubyGems' serialised
  gemspec, which is a plain array literal. Git and path sources have no such file — only the
  project's own arbitrary-Ruby `.gemspec` — so they fall back to `lib`. Absolute entries are
  native-extension stubs and are skipped, the same rule rubydex's Ruby-side `graph.rb` applies.
- **Unresolved gems are counted per name, not per spec.** A lockfile resolved for seven platforms
  lists `nokogiri` seven times and six of those directories will never exist here.

### Rules that matter when editing

- **`analysis/` is the only module allowed to name a rubydex type.** rubydex is pre-1.0 with one
  published release; when its API churns the blast radius must be one directory.
- **Nothing may write to stdout** except the LSP transport. Logging goes to stderr via `tracing`
  (`YA_LSP_LOG` sets the filter). A stray `println!` disconnects the editor with no error.
- **`Graph::set_encoding` is inert** — offsets from rubydex are always UTF-8 bytes. Convert with
  `analysis::position::TextDocument`, never with `Offset::to_location`.
- **Document keys go through `workspace::uri::DocUri`**, which spells URIs exactly the way
  rubydex does (`url::Url::from_file_path`). A raw client URI can fork a duplicate document.
- **rubydex method declarations carry parens**: `graph.get("Person#shout")` is `None`;
  `"Person#shout()"` works. Declarations key their *members* by the same parenthesised string,
  but method *references* are recorded under the bare name — except `alias`, which uses the
  parenthesised form. `analysis::locator::member_name` is the one place that reconciles them;
  get it wrong and every method lookup misses silently. `StringId::from(&str)` is a pure hash,
  so a key can be built without touching the graph.
- **rubydex's indexing entry points need absolute paths.** `index_files`/`index_source` key
  documents through `Url::from_file_path`, which fails on a relative path — and fails *silently*,
  indexing nothing. Always pass absolute paths.
- **Diagnostic defaults are measured, not chosen.** Most rubydex rules fire on correct Ruby
  (`dynamic-ancestor` hits solargraph 384 times) and ship `Off`. Before changing a default in
  `analysis::diagnostics`, run it over `tmp/` and count. The table in PLAN.md's M1 status has the
  current numbers.
- **`publishDiagnostics` is stateful per URI.** Clearing means sending an explicit empty array;
  sending nothing leaves the old squiggles on screen forever.
- **rubydex invents constant references you must not follow.** Every call with an implicit or
  constant receiver gets a fabricated reference to `<Foo>` so the singleton class can be
  resolved. `Person.new` therefore has two references over the same six bytes, and for an
  *implicit* receiver the invented one spans the **whole call** — so `alias_method :a, :b`
  carries a reference that resolves to `class << self`. Angle brackets are rubydex's only
  spelling for singleton names; `analysis::locator` drops references that start with `<`.
- **A definition matches only its name span, never its body.** `Definition::name_offset` is
  `None` for constants, `attr_*`, and aliases — for those, `offset()` is already just the name.
  Widening this to the body makes hover fire over whitespace.
- **Client capabilities gate response shapes.** `analysis::ClientSupport` reads
  `hierarchicalDocumentSymbolSupport` and `definition.linkSupport`; both default to false.
  Sending nested symbols or `LocationLink`s to a client that did not ask can make it fail to
  parse the response, not merely ignore it.
- **A gem's file URIs are inside the workspace when the bundle is vendored.** Any new
  "is this the user's code?" test has to exclude `Analysis::foreign_prefixes`, not just the
  workspace prefix — go through `is_own_code` rather than writing another one. That list holds
  the gem roots, the RBS root, and Ruby's own library directory; it was called `gem_prefixes`
  until M7 made the name untrue.
- **How a construct is spelled for a human lives in `render`, and is shared.** `split_qualified`
  (symbol lists) and `qualified_name` (hover) both go through `singleton_parts`, and
  `symbols::kind_of` is shared with `search`. A symbol that reads differently in the outline and
  in the picker is a bug, not a style choice.
- **`didChange` carries every change, in order.** Ranges are expressed against the text the
  previous change produced, so they cannot be reordered or coalesced — full sync's "keep only
  the last one" shortcut silently corrupts the buffer under incremental sync.

## Commands

```bash
# Third-party notices, after any dependency change (CI regenerates and diffs)
cargo install cargo-about --locked --features cli
cargo about generate about.hbs -o THIRD-PARTY-NOTICES.txt
```

```bash
# The extension (from editors/vscode)
yarn install --frozen-lockfile   # once; the flag is yarn 1's `npm ci`
yarn test                        # compile, bundle, and run the 24 tests
yarn lint                        # type-check only
yarn bundle                      # dist/extension.js, what actually ships
yarn vsce package --target darwin-arm64   # a VSIX; needs server/ya-lsp staged first
```

The extension's `contract.test.ts` talks to a real `ya-lsp`; it looks for
`editors/vscode/server/ya-lsp`, then `target/release`, then `target/debug`, and **skips itself**
if it finds none — so build the server before trusting a green run. Press F5 in the repo root to
launch an extension host with everything built; `.vscode/tasks.json` does the staging.

```bash
cargo build                  # debug build
cargo build --release        # optimized build
cargo run                    # run the binary
cargo test                   # all tests
cargo test <name>            # tests whose path/name contains <name>
cargo test <name> -- --exact --nocapture   # one exact test, with stdout shown
cargo check                  # type-check only
cargo clippy --all-targets   # lint (kept clean)
cargo fmt                    # format (kept clean)
```

Manual smoke test against a real Ruby repo — drives the binary over stdio and reports index time:

```bash
cargo build --release
python3 <driver> <repo> target/release/ya-lsp   # see PLAN.md M0-M4 status for numbers
```

The driver speaks LSP over the binary's stdio: `initialize`, `initialized`, collect
`textDocument/publishDiagnostics` until the server goes quiet, then `didOpen` real files and ask
for `documentSymbol` / `hover` / `definition` at their symbol positions. Read messages on a
thread with a timeout — a blocking read with no timeout hangs the moment the server has nothing
left to say. Advertise `hierarchicalDocumentSymbolSupport` and `definition.linkSupport` in
`initialize`, or the server correctly answers with the flat pre-3.10 shapes instead.

For M5, the number that matters is not request latency but **typing** latency: send a `didChange`
and a `textDocument/completion` for every character of a phrase, which is what an editor does.
Measure at least one of each context — an expression, `Foo::`, `Foo.`, an argument list, and a
receiver with no type — because they have different costs and only the last one scales with the
workspace. The buffer should be one the disk has never seen, since that is the situation
completion always runs in.

For M7, the number that matters is the **steady resolve**, not end-to-end latency: run the server
with `YA_LSP_LOG=ya_lsp=debug`, wait for the `$/progress` end, then send thirty
`didChange` + `completion` pairs and take the median of the `resolved in ...` lines. End-to-end
completion latency moves for the same reason but with more noise in it. Compare configurations in
one process each — `[rbs] enabled`, `[rbs] stdlib`, `[gems] default_gems` — and warm the page
cache first, or the config that happens to run first pays for the disk and the comparison is
meaningless.

For M4, the interesting measurements need two workspaces, not one. A real Rails app is the
*small* case for `references` — its cost is the size of the user's own code, and 161 files is
0.4 ms. The worst case has to be built: point the workspace root at a whole gem directory
(`~/.asdf/installs/ruby/*/lib/ruby/gems/*/gems`) with a `ya-lsp.toml` that disables gems and
raises `index.max_files`, and 17,557 files all count as the user's own. That is where
`MAX_REFERENCES` is reached and where the linear scaling is visible.

For M3, advertise `window.workDoneProgress`, **answer the server's
`window/workDoneProgress/create` request**, and wait for `$/progress` `end` before asserting that
a gem is navigable — it is background work, and a request racing it correctly answers `null`. To
test the "no Ruby" claim honestly, point `PATH` at an empty directory: macOS keeps a stub `ruby`
in `/usr/bin`. Real Rails apps to drive against: `~/dev/home/home` (Rails 8.1, 151 gems, all
installed) and `~/dev/RubyBookStore` (Rails 6, whose Ruby is *not* installed — the degraded
path).

## Toolchain

Rust **1.93.1**, pinned in `.tool-versions` (asdf/mise). Crate uses **edition 2024** — `unsafe_op_in_unsafe_fn`, `gen` as a keyword, and the new `impl Trait` capture rules apply; don't write edition-2021 idioms that the 2024 lints reject.
