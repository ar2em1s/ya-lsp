<div align="center">

# ya-lsp

**Y(et) A(nother) LSP** — a language server for Ruby that never runs Ruby.

[![CI](https://github.com/ar2em1s/ya-lsp/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/ar2em1s/ya-lsp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ar2em1s/ya-lsp)](https://github.com/ar2em1s/ya-lsp/releases/latest)
[![VS Marketplace](https://img.shields.io/badge/vs_marketplace-ar2em1s.yalsp-8A2BE2)](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp)
[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE.txt)

</div>

ya-lsp parses your Ruby, reads `Gemfile.lock` and the gems on disk, and answers from a graph it
builds itself. It never executes Ruby and never shells out to `ruby`, `bundle` or `gem`. One Rust
binary: no `Gemfile` entry, no runtime dependency, nothing to boot.

## Why try it

- **It starts where a server that has to load your application cannot.** A legacy app on a Ruby you
  may not install, a locked bundle you may not add a gem to, an air-gapped box, an editor that
  should not need a version manager configured first. No `bundle install`, no added gem: a download.
- **It reads Rails without running Rails.** Your columns, associations, enums, routes, mailers and
  jobs all resolve, because the schema and the macros are read as the text they are.
- **Every answer says how far to trust it.** Each hover card and completion row is labelled
  *Resolved*, *Derived* or *Guessed* — and the guess can be turned off entirely.
- **Nothing to keep warm.** There is no cache on disk, so there is nothing to invalidate, prune or
  explain when it goes stale. A `git checkout` or a generator re-indexes what changed.

**What it costs:** no formatting and no RuboCop — both are Ruby. Run
[RuboCop's own language server alongside](#running-rubocop-alongside); that closes both gaps.

## Install

**VS Code** — [install from the Marketplace](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp).
The extension bundles a binary for your platform. Set `ya-lsp.serverPath` to use a local build.

**Any other LSP client** — download from the
[latest release](https://github.com/ar2em1s/ya-lsp/releases/latest) or `cargo build --release`,
then run `ya-lsp --stdio`.

```bash
tar xzf ya-lsp-aarch64-apple-darwin.tar.gz
./ya-lsp-aarch64-apple-darwin/ya-lsp --version
```

Releases are archives, not bare binaries: an asset downloaded over HTTP loses its executable bit.
`SHA256SUMS` covers every asset. `ya-lsp --licenses` prints ya-lsp's terms, the notice for the
embedded Ruby signatures, and every linked crate's licence — from inside the binary.

## What you get

| | |
| --- | --- |
| **Diagnostics** | Parse errors and Prism warnings, live. Per-rule severity. |
| **Go to definition** | Constants, methods, the path in `require "..."` — into gems too. |
| **Hover** | Signature and documentation, plus where the type came from. |
| **Completion** | Ancestor-aware, visibility-aware; keyword arguments and model columns. |
| **Signature help** | Parameters of the call you are in, current one marked. |
| **Find references** | Exact for constants, name-based for methods. Your code only. |
| **Highlight occurrences** | Same name in the file, reads and writes marked apart. |
| **Symbols** | The file's outline, and fuzzy search over project, gems, core and stdlib. |
| **Type hierarchy** | Both directions — every ancestor, and everything below. |
| **Rename** | Locals, parameters, constants. Refuses what it cannot do exactly. |
| **Refactorings** | Extract variable/method, toggle block style, declare `attr_`. |
| **Folding & expand selection** | Out through Ruby's own structure. |
| **Semantic highlighting** | The one thing a grammar cannot decide: local or call? |
| **ERB templates** | `app/views/**/*.erb` indexed like any other file. |
| **Rails awareness** | Schema, model macros, routes, mailers, jobs, engines. |
| **Gems, core & stdlib** | Every gem in `Gemfile.lock`, from disk, with progress. |
| **Multi-root** | One server per folder; a folder with no Ruby gets none. |

## Every answer is labelled

Three tiers, and **every hover card and completion row says which one it is**:

| Tier | Source | Example |
| --- | --- | --- |
| **Resolved** | The code names the type | `Foo.bar`, `self.bar`, `"x".upcase`, `Foo.new.bar` |
| **Derived** | A signature, an assignment, or a convention — the card names it | `"x".upcase.strip`, `@title.upcase`, a view's `@story` |
| **Guessed** | The receiver's name alone | `@user` → `User`, `first_name` → `FirstName` |

Guessed is the only tier allowed to be wrong, and it never displaces the other two. Turn it off
with `[types] guess_from_names = false`.

Under a receiver ya-lsp tries, in this order: the graph naming the type; a signature or an
assignment; the controller a template's path names; the receiver's own spelling; then the
name-based list. A rung ships only if it moves calls **up** a tier and moves none down.

Types come from RBS — Ruby's own signatures are embedded in the binary, and a gem's `sig/`, your
project's `sig/` and `.gem_rbs_collection/` are read where they exist. Sorbet `sig` blocks and YARD
`@return` tags are read too, and declined rather than approximated where they say something RBS
cannot.

## Rails, without a Rails process

No `rails runner`, no eager load, no initializers. Every fact below is read as the literal Ruby or
SQL it is and indexed like any other signature.

- **Your columns.** `db/schema.rb` or `db/structure.sql`, every database and not just the primary,
  so `story.created_at.` chains and the card names the table, the column and whether it is nullable.
- **Model macros.** Associations, `enum`, `attribute`, `delegate`, `scope` and a couple of dozen
  more — `class_attribute`, `has_secure_password`, `store_accessor`, `alias_attribute` and the rest
  — declare the methods Rails would declare, including the ones a concern's `included do` lands on
  every including class. Each jumps to the macro line you wrote.
- **The query interface**, taken from Rails' own list, so `Story.where(...).first.user.email`
  resolves end to end. How you wrote the call decides the type: `Story.first` is a `Story`,
  `Story.first(3)` an array.
- **Routes.** Every helper `config/routes.rb` names, hovering with and jumping to the DSL line.
- **Mailers, jobs and Sidekiq workers.** `UserMailer.welcome(user)`, `perform_later`,
  `perform_async` — each resolved to the `def` it ends up calling.
- **Engines in your bundle.** A gem's `app/` is read too, so `ActiveStorage::Blob` resolves.
- **Templates.** A cursor inside `<% %>` gets the same answers a `.rb` file would, and `@story` in
  `app/views/stories/show.html.erb` is typed from what `StoriesController` assigns.

## Where it stops

- **A method call whose receiver cannot be named is matched by name.** Find-references then returns
  every call spelled that way in your project — the right answer for an unusual name, a scoped text
  search for `call` or `id`. Signature help and keyword-argument completion stay **silent** there
  rather than answer from whichever class happened to match.
- **Find-references never enters gems**, where a common name appears tens of thousands of times.
  Definition and symbol search do, because a read-only answer is still worth having.
- **Rename declines rather than degrades.** Methods and instance variables are refused, with the
  reason on screen, because neither can be found exactly. Nothing is written until every site has
  been read back: one unconfirmable site declines the whole rename.
- **Chains through an untyped method end there.** Ruby's own library declares its return types;
  most applications do not, and inferring the type of every expression is a different project.
- **Runtime metaprogramming is invisible** — `define_method`, an `include` from a variable, a
  `Class.new` nothing names.
- **Not included:** formatting, RuboCop, quick fixes, inlay hints, code lenses, test running, a
  debugger, a plugin API.

## Setup details

**Which Ruby.** Read from `.ruby-version` or `.tool-versions`, in the project or any directory
above it — how rbenv, chruby, RVM, asdf and mise all resolve it — falling back to `RUBY VERSION` in
`Gemfile.lock`. If none answer, ya-lsp says so instead of guessing.

**Diagnostics** are Prism's parse errors and warnings. The other rules ship `off` or `hint`,
because they fire on correct Ruby. ERB templates publish none: a template's Ruby does not parse
standalone, since `<%= yield %>` is legal in the method it compiles to.

**File watching.** A `git checkout`, `git pull`, rebase or `rails g model` re-indexes exactly the
files it touched, deletions included, with no restart. Needs a client that accepts a file watcher
registration; ya-lsp logs when one does not, because being blind looks like being wrong.

## Running RuboCop alongside

ya-lsp reports parse errors and stops, because every cop is Ruby. LSP allows more than one server
per language, and RuboCop ships one: `rubocop --lsp`, over stdio, from the gem you already lint
with. **Run both** and you have formatting, offences and per-offence quick fixes as well.

**Quick fixes need RuboCop 1.89 or newer** — that is the version where its server started
advertising `codeActionProvider`.

**Turn off `parse-warning` when you do this.** Prism's warnings and `Lint/UselessAssignment` cover
the same ground, so "assigned but unused variable" arrives twice. Switch off the rule, not the
category, so a file that does not parse still gets its squiggle:

```toml
# ya-lsp.toml
[diagnostics.rules]
parse-warning = "off"
```

**VS Code** — install [RuboCop](https://marketplace.visualstudio.com/items?itemName=rubocop.vscode-rubocop),
published by the RuboCop team. Nothing to configure: ya-lsp advertises no formatting capability,
the two extensions own separate diagnostic collections, and the lightbulbs merge because ya-lsp
advertises only `refactor.extract` and `refactor.rewrite` while RuboCop advertises only `quickfix`.
ya-lsp offers this once in a project with a `.rubocop.yml` or `rubocop` in `Gemfile.lock`;
`ya-lsp.rubocop.hint` turns the offer off.

**Neovim** (0.11+):

```lua
vim.lsp.config('ya_lsp', {
  cmd = { 'ya-lsp', '--stdio' },
  filetypes = { 'ruby', 'eruby' },
  root_markers = { 'ya-lsp.toml', 'Gemfile', '.git' },
})
vim.lsp.config('rubocop', {
  cmd = { 'rubocop', '--lsp' },
  filetypes = { 'ruby' },
  root_markers = { '.rubocop.yml', 'Gemfile', '.git' },
})
vim.lsp.enable({ 'ya_lsp', 'rubocop' })
```

`nvim-lspconfig` ships the `rubocop` half already.

**Helix** (`languages.toml`):

```toml
[language-server.ya-lsp]
command = "ya-lsp"
args = ["--stdio"]

[language-server.rubocop]
command = "rubocop"
args = ["--lsp"]

[[language]]
name = "ruby"
language-servers = ["ya-lsp", "rubocop"]
```

**Zed** ships `rubocop` and enables it by name:

```json
{ "languages": { "Ruby": { "language_servers": ["rubocop", "..."] } } }
```

ya-lsp is not in that list: Zed runs language servers from extensions, and there is no ya-lsp
extension for it yet.

**Any other client** — point one server at `ya-lsp --stdio` and another at `rubocop --lsp`.
Neither needs telling about the other; they share no state and claim no capability in common.

## Configuration

A `ya-lsp.toml` at the workspace root overrides whatever the editor sends, so a team can commit one
setup. Changes take effect without a restart. Defaults:

```toml
[index]
include           = [
  "**/*.rb", "**/*.erb", "**/*.rbs", "**/*.rake",
  "**/*.gemspec", "**/Rakefile", "**/Gemfile", "**/config.ru",
]
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

[types]
guess_from_names = true                   # `@user` is a `User`; always labelled as a guess

[diagnostics]
enabled = true
rules   = {}                              # e.g. { parse-warning = "error" }
```

Rules are keyed by the name in a diagnostic's `code`. Only `parse-error` and `parse-warning` are
statements about your code and are on by default; the rest describe indexer limits.

VS Code exposes all of these except `gems.max_files`, with the TOML key in each description. Two
settings have no TOML twin: `ya-lsp.serverPath` and `ya-lsp.logLevel`, both of which restart the
server. Commands: **ya-lsp: Restart Server**, **ya-lsp: Show Output**. A committed `ya-lsp.toml`
wins over all of them.

## Development

`make` lists every target. `make setup` installs the two pinned cargo subcommands; `make ci` runs
what CI runs — format, clippy, tests and the coverage gate. Rust 1.93.1, pinned in `.tool-versions`.

`make canary` is the one target needing a network: it fetches a pinned commit of a real,
open-source Rails application, opens it the way an editor does, and checks that the expected files
index, that none fails to parse, and that the cold index stays inside a generous ceiling. It is a
canary, not a benchmark, and does not cover gems. CI runs it as its own job.

## Licence

MIT — see [LICENSE.txt](LICENSE.txt). ya-lsp embeds Ruby's own RBS signatures, which are
BSD-2-Clause/Ruby; [NOTICE.txt](NOTICE.txt) carries their notice and
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) every linked crate's licence.
