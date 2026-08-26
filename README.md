<div align="center">

# ya-lsp

**Y(et) A(nother) LSP** — a language server for Ruby that never runs Ruby.

[![CI](https://github.com/ar2em1s/ya-lsp/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/ar2em1s/ya-lsp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ar2em1s/ya-lsp)](https://github.com/ar2em1s/ya-lsp/releases/latest)
[![VS Marketplace](https://img.shields.io/badge/vs_marketplace-ar2em1s.yalsp-8A2BE2)](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp)
[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE.txt)

</div>

Standalone means what it says: ya-lsp never executes Ruby and never shells out to `ruby`, `bundle`
or `gem`. It reads `Gemfile.lock` and the gems on disk itself, so it works on a machine where the
project's Ruby is not installed. Written in Rust, no runtime dependencies, no `Gemfile` entry.

## Install

**VS Code** — [install from the Marketplace](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp).
The extension bundles a `ya-lsp` binary for your platform; there is nothing else to install. Point
`ya-lsp.serverPath` at your own binary to develop against a local build.

**Any other LSP client** — download the archive for your platform from the
[latest release](https://github.com/ar2em1s/ya-lsp/releases/latest), or build it with
`cargo build --release`. Either way, run `ya-lsp --stdio`.

```bash
tar xzf ya-lsp-aarch64-apple-darwin.tar.gz
./ya-lsp-aarch64-apple-darwin/ya-lsp --version
```

Archives rather than bare binaries, because a release asset downloaded over HTTP arrives without
its executable bit. `SHA256SUMS` covers every asset; `sha256sum -c SHA256SUMS` checks whichever
ones you took. `ya-lsp --licenses` prints ya-lsp's terms, the notice for the Ruby signatures
embedded in the binary, and the licences of every crate linked into it — from inside the binary,
so a copy that travels alone still carries what it owes.

## Features

| | |
| --- | --- |
| **Diagnostics** | Parse errors and Prism's warnings, live as you type. Per-rule severity. |
| **Go to definition** | Constants, methods, and the path in a `require "..."`. |
| **Hover** | Signature and documentation, including for core and stdlib. |
| **Find references** | Exact for constants; name-based for methods. Your own code only. |
| **Document symbols** | The file's outline, nested. |
| **Workspace symbols** | Fuzzy search across the project, its gems, core and stdlib. |
| **Completion** | Ancestor-aware, visibility-aware, with keyword arguments. See below. |
| **Gem indexing** | Every gem in `Gemfile.lock`, read from disk, with progress reported. |
| **Core & stdlib** | `String`, `Array`, `Hash`, `Kernel`, plus `CSV`, `URI`, `Logger` and ~57 more. |
| **Multi-root** | One server per folder; a folder with no Ruby in it never gets one. |

Which Ruby's library to index is read from `.ruby-version` or `.tool-versions` — in the project or
any directory above it, the way rbenv, chruby, RVM, asdf and mise all resolve it — falling back to
`RUBY VERSION` in `Gemfile.lock`. ya-lsp refuses to guess when none of them answer, and says so
rather than silently indexing nothing: guessing once put macOS's vestigial Ruby 2.6 into the index.
Core and stdlib signatures come from the `rbs` gem when the machine has one and from a copy inside
the binary when it does not.

## How precise are the answers?

ya-lsp resolves constants and does not infer types. That line runs through every feature, so it is
worth stating rather than discovering:

- **Constants are exact.** Classes, modules and `CONSTANT` names resolve through a real graph, not
  matching text. `Person` inside `module HR` and top-level `HR::Person` are understood to be one
  constant; a different top-level `Person` is understood not to be.
- **Methods are exact when the receiver can be named.** `Foo.bar`, `self.bar`, a literal like
  `"hello".upcase`, `Foo.new.bar`, and calls in a class body all resolve properly.
- **Otherwise they are matched by name.** For `person.name`, where `person` came from anywhere but
  a literal, there is nothing to resolve against — so find-references returns every call spelled
  that way in your project. For an unusual name that is the answer you wanted; for `call`, `name`
  or `id` it is a scoped text search with a nicer interface, and ya-lsp does not pretend otherwise.
- **Find-references never looks inside gems.** A bundle spells `name` tens of thousands of times
  and none are an answer; go-to-definition and symbol search do reach in, because there the answer
  is useful even when you cannot change it.

Closing the gap means inferring the type of every expression, which is a different project. If you
need that today, solargraph does it.

### Completion

`Foo::`, `Foo.`, `self.` and a bare word are exact: ya-lsp walks the real ancestor chain and
applies real visibility, so `private` methods are offered inside their class and nowhere else, and
`Foo.` offers class methods rather than instance ones. Inside a call's parentheses you also get the
keyword arguments that method actually takes — only when it resolved exactly, because a wrong
keyword argument is a syntactically valid wrong answer.

Literals are exact too. `"hello".`, `[1, 2].`, `{}.`, `42.`, `:name.` and `Foo.new.` are typed from
what the parser already decided, so they reach the real class. A local assigned one of those
carries the type along — `greeting = "hello"` then `greeting.` knows. That is the one place ya-lsp
can be wrong rather than merely absent: reassign inside a branch and it answers with whichever
assignment is textually last.

Everything else after a `.` is a guess: every method name in your project, deduplicated, your own
code first. Less than type inference gives you, more than an editor without it — your editor's
suggestions cannot see a method defined in a file you do not have open.

Two things no static tool sees: **Rails' `validates`, `has_many` and `scope`**, installed at
runtime by `ActiveSupport::Concern` and present in no source file (the ~125 class methods
`ActiveRecord::Base` does write down are offered normally), and **`define_method`**. Completion
never fires inside a comment or a string, but does inside `#{}`, because that is code.

## Configuration

A `ya-lsp.toml` at the workspace root overrides whatever the editor sends, so a team can commit one
setup that works everywhere. Changes take effect without a restart, in every editor. Defaults:

```toml
[index]
include           = ["**/*.rb"]
exclude           = ["vendor/**/*", ".bundle/**/*", "tmp/**/*", "node_modules/**/*"]
load_paths        = ["lib", "app"]        # extra roots, also used to resolve `require`
max_files         = 50000
respect_gitignore = true

[gems]
enabled      = true
default_gems = true                       # the ~40 gems shipped inside Ruby itself
# ruby_version = "3.4.1"                  # unset: read from .ruby-version / .tool-versions
paths        = []                         # extra gem roots
max_files    = 300000

[rbs]
enabled = true
stdlib  = true                            # the ~60 stdlib libraries beyond core
# path  = "/path/to/rbs"                  # unset: the rbs gem, else the copy in the binary

[diagnostics]
enabled = true
rules   = {}                              # e.g. { parse-warning = "error" }
```

Rules are keyed by the name in a diagnostic's `code`. Only `parse-error` and `parse-warning` are
statements about your code and are on by default; the rest describe indexer limits, fire on legal
Ruby, and ship `off` or `hint`.

VS Code exposes the common settings under `ya-lsp.*` — `serverPath`, `logLevel`, `gems.enabled`,
`gems.defaultGems`, `gems.rubyVersion`, `rbs.enabled`, `rbs.stdlib`, `rbs.path`,
`diagnostics.enabled`, `diagnostics.rules`, `index.maxFiles` — plus the commands **ya-lsp: Restart
Server** and **ya-lsp: Show Output**. A committed `ya-lsp.toml` wins over all of them.

## Development

`make` on its own lists every target. `make setup` installs the two pinned cargo subcommands,
`make ci` runs what CI runs — format, clippy, tests, and a coverage gate at 95% of lines and
branches. Rust 1.93.1, pinned in `.tool-versions`.

## Licence

MIT — see [LICENSE.txt](LICENSE.txt). ya-lsp embeds Ruby's own RBS signatures, which are
BSD-2-Clause/Ruby; [NOTICE.txt](NOTICE.txt) carries their notice and
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) the licences of every crate linked in.
