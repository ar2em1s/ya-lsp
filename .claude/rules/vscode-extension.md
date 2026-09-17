---
paths:
  - "editors/vscode/**"
---

# VS Code extension

- **`config.ts`, `server.ts` and `claims.ts` must never import `vscode`.** That is the only reason
  they are testable — there is no extension host here, and they hold the mistakes that are invisible
  from inside one: the platform-specific paths, the settings translation, and which of several
  servers answers about a file no folder holds. Anything needing the editor goes in `extension.ts`, tested through the
  bundle-loading smoke test.
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
- **The document selector is the only gate, and every client's is its own folder.**
  `LanguageClientOptions.workspaceFolder` sets the `rootUri` and nothing else — it does not filter
  documents — so the selector alone decides which files are ever `didOpen`ed and asked about.
  Everything else scores 0 in `languages.match` and the server is never told the document exists.
  A gem's source, Ruby's stdlib and the RBS beside them live outside *every* workspace folder, and
  the server indexes and answers about all three; it shipped claiming only the folder and presented
  in the worst form available: `definition` worked, jumped into
  `activerecord-8.1.3.1/lib/active_record.rb`, and every request in the file it had just opened was
  dead — nothing logged, because no request was ever sent. **The extension does not fix that by
  guessing where the gems are.** Doing so means a second copy of `workspace/gems.rs` in TypeScript —
  the bundle parse, `require_paths`, the vendored-versus-installed RBS choice, an engine's `app/` —
  which would drift in the one direction nothing reports, since an unclaimed file produces silence
  rather than an error. So the server names its own roots after the handshake, over
  `client/registerCapability`, and the selector built here stays the folder and only the folder,
  whatever the workspace looks like. Nothing varies with the number of folders, which is why no
  folder count restarts anything.
- **`claims.ts` decides which server answers about a root, because it is the only place that sees
  them all.** Two folders on one Ruby resolve to the same gem roots, both servers register them, and
  two providers over one document is one hover card printed twice with no way to tell which server
  wrote either half. First asker wins, through `middleware.handleRegisterCapability`; a root only one
  bundle resolved to is claimed by that bundle, and a registration whose selector empties is dropped
  rather than forwarded empty — an empty array is not nullish, so the client would keep a provider
  that can never match. **A root inside another workspace folder is refused outright**, before
  first-asker-wins applies: that folder's client already claims it by selector, so granting it puts
  two providers over one file from the registration side — the one direction `claimedByNestedFolder`
  cannot see. Which folder "another" means is `innermostFolder`, shared with the request-side
  narrowing so the two cannot disagree; a vendored bundle inside a *nested* folder belongs to that
  folder and not to the one above it, and a first-match rule would have handed it to whichever folder
  the `.code-workspace` listed first. A registration the server did not make under `ya-lsp-documents/` passes
  through untouched, which is what the file watcher's
  registration needs. When a client stops it gives its roots
  up, and `onFoldersChanged` rebuilds exactly the clients that had asked for one nobody owns any
  more — a selector cannot be changed after construction, so a client that lost a root the first
  time cannot pick it up later.
- **A workspace folder may contain another one, and the selector cannot say so.** VS Code allows
  it — a monorepo listing the repository for the code beside the apps and each app for its own
  `Gemfile.lock` — and `getWorkspaceFolder` resolves a file in the inner folder to the *innermost*
  one. An LSP glob has `*`, `**`, `?`, `{}` and `[]` and **no way to subtract a path**, so "under
  the parent but not under the child" is unsayable: the outer folder's client claims the inner
  folder's files along with its own, both clients match, and every request is answered twice — a
  hover card printed twice, every completion item doubled, one set of squiggles over another.
  Nesting cannot simply be refused, because each folder may hold the only `Gemfile.lock` for its
  own code and a server resolves exactly one bundle. So the outer client is narrowed where it
  sends, in `narrowing()`: `GeneralMiddleware.sendRequest` covers every request with one hook, and
  the four text-sync hooks keep the outer server from being handed the buffer at all. The predicate
  is `claimedByNestedFolder`, beside `Claims` because it is the same question from the other side —
  that one arbitrates a file *no* folder holds, this one a file *two* folders hold. **`index.exclude`
  does not reach this**: the server indexes an opened buffer whatever its walk collected, so an
  excluded file still gets a hover. The wiring is pinned in `activation.test.ts` rather than only
  the predicate, because what is left to get wrong is reading the document from the wrong place in
  the parameters or passing the two strings in the wrong order — both silent, and failing in
  opposite directions.
- **The pattern is the protocol's relative-pattern shape, never `vscode.RelativePattern`.**
  `vscode-languageclient` 10 runs every selector through `asDocumentSelector`, whose
  `asGlobPattern` recognises exactly two things: a plain string, and LSP 3.18's
  `{ baseUri, pattern }` where `baseUri` is a **URI string**. Everything else becomes `undefined`,
  including a `vscode.RelativePattern`, whose `baseUri` is a `Uri` object and fails `URI.is`. An
  undefined pattern does not narrow, it *widens*: `languages.match` then scores on language and
  scheme alone, so every folder's client would claim every folder's Ruby files — and so would every
  registration the server sends. `activation.test.ts` pins the conversion and the containment that
  follows from it, with an explicit guard so neither can pass vacuously.
- **`engines.vscode` decides three other versions, and they are not independent.** `@types/vscode`
  is pinned *exactly* to it — a higher one compiles against APIs the oldest supported editor lacks
  and fails at a user's runtime rather than in CI. `vscode-languageclient` has its own floor (10.x
  wants 1.91). The Node that `@types/node` and esbuild's `target` describe is the editor's Electron,
  not `.tool-versions`: 1.108 is Electron 39.2.7, which is Node 22.21. Derive it from
  `microsoft/vscode`'s `.npmrc` at the matching `release/*` branch rather than guessing.
- **`yarn test` runs the bundle, not the sources.** `dist/extension.js` is what ships, and the
  failures worth catching — a dropped import, a command declared but never registered — only exist
  there. `compile` empties `out/` before `tsc` writes it, because `tsc` does not: a deleted test file
  left a compiled copy behind and `node --test "out/**/*.test.js"` went on running it green, against
  a module the repository no longer had.
- **Anything the server reads once at startup needs a restart, not a notification.** `logLevel` is
  `EnvFilter::try_from_env`; `serverPath` decided which process was spawned. `RESTART_REQUIRED` is
  the list, and the extension performs the restart rather than leaving the setting inert.
- **The extension supplies no file watcher, and must not start.** The server registers its own
  `ya-lsp.toml` watcher through `client/registerCapability`, which is what makes reload work in
  editors with no extension, and `vscode-languageclient` installs that registration itself. A
  watcher passed through `synchronize.fileEvents` as well is a second watcher on the same file — two
  `didChangeWatchedFiles` per save, each dropping the whole graph and re-running the gem index. It
  also belonged to the caller: the client never disposed it, so every restart leaked one.
