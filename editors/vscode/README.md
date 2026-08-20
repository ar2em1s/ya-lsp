# ya-lsp for VS Code

Ruby language support that does not need Ruby.

The extension bundles a `ya-lsp` binary for your platform. There is nothing to install, nothing to
add to your `Gemfile`, and no version manager to detect — it works on a machine where the
project's Ruby is not installed at all.

## What you get

Diagnostics, go-to-definition, hover, document symbols, workspace symbol search, find-references,
and completion — across your project **and its gems**, which are read straight from
`Gemfile.lock` and the gem directories on disk.

## How precise are the answers?

ya-lsp resolves constants and does not infer types, and that line runs through every feature.
Constants are exact. Methods are matched by name once the receiver is a local variable, which
means find-references on `name` returns every call spelled that way. `Foo.`, `self.` and a bare
call in a class body all resolve properly. The repository README goes into this in full.

## Settings

| Setting | What it does |
| --- | --- |
| `ya-lsp.serverPath` | Run a binary of your own instead of the bundled one. Takes `~` and `${workspaceFolder}`. |
| `ya-lsp.logLevel` | How much the server writes to its output channel. Changing it restarts the server. |
| `ya-lsp.gems.enabled` | Index the project's gems. On by default; it is most of the value. |
| `ya-lsp.gems.rubyVersion` | Override which Ruby's gems to index. |
| `ya-lsp.diagnostics.enabled` | Report problems found while indexing. |
| `ya-lsp.diagnostics.rules` | Per-rule severity, keyed by the rule name in the problem's code. |
| `ya-lsp.index.maxFiles` | Refuse to index a workspace larger than this. |

A committed `ya-lsp.toml` in the workspace root **overrides these**, so a team can agree on one
setup that works in every editor. Changing it takes effect without a restart.

## Commands

- **ya-lsp: Restart Server**
- **ya-lsp: Show Output**

## Multi-root workspaces

One server per folder, because everything a server does — the index, gem discovery, `ya-lsp.toml`
— is scoped to a single root. The first folder starts with the window; the rest start when you
open a Ruby file inside them.
