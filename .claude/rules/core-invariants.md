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
- **`didChange` carries every change, in order.** Ranges are expressed against the text the
  previous change produced, so they cannot be reordered or coalesced — full sync's "keep only
  the last one" shortcut silently corrupts the buffer under incremental sync.
