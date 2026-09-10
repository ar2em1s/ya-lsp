---
paths:
  - "src/**"
  - "tests/**"
  - "build.rs"
---

# Crate-wide invariants

- **`analysis/` is the only module allowed to name a rubydex type.** rubydex is pre-1.0 with one
  published release; when its API churns the blast radius must be one directory. The return-type
  table is inside that directory and *beside* the graph rather than in it, which keeps the pin cheap
  to reverse — `types.md`.
- **Nothing may write to stdout** except the LSP transport. Logging goes to stderr via `tracing`
  (`YA_LSP_LOG` sets the filter). A stray `println!` disconnects the editor with no error.
- **`Graph::set_encoding` is inert** — offsets from rubydex are always UTF-8 bytes. Convert with
  `analysis::position::TextDocument`, never with `Offset::to_location`.
- **Document keys go through `workspace::uri::DocUri`**, which spells URIs exactly as rubydex does
  (`url::Url::from_file_path`). A raw client URI can fork a duplicate document.
- **Two ways to resolve a call, and the difference is deliberate.** `locator::resolve_typed` adds one
  rung — a receiver ya-lsp typed itself, from a signature or an assignment — and needs the document's
  text, so `hover` and `definition` use it and cannot disagree. `locator::resolve` has no text and
  cannot derive anything; `references`, the type hierarchy and `rename` stay on it **on purpose**,
  because a work list is a list of places to edit and a derived receiver is the one thing in it that
  could be wrong.
- **rubydex method declarations carry parens**: `graph.get("Person#shout")` is `None`;
  `"Person#shout()"` works. Declarations key their *members* by the same parenthesised string, but
  method *references* are recorded under the bare name — except `alias`, which uses the parenthesised
  form. `analysis::locator::member_name` is the one place that reconciles them; get it wrong and
  every method lookup misses silently. `StringId::from(&str)` is a pure hash, so a key can be built
  without touching the graph.
- **rubydex's indexing entry points need absolute paths.** `index_files` / `index_source` key
  documents through `Url::from_file_path`, which fails on a relative path — and fails *silently*,
  indexing nothing.
- **Diagnostic defaults are measured, not chosen.** Most rubydex rules fire on correct Ruby
  (`dynamic-ancestor` fires hundreds of times on a real project) and ship `Off`. Before changing a
  default in `analysis::diagnostics`, run it over a corpus in `tmp/` and count — `benchmarking.md`.
- **`publishDiagnostics` is stateful per URI.** Clearing means sending an explicit empty array;
  sending nothing leaves the old squiggles on screen forever.
- **Client capabilities gate response shapes.** `analysis::ClientSupport` reads
  `hierarchicalDocumentSymbolSupport` and `definition.linkSupport`; both default to false. Sending
  nested symbols or `LocationLink`s to a client that did not ask can make it fail to parse the
  response, not merely ignore it.
- **A gem's file URIs are inside the workspace when the bundle is vendored.** Any new "is this the
  user's code?" test must exclude `Analysis::foreign_prefixes`, not just the workspace prefix — go
  through `is_own_code`. That list holds the gem roots, the RBS root, and Ruby's own library.
- **rubydex's resolver panics after a deletion, and `analysis::resolve` contains it.**
  `Graph::delete_document` invalidates before it untracks the deleted document's strings, so the work
  the invalidation queued can name a string that has gone, and `resolution.rs:748` unwraps it.
  Reproduced by deleting a file from a large real checkout; not reproducible on a small workspace. There is no published version to upgrade to. `resolve` catches
  the unwind, says one sentence, and rebuilds the graph from scratch, guarded by `recovering` so a
  rebuild that crashes again stops. **Never call `Resolver::resolve` from anywhere else** — a second
  call site is a second way for the analysis thread to die, and a dead analysis thread is a server
  that answers nothing at all with nothing said anywhere.
- **rubydex's *indexer* panics too, and `analysis/indexer.rs` is the bulkhead.** 0.2.5 unwraps a
  lexical scope on `extend self` inside a `Class.new` / `Module.new` block, which a real gem in a real
  bundle writes and a user can type into a buffer. **The pinned rev fixes that one and the
  bulkhead stays**: `create_declaration`'s two unwraps are still on upstream's `main`, so a version
  fixes an instance of the hazard, not the hazard. **Never call `rubydex::indexing::index_files` or
  `index_source` directly** — `indexer::index_files` and `indexer::index_source` are the entry points.
  The failure unit is the **file**, not a detail: wrapping `index_files` in one `catch_unwind` loses
  whatever the dying worker had stolen — measured, a handful of bad files silently lost many times
  their number of good ones. So the pool is ours: one file at a time out of a shared channel, `catch_unwind` around
  `build_local_graph`, and a serial merge. The merge is deliberately **not** contained — a half-merged
  graph is the unknown state `resolve`'s `rebuild()` exists for, and catching it here hides it.
- **A document that crashed the indexer goes on `Analysis::skipped`, and `rebuild` keeps it.** That
  set is the one thing `rebuild` does not clear, because everything else it clears is keyed by a graph
  about to be replaced and this is keyed by a file still on disk — and `resolve`'s crash recovery *is*
  a rebuild, so a workspace holding a bad file would otherwise take the recovery down with it. **A
  document is on the list exactly while the last attempt to index it panicked**, which makes it
  self-clearing: every route that indexes because the text may have moved retries and drops the entry
  when it works, and the two that re-read unmoved text consult it (the workspace walk and the gem
  batch). `record_skip` warns every time and shows the user a message only the first, because the
  buffer route retries per keystroke.
