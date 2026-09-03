---
paths:
  - "src/**"
  - "tests/**"
  - "build.rs"
---

# Crate-wide invariants

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
  `analysis::diagnostics`, run it over `tmp/` and count.
- **`publishDiagnostics` is stateful per URI.** Clearing means sending an explicit empty array;
  sending nothing leaves the old squiggles on screen forever.
- **Client capabilities gate response shapes.** `analysis::ClientSupport` reads
  `hierarchicalDocumentSymbolSupport` and `definition.linkSupport`; both default to false.
  Sending nested symbols or `LocationLink`s to a client that did not ask can make it fail to
  parse the response, not merely ignore it.
- **A gem's file URIs are inside the workspace when the bundle is vendored.** Any new
  "is this the user's code?" test has to exclude `Analysis::foreign_prefixes`, not just the
  workspace prefix — go through `is_own_code` rather than writing another one. That list holds
  the gem roots, the RBS root, and Ruby's own library directory; it was called `gem_prefixes`
  until M7 made the name untrue.
- **rubydex's resolver panics after a deletion, and `analysis::resolve` is where that is
  contained.** `Graph::delete_document` invalidates before it untracks the deleted document's
  strings, so the work the invalidation queued can name a string that has gone, and
  `resolution.rs:748` unwraps it. Reproduced by deleting `lib/solargraph/yard_map/to_method.rb`
  from a solargraph v0.58.2 checkout; not reproducible under about two hundred files. There is
  no published version to upgrade to. `resolve` catches the unwind, says one sentence, and
  rebuilds the graph from scratch, guarded by `recovering` so a rebuild that crashes again
  stops. **Never call `Resolver::resolve` from anywhere else** — a second call site is a second
  way for the analysis thread to die, and a dead analysis thread is a server that answers
  nothing at all with nothing said anywhere.
- **`Workspace::indexes` and `Workspace::discover` are one set of rules with two entry points.**
  The predicate the file watcher asks and the walk that built the index must agree on every
  path: one that the walk indexes and the predicate rejects never refreshes again, and the
  reverse indexes what the user excluded. Neither is visible for the life of the process. They
  share the compiled globs and the `ignore::WalkBuilder`, and
  `the_predicate_answers_exactly_what_the_walk_collected` asserts them against each other over
  every file in a fixture tree — never each against a hand-written list, which can be wrong in
  the same way twice.
- **`analysis/position.rs` is the one module held by properties rather than only by fixtures, and
  the reason is the shape of its input.** A change list is a *history* — every element is
  interpreted against the text the one before it left behind — so what would need enumerating is
  not a string but a sequence, and the file sat at 100% of lines and branches with none of that
  asked. `proptest` (a dev-dependency; `about.toml` ignores those, so it owes no notice) generates
  buffers by concatenating pieces from a fixed alphabet — an accent, CJK, an emoji, a combining
  mark, and all three line terminators including a lone `\r` — which is what makes a shrunk
  counterexample legible. Three properties: a change list lands where plain `String::replace_range`
  lands, any `Position` a client can send resolves to an offset that is in range and on a character
  boundary, and every addressable offset round-trips. Adding a fourth is cheap; **weakening the
  generator is not** — its first run found an out-of-bounds in the test corpus helper that eleven
  hand-written strings had never reached, because none of them ended in a bare `\r`.
- **`didChange` carries every change, in order.** Ranges are expressed against the text the
  previous change produced, so they cannot be reordered or coalesced — full sync's "keep only
  the last one" shortcut silently corrupts the buffer under incremental sync.
