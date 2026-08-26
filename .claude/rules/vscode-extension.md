---
paths:
  - "editors/vscode/**"
---

# VS Code extension

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
- **The extension supplies no file watcher, and must not start supplying one again.** As of
  v0.2.0 the server registers its own `ya-lsp.toml` watcher through `client/registerCapability`,
  which is what makes reload work in editors with no extension to bring one, and
  `vscode-languageclient` installs that registration itself. A watcher passed through
  `synchronize.fileEvents` as well would be a second watcher on the same file — two
  `didChangeWatchedFiles` per save, and each one drops the whole graph and re-runs the gem index.
  It also belonged to the caller: the client never disposed it, so every restart leaked one.
