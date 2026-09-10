---
paths:
  - "editors/vscode/**"
---

# VS Code extension

- **`config.ts` and `server.ts` must never import `vscode`.** That is the only reason they are
  testable — there is no extension host here, and they hold the platform-specific mistakes.
  Anything needing the editor goes in `extension.ts`, tested through the bundle-loading smoke test.
- **Settings are read with `inspect`, never `get`.** `get` returns package.json's default when
  nobody set anything, which would make package.json a second source of truth for every default in
  `workspace::config` and would quietly outrank the server's. Only explicitly-set values are sent,
  which is also what keeps `ya-lsp.toml` on top.
- **Setting names are translated, not passed through.** The server deserializes
  `initializationOptions` with `deny_unknown_fields`, which does not degrade: one camelCase key
  rejects the *entire* layer and every other setting silently stops working. `contract.test.ts`
  spawns the real binary to check this, and carries a guard case so it cannot pass vacuously.
- **Every setting the server reads is `scope: "resource"`, and a manifest test says so.** A property
  with no `scope` is `window`-scoped, and VS Code does not read a `window`-scoped setting from a
  folder's `.vscode/settings.json`. `extension.ts` reads settings per folder because it runs one
  server per folder, so a wrong scope makes a multi-root workspace unable to vary the setting at
  all, silently, while the surrounding code looks right. Ten of twelve settings shipped that way
  through two releases, which is why the test's list comes from `serverOptions` itself — called with
  a `Settings` that records each key and answers `undefined` — rather than written out a third time.
  `serverPath` and `logLevel` are `machine-overridable`, honoured in a folder and additionally
  letting a container carry its own.
- **The manifest is checked from both languages, and the split is not arbitrary.**
  `manifest.test.ts` knows what `config.ts` reads: scopes, two-way agreement between the manifest
  and the settings sent, and whether every setting and enum value is explained.
  `tests/vscode_manifest.rs` knows what the server does: documented defaults against
  `Config::default()` and `DEFAULT_LOG_FILTER`, and the ten rule names against
  `diagnostics::known_names()`. Neither compiler sees the other side; a drifted default is wrong
  documentation with nothing to catch it, which is how `logLevel` shipped saying `warn` about a
  server that does `info`.
- **A description says what the user will see change, not what the server will do.** "Index Ruby's
  own core signatures" is about the indexer; what is being decided is whether `String#upcase`
  completes. Three rules follow. Every enum carries an `enumDescriptions` entry per value — a
  drop-down of bare words (`hint` against `warning`, `messages` against `verbose`) hides the whole
  decision. Every setting with a `ya-lsp.toml` twin names the TOML key, since the spellings differ
  by convention (`maxFiles` against `max_files`) and nothing else maps them. Where a setting has a
  cost the user should know about, the description says so in the text.
- **`diagnostics.rules` declares all ten rules rather than leaving the object open.** An open object
  completes its *values* and never its *names*, so the setting was an empty `{}` with no way to
  learn what goes in it — which is why nobody finds `parse-warning` to switch it off.
  `additionalProperties` stays, so an eleventh rule from rubydex still validates. The list exists
  twice, closed by a test rather than discipline: only the names are duplicated, no rule declares a
  `default`, and the default severities stay in `describe`.
- **No shipped setting key is renamed.** The extension is on the Marketplace, so a rename costs a
  `deprecationMessage`, a migration, and a window in which both keys are read and can disagree.
  Every clarity win worth having comes from a category title, a rewritten first sentence, or an
  `enumDescriptions` entry — none of which can break a settings file.
- **The document selector's pattern is the protocol's relative-pattern shape, never
  `vscode.RelativePattern`.** It is the only thing stopping one folder's client from claiming a
  sibling folder's files. `vscode-languageclient` 10 runs every selector through
  `asDocumentSelector`, whose `asGlobPattern` recognises exactly two things: a plain string, and LSP
  3.18's `{ baseUri, pattern }` where `baseUri` is a **URI string**. Everything else becomes
  `undefined`, including a `vscode.RelativePattern`, whose `baseUri` is a `Uri` object and fails
  `URI.is`. An undefined pattern does not narrow, it *widens*: `languages.match` then scores on
  language and scheme alone, so every folder's client claims every folder's Ruby files. Given the
  protocol shape, the client constructs the `vscode.RelativePattern` itself, which makes the
  separator right on Windows by construction. `activation.test.ts` pins both halves, the second as
  an explicit guard.
- **`engines.vscode` decides three other versions, and they are not independent.** `@types/vscode`
  is pinned *exactly* to it — a higher one compiles against APIs the oldest supported editor lacks
  and fails at a user's runtime rather than in CI. `vscode-languageclient` has its own floor (10.x
  wants 1.91). The Node that `@types/node` and esbuild's `target` describe is the editor's Electron,
  not `.tool-versions`: 1.108 is Electron 39.2.7, which is Node 22.21. Derive it from
  `microsoft/vscode`'s `.npmrc` at the matching `release/*` branch rather than guessing.
- **`yarn test` runs the bundle, not the sources.** `dist/extension.js` is what ships, and the
  failures worth catching — a dropped import, a command declared but never registered — only exist
  there.
- **Anything the server reads once at startup needs a restart, not a notification.** `logLevel` is
  `EnvFilter::try_from_env`; `serverPath` decided which process was spawned. `RESTART_REQUIRED` is
  the list, and the extension performs the restart rather than leaving the setting inert.
- **The extension supplies no file watcher, and must not start.** The server registers its own
  `ya-lsp.toml` watcher through `client/registerCapability`, which is what makes reload work in
  editors with no extension, and `vscode-languageclient` installs that registration itself. A
  watcher passed through `synchronize.fileEvents` as well is a second watcher on the same file — two
  `didChangeWatchedFiles` per save, each dropping the whole graph and re-running the gem index. It
  also belonged to the caller: the client never disposed it, so every restart leaked one.
