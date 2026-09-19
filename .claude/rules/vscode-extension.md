---
paths:
  - "editors/vscode/**"
---

# VS Code extension

## Must

1. **`config.ts`, `server.ts` and `claims.ts` never import `vscode`.** That keeps them testable
   without an extension host. Editor-only code goes in `extension.ts`.
2. **Read settings with `inspect`, never `get`.** `get` returns package.json's defaults, which would
   become a second source of truth and outrank `ya-lsp.toml`. Send only values the user set.
3. **Translate setting names; never pass them through.** The server's `initializationOptions` use
   `deny_unknown_fields`, so one camelCase key silently rejects every setting. `contract.test.ts`
   runs the real binary, with a guard case (build the server first, or it skips).
4. **Every setting the server reads is `scope: "resource"`.** A setting with no scope is `window`,
   which a folder's `.vscode/settings.json` cannot set. The test builds its list from
   `serverOptions`. `serverPath` and `logLevel` are `machine-overridable`.
5. **Never rename a shipped setting key.** It is on the Marketplace. Improve titles, first sentences
   and `enumDescriptions` instead.

## Writing the manifest

- **Describe what the user will see change**, not what the server does ("`String#upcase`
  completes").
- **Give every enum value an `enumDescriptions` entry.**
- **Name the `ya-lsp.toml` twin of every setting** (`maxFiles` ↔ `max_files`).
- **State any cost in the description.**
- **`diagnostics.rules` declares all the rules by name** so they complete, but keeps
  `additionalProperties`. No rule declares a `default`.
- **Checked from both sides:** `manifest.test.ts` (scopes, settings sent, descriptions) and
  `tests/vscode_manifest.rs` (defaults vs `Config::default()` and `DEFAULT_LOG_FILTER`, rule names
  vs `diagnostics::known_names()`).
- **New Ruby file extensions are added to the existing `ruby` language**, never as a new language.
  `.jbuilder`, `.builder` and `.ruby` are there, and `LANGUAGE_IDS` stays two long. Held by
  `every_language_the_server_claims_is_one_the_extension_activates_for`.

## Several servers in one window

One client runs per workspace folder. The document selector is the only gate: `workspaceFolder`
sets `rootUri` and filters nothing.

1. **Each client's selector is its own folder, and only that folder.** The server registers its
   gem and RBS roots itself (`client/registerCapability`). Never rebuild gem discovery in
   TypeScript.
2. **`claims.ts` decides who answers for a shared root** (`middleware.handleRegisterCapability`):
   - The first client to ask wins.
   - A root inside another workspace folder is refused (`innermostFolder`).
   - A registration whose selector ends up empty is dropped, not forwarded.
   - Registrations outside `ya-lsp-documents/` pass through untouched.
   - `onFoldersChanged` rebuilds only the clients that asked for an orphaned root.
3. **For nested folders, the outer client is narrowed where it sends** (`narrowing()`):
   - `GeneralMiddleware.sendRequest` covers every request, and the four text-sync hooks cover the
     rest.
   - The predicate is `claimedByNestedFolder`.
   - `activation.test.ts` pins the wiring.
   - `index.exclude` does not help here, because opened buffers are always indexed.
4. **Generated documents:** `middleware.provideTextDocumentContent` turns an error into `null`, so
   VS Code tries the next server.
5. **Write patterns as `{ baseUri: string, pattern }`, never as `vscode.RelativePattern`.**
   languageclient 10 turns a `RelativePattern` into `undefined`, and that *widens* the match to
   every folder.

## Settled

- **`willRenameFiles` needs no extension code.** languageclient's built-in feature handles it. Don't
  add a middleware hook without a failure to point at.
- **The extension supplies no file watcher.** The server registers its own, and a second one
  doubles every reload.
- **`RESTART_REQUIRED` is `serverPath` only.** The extension restarts the server for it. Log level
  reloads live (`logging.md`).
- **`engines.vscode` fixes three other versions:** `@types/vscode` exactly, languageclient's floor,
  and the Node version of that editor's Electron (for `@types/node` and esbuild's `target`). Read
  the Node version from `microsoft/vscode`'s `.npmrc` on the matching `release/*` branch.
- **`yarn test` runs the bundle (`dist/extension.js`).** `compile` empties `out/` first, so a deleted
  test can't keep passing.
