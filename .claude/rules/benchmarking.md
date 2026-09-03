---
paths:
  - "tests/**"
  - "src/main.rs"
  - "src/server/**"
---

# Benchmarking and manual smoke tests

Driving the binary against a real Ruby repository by hand, and which number matters for which
milestone. None of this is needed to edit the crate; it is how a claim about speed or degradation
gets checked.

**One of these runs is no longer manual.** `make canary` opens a pinned commit of a real Rails
application and asserts on own-code indexing, diagnostics and a ceiling on the cold index; CI runs
it on every push. It does *not* cover gems, because that needs a `bundle install` — so everything
below about gem directories, degraded Ruby versions and the steady resolve is still done by hand,
and is the part a green canary says nothing about. See `canary.md`.

Manual smoke test against a real Ruby repo — drives the binary over stdio and reports index time:

```bash
cargo build --release
python3 <driver> <repo> target/release/ya-lsp
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
0.4 ms. The worst case has to be built: point the workspace root at a whole gem directory —
whatever `gem env gemdir` names, then its `gems/` subdirectory — with a `ya-lsp.toml` that
disables gems and raises `index.max_files`, so every file in the tree counts as the user's own.
That run measured 17,557 files, and it is where `MAX_REFERENCES` is reached and where the linear
scaling is visible.

For M3, advertise `window.workDoneProgress`, **answer the server's
`window/workDoneProgress/create` request**, and wait for `$/progress` `end` before asserting that
a gem is navigable — it is background work, and a request racing it correctly answers `null`. To
test the "no Ruby" claim honestly, point `PATH` at an empty directory: macOS keeps a stub `ruby`
in `/usr/bin`. Drive against two real Rails apps: one whose bundle is fully installed — the
reference run was Rails 8.1 with 151 gems — and one whose Ruby version is *not* installed on the
machine, which is the degraded path.
