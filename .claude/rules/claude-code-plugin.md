---
paths:
  - "editors/claude-code/**"
  - ".claude-plugin/**"
  - "tests/claude_plugin.rs"
---

# Claude Code plugin

## What it is

- `editors/claude-code/.claude-plugin/plugin.json`: the manifest `/plugin` shows.
- `editors/claude-code/.lsp.json`: all the behaviour.
- `editors/claude-code/skills/ya-lsp-setup/SKILL.md`: the setup skill, found by its location alone.
- `.claude-plugin/marketplace.json`: how it installs from this repo.
- `.claude/settings.json` points the marketplace at `"path": "."`. `${CLAUDE_PROJECT_DIR}` is not
  expanded there.

## Must

1. **Never commit a binary.** `.lsp.json` cannot choose a command per platform. The command is a
   bare `ya-lsp` on `PATH`. `tests/claude_plugin.rs` asserts that nothing here is executable.
2. **Add no `userConfig` option for the server path.** Claude Code's LSP path ignores option
   defaults and fails with `lsp-config-invalid`.
3. **Add no `initializationOptions`.** Users configure through `ya-lsp.toml`. If one is ever added,
   it must deserialize as `PartialConfig`, which the test checks.
4. **Keep `plugin.json`'s version equal to the crate's.** The test holds it.
5. **Every `index.include` glob must name an extension, not a file name.** Claude Code routes by
   extension only, so `**/*.ru`, not `**/config.ru`. `tests/claude_plugin.rs` holds both sides.

## Routing: `extensionToLanguage`

- **It is the only gate, and it is narrower than the index.** `.rbs` is deliberately left off.
  `Rakefile` and `Gemfile` have no extension and cannot be routed at all.
- `.jbuilder`, `.builder` and `.ruby` are routed as `ruby`. The skill lists every routed extension
  (`the_skill_names_the_extensions_the_plugin_routes`).
- **Two plugins that map `.rb`:** the first one loaded wins silently (`lsp-extension-conflict`). The
  docs say to disable the other one. Which plugin loads first has not been investigated, so write
  no rule about it.

## What Claude Code does and doesn't do

- **It drops every sentence the server writes** (`showMessage`, `logMessage`, progress) and sends no
  `didChangeWatchedFiles`.
- Its own `Write` and `Edit` send `didChange` + `didSave`. A Bash edit sends nothing.
- It opens a document on first use and evicts past 50.
- **It hides answers located in git-ignored files.** ya-lsp itself reads no ignore files
  (`gems.md`). A vendored bundle is still indexed, through the gem pass; Claude Code just won't show
  locations inside it.

## The setup skill

- **It diagnoses by asking the server, never by reading what the server writes.** It makes three
  `LSP` calls: `documentSymbol`, `workspaceSymbol`, then `goToDefinition` on a constant the project
  names but doesn't declare (the missing-bundle symptom). `workspaceSymbol` still needs a file,
  line and character to choose the server.
- **Its `description` names symptoms** (nothing on `PATH`, an empty jump into a gem), because that
  is all a model sees before loading the skill.
- **`tests/claude_plugin.rs` checks four things:**
  - the name matches its directory
  - every TOML block loads, with a mistyped-key guard
  - the archives named match the release matrix, both ways
  - every routed extension is listed
- **The facts its steps are ordered around:**
  - An empty `GEM_HOME` hides nothing. An empty `ASDF_DATA_DIR` hides the whole bundle.
  - Most "no server" cases are a binary that exists but is off `PATH` (an `export` in `~/.zshrc`).
  - A `ya-lsp` symlink is probably a developer's build. Print its target before offering to
    replace it.
- **Test an update with a real older release archive**, never a fake `--version` script. The agent
  reads the script, and a fake that exits hangs Claude Code.

## How to check it

1. **Run headless, from inside the corpus**, never from this repo (the default include drops
   `tmp/**`):
   ```bash
   cd tmp/corpora/<name> && PATH="$REPO/target/release:$PATH" \
     claude -p --plugin-dir "$REPO/editors/claude-code" --allowedTools LSP \
     --output-format stream-json --verbose "hover on <file> line <n> column <m>; print the raw result"
   ```
   These runs are pre-approved.
2. **In an interactive session, after a rebuild, run `pkill -f 'ya-lsp --stdio'`.** The next request
   starts a fresh server. `/reload-plugins` is needed only when a JSON file changes. A cold
   marketplace needs two reloads.