- **Answering a request is guarded too — a third seam, not the same one.** rubydex 0.2.5 unwraps
  twice in `find_self_receiver_declaration`, which eight lines of ordinary Ruby reach from
  `textDocument/definition` — the *request* path, covered by neither the indexing bulkhead nor
  `resolve`'s guard. `Analysis::serve` wraps `settle` and the dispatch together and answers an
  `InternalError`, so the failure unit is the request. That error is deliberately not a `messages::`
  sentence: it is addressed to the client and answers one request (`messages.md`). The pinned rev
  fixes that unwrap too — `a_constant_alias_reopened_under_its_alias_answers_rather_than_crashing`
  asserts it — so the seam is exercised by `REQUESTS_TO_CRASH` and kept for what it bounds.
- **rubydex is pinned by revision, not by branch.** 0.2.5 is the only version on crates.io and the two
  panics above are fixed only on an unreleased `main`, so the dependency is `git` + an explicit `rev`.
  A branch pin would make `cargo update` a silent behaviour change, and moving the rev once showed
  why: upstream deleted `expression_completion`'s "derive `self` from the nesting" branch **while
  leaving the doc comment that promises it**, which cost every expression completion its methods and
  instance variables until `receiver_for` started passing `Scope::caller`. Moving the rev is a port
  and a five-corpus sweep, never a bump. `Rule`'s severities stay ya-lsp's own measured ones —
  upstream now ships `default_severity` and nothing reads it — and so does its *name*:
  `InvalidPrivateConstant` became `InvalidConstantVisibility` upstream and the config key did not
  follow, because that key is a sentence a user writes.
- **The default panic hook stays installed, everywhere.** Every `catch_unwind` in this crate relies on
  it: rubydex's own file and line reach stderr through it, and that is what a bug report is made of. A
  contained panic that prints nothing is a bug nobody can report. For the same reason no profile may
  set `panic = "abort"` — every one of the three seams would become a process death.
- **`Workspace::indexes` and `Workspace::discover` are one set of rules with two entry points.** The
  predicate the file watcher asks and the walk that built the index must agree on every path: one the
  walk indexes and the predicate rejects never refreshes again, and the reverse indexes what the user
  excluded. Neither is visible for the life of the process. They share the compiled globs and the
  `ignore::WalkBuilder`, and `the_predicate_answers_exactly_what_the_walk_collected` asserts them
  against each other over every file in a fixture tree — never each against a hand-written list, which
  can be wrong the same way twice. **`Workspace::admits` is a third entry point held to the same
  standard**: `indexes` without its include half, for the one file type `index.include` can never name
  (a `db/structure.sql`), asserted *strictly wider* over the same tree.
- **`analysis/position.rs` is held by properties rather than only by fixtures, because of the shape of
  its input.** A change list is a *history* — every element interpreted against the text the one
  before left behind — so what would need enumerating is a sequence, not a string, and the file sat at
  100% of lines and branches with none of that asked. `proptest` (a dev-dependency; `about.toml`
  ignores those, so it owes no notice) generates buffers by concatenating pieces from a fixed alphabet
  — an accent, CJK, an emoji, a combining mark, and all three line terminators including a lone `\r` —
  which makes a shrunk counterexample legible. Three properties: a change list lands where plain
  `String::replace_range` lands; any `Position` a client can send resolves to an offset that is in
  range and on a character boundary; every addressable offset round-trips. Adding a fourth is cheap;
  **weakening the generator is not** — its first run found an out-of-bounds in the test corpus helper
  that eleven hand-written strings had never reached, because none ended in a bare `\r`.
- **A template is blanked on every route into the server, and there are four.** `.erb` reaches rubydex
  through `index_templates` on the cold walk and through `index_buffer` from `didOpen`, `didChange`
  and the watcher; it reaches the *parsers* through `with_text`, which ten of the eighteen requests
  use instead of the graph. Every one gets `erb::ruby_view`, which is length- and line-preserving, so
  there is no second coordinate system anywhere. The one accessor handing back the real markup is
  `with_source`, and only `completion` calls it (`erb.md`). `codeAction` is declined in a template
  outright: it is the one that writes *lines* (`code-actions.md`).
- **A document is *read* and it is *addressed*, and for a template those are two different strings.**
  One coordinate system — the byte offset — and two texts a *column* can be counted in, because an LSP
  column counts code units and blanking a 3-byte `“` writes three spaces where the client counts one
  UTF-16 unit. `TextDocument::blanked` holds the view beside the source, `text()` is the only accessor
  answering with the view, and every conversion in `position.rs` goes through `coordinates()`. One
  line index serves both — `ruby_view`'s length-and-line-break property spent a second time.
  `a_blanked_document_is_addressed_as_the_text_the_client_has` states it as an equality against the
  unblanked document rather than as four assertions, so any conversion reaching for `self.text` fails
  it. Getting this wrong is silent, wrong rather than absent, and only for people who do not write
  their markup in English — which is why the fixture is not ASCII.
- **`didChange` carries every change, in order.** Ranges are expressed against the text the previous
  change produced, so they cannot be reordered or coalesced — full sync's "keep only the last one"
  shortcut silently corrupts the buffer under incremental sync.
