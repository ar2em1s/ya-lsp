# ya-lsp

*Y(et) A(nother) LSP.*

**A language server for Ruby that never runs Ruby.** It parses your code, reads `Gemfile.lock` and
the gems on disk, and answers from a graph it builds itself — so go-to-definition, hover, completion
and 21 more LSP requests work on a machine where the project's Ruby is not installed, the bundle is
not installed, and nothing has to boot. One Rust binary: no `Gemfile` entry, no runtime dependency,
no `bundle exec`.

**What makes it different from every other Ruby server: each answer tells you how far to trust it.**
A Ruby tool that cannot run your code has to guess sometimes, and the usual arrangement is that you
find out which answers were guesses by being wrong. Here every hover card and completion row is
labelled *Resolved*, *Derived* or *Guessed* — and the guessing can be switched off entirely.

- **No Ruby, at all.** Never executes `ruby`, `bundle` or `gem`, and never shells out. A legacy app
  on a Ruby you may not install, a locked bundle you may not add a gem to, an air-gapped box, a CI
  container: it is a download and nothing else.
- **Fast from cold.** A pinned commit of a real Rails application — **606 files** — indexes in
  **under 50 ms**, re-measured on every CI run against a 500 ms ceiling the build fails at. Its gems
  are indexed after it, with progress reported to your editor.
- **Nothing to keep warm.** There is **no cache on disk**, so there is nothing to invalidate, prune,
  or explain when it goes stale. A `git checkout` or `rails g model` re-indexes what changed.
- **It reads Rails without running Rails.** Your columns, associations, `enum`s, routes, mailers,
  jobs and engines all resolve, because the schema and the macros are read as the text they are.
- **Types come from RBS.** Ruby's own signatures are embedded in the binary; a gem's `sig/`, your
  project's `sig/` and `.gem_rbs_collection/` are read where they exist, and a Sorbet `sig` or a
  YARD `@return` is read too.

Hovering `title` in a Rails project, with no Ruby process anywhere:

```ruby
story = Story.where(published: true).first
story.title
#     ↑ Story#title
#
#       From `db/schema.rb`, table `stories`, column `title` (`string`, `null: false`).
```

That last line is the part to notice: ya-lsp read `db/schema.rb`, so it knows the column exists, what
it returns, and that it can never be `nil`.

**What it costs:** no formatting and no RuboCop — both are Ruby.
[Run RuboCop's own language server alongside](#running-rubocop-alongside); that closes both gaps.

---

- [Install](#install)
- [What you get](#what-you-get)
- [Every answer is labelled](#every-answer-is-labelled)
- [Rails, without a Rails process](#rails-without-a-rails-process)
- [Where it stops](#where-it-stops)
- [Running RuboCop alongside](#running-rubocop-alongside)
- [Configuration](#configuration)
- [Development](#development)

## Install

### VS Code

1. Install **[ya-lsp from the Marketplace](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp)**.
2. Open a Ruby file.

That is the whole setup. The extension bundles a binary for your platform. `ya-lsp.serverPath` points
it at a local build instead.

### Any other LSP client

1. Download the archive for your platform from the
   [latest release](https://github.com/ar2em1s/ya-lsp/releases/latest) — or build it with
   `cargo build --release`.
2. Extract it and check it runs:

   ```bash
   tar xzf ya-lsp-aarch64-apple-darwin.tar.gz
   ./ya-lsp-aarch64-apple-darwin/ya-lsp --version
   ```

3. Put `ya-lsp` on your `PATH`, and point your client at `ya-lsp --stdio`.

Releases are archives rather than bare binaries, because an asset downloaded over HTTP loses its
executable bit. `SHA256SUMS` covers every asset. `ya-lsp --licenses` prints ya-lsp's own terms, the
notice for the embedded Ruby signatures, and every linked crate's licence, from inside the binary.

**Neovim** (0.11+), in `init.lua`:

```lua
vim.lsp.config('ya_lsp', {
  cmd = { 'ya-lsp', '--stdio' },
  filetypes = { 'ruby', 'eruby' },
  root_markers = { 'ya-lsp.toml', 'Gemfile', '.git' },
})
vim.lsp.enable({ 'ya_lsp' })
```

**Helix**, in `languages.toml`:

```toml
[language-server.ya-lsp]
command = "ya-lsp"
args = ["--stdio"]

[[language]]
name = "ruby"
language-servers = ["ya-lsp"]
```

**Zed** runs language servers from extensions and there is no ya-lsp extension for it yet.

### Which Ruby it reads

From `.ruby-version` or `.tool-versions`, in the project or any directory above it — how rbenv,
chruby, RVM, asdf and mise all resolve it — falling back to `RUBY VERSION` in `Gemfile.lock`. If
none of them answer, ya-lsp says so instead of guessing.

## What you get

| | |
| --- | --- |
| **Diagnostics** | Parse errors and Prism warnings, live. Per-rule severity. |
| **Go to definition** | Constants, methods, the path in `require "..."` — into gems too. |
| **Document links** | Every `require` path is a link. One that resolves nowhere is not. |
| **Hover** | Signature and documentation, plus where the type came from. |
| **Completion** | Ancestor-aware, visibility-aware; keyword arguments and model columns. |
| **Signature help** | Parameters of the call you are in, current one marked. |
| **Inlay hints** | Types nothing in the line says. A guessed one is never drawn. |
| **Find references** | Exact for constants, name-based for methods. Your code only. |
| **Highlight occurrences** | Same name in the file, reads and writes marked apart. |
| **Symbols** | The file's outline, and fuzzy search over project, gems, core and stdlib. |
| **Type hierarchy** | Both directions — every ancestor, and everything below. |
| **Call hierarchy** | Who calls this, and what it calls. Callers are by name and say so. |
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
| **Derived** | A signature, an assignment, or a convention — the card names it | `"x".upcase.strip`, `@title.upcase`, `ENV.fetch`, a `CONFIG = Settings.new`, a view's `@story` |
| **Guessed** | The receiver's name alone | `@user` → `User`, `first_name` → `FirstName` |

Guessed is the only tier allowed to be wrong, it never displaces the other two, and it is never
painted into a margin as an inlay hint. Turn it off with `[types] guess_from_names = false`.

Under a receiver ya-lsp tries, in this order: the graph naming the type; a signature or an
assignment; the class a template's path names, a controller or a mailer; the receiver's own
spelling; then the name-based list. A rung ships only if it moves calls **up** a tier and moves none
down.

## Rails, without a Rails process

No `rails runner`, no eager load, no initializers. Every fact below is read as the literal Ruby or
SQL it is and indexed like any other signature.

- **Your columns.** `db/schema.rb` or `db/structure.sql`, every database and not just the primary,
  so `story.created_at.` chains and the card names the table, the column and whether it is nullable.
- **Model macros.** Associations, `enum`, `attribute`, `delegate`, `scope` and a couple of dozen
  more — `class_attribute`, `has_secure_password`, `store_accessor`, `alias_attribute` and the rest
  — declare the methods Rails would declare, including the ones a concern's `included do` and
  `class_methods do` land on every including class. Each jumps to the macro line you wrote.
- **The query interface**, taken from Rails' own list, so `Story.where(...).first.user.email`
  resolves end to end. How you wrote the call decides the type: `Story.first` is a `Story`,
  `Story.first(3)` an array. `Story.where` jumps into activerecord, where it is really declared.
- **Routes.** Every helper `config/routes.rb` names, hovering with and jumping to the DSL line.
- **Mailers, jobs and Sidekiq workers.** `UserMailer.welcome(user)`, `perform_later`,
  `perform_async` — each resolved to the `def` it ends up calling.
- **Engines in your bundle.** A gem's `app/` is read too, so `ActiveStorage::Blob` resolves.
- **Templates.** A cursor inside `<% %>` gets the same answers a `.rb` file would; `@story` in
  `app/views/stories/show.html.erb` is typed from what `StoriesController` assigns, and jumps to the
  line that assigns it.
- **Macro symbols.** `before_action :authenticate`, `validates :title`, `belongs_to :user` — the
  symbol is a member of the class the macro is written in, and resolves like one.

## Where it stops

- **A method call whose receiver cannot be named is matched by name.** Find-references then returns
  every call spelled that way in your project — the right answer for an unusual name, a scoped text
  search for `call` or `id`. Signature help and keyword-argument completion stay **silent** there
  rather than answer from whichever class happened to match.
- **An untyped receiver is offered a bounded list or none.** With nothing typed after the `.`, the
  candidate list is every method in the graph, so ya-lsp offers nothing at all; it reappears,
  capped at 128, once the word is long enough to narrow it.
- **Find-references never enters gems**, where a common name appears tens of thousands of times.
  Definition and symbol search do, because a read-only answer is still worth having.
- **Rename declines rather than degrades.** Methods and instance variables are refused, with the
  reason on screen, because neither can be found exactly. Nothing is written until every site has
  been read back: one unconfirmable site declines the whole rename.
- **Chains through an untyped method end there.** Ruby's own library declares its return types;
  most applications do not, and inferring the type of every expression is a different project.
- **Runtime metaprogramming is invisible** — `define_method`, an `include` from a variable, a
  `Class.new` nothing names.
- **Not included:** formatting, RuboCop, quick fixes, code lenses, test running, a debugger, a
  plugin API.

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

**Neovim** — add a second server beside the first; `nvim-lspconfig` ships the `rubocop` half already:

```lua
vim.lsp.config('rubocop', {
  cmd = { 'rubocop', '--lsp' },
  filetypes = { 'ruby' },
  root_markers = { '.rubocop.yml', 'Gemfile', '.git' },
})
vim.lsp.enable({ 'ya_lsp', 'rubocop' })
```

**Helix** — add `rubocop` to the same language:

```toml
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

[log]
level      = "info"                       # stderr; YA_LSP_LOG outranks it
file       = false                        # a second copy on disk, for a bug report
file_path  = "tmp/ya-lsp.log"             # relative to the workspace root
file_level = "debug"                      # its own level: every request, in and out

[rails]
enabled     = "auto"                      # "auto" | true | false; auto looks for
                                          # config/application.rb, then railties in Gemfile.lock
schema      = true                        # db/schema.rb, db/structure.sql, table_name
models      = true                        # associations, enum, attribute, delegate, scope, ...
routes      = true                        # config/routes.rb and its helpers
entrypoints = true                        # mailers, jobs, Sidekiq workers
views       = true                        # what a template can call, and its @ivars

[trees]
# test         = ["spec", "test", "tests", "features"]   # replaces; [] turns the fence off
test_support   = []                       # adds to the built-in `testing_support`
# migration    = ["db/migrat"]            # parent/mark pairs; replaces; [] turns it off

[types]
guess_from_names = true                   # `@user` is a `User`; always labelled as a guess
structs          = true                   # Struct.new and Data.define
annotations      = true                   # a Sorbet `sig`, a YARD `@return`

[hints]
block_parameters = true                   # what a method says it yields
locals           = true                   # what the call on the right hands back
returns          = true                   # what a signature says a `def` returns

[diagnostics]
enabled = true
rules   = {}                              # e.g. { parse-warning = "error" }
```

Rules are keyed by the name in a diagnostic's `code`. Only `parse-error` and `parse-warning` are
statements about your code and are on by default; the rest describe indexer limits.

`[rails]` is about **answers, not speed**: a project that is not Rails should not be told about
Rails, and one with an `app/views/` gets a hover citing a controller that does not exist. `[trees]`
says where this project keeps the trees the server fences on — a `def` written in your test suite
is offered and jumped to only from inside it. Both lists that *replace* say so in the log when you
set one; a fenced file is still indexed, so `references`, `rename` and highlight find every use in
it either way.

**Diagnostics** are Prism's parse errors and warnings. The other rules ship `off` or `hint`,
because they fire on correct Ruby. ERB templates publish none: a template's Ruby does not parse
standalone, since `<%= yield %>` is legal in the method it compiles to.

**File watching.** A `git checkout`, `git pull`, rebase or `rails g model` re-indexes exactly the
files it touched, deletions included, with no restart. Needs a client that accepts a file watcher
registration; ya-lsp logs when one does not, because being blind looks like being wrong.

VS Code exposes all of these except `gems.max_files`, with the TOML key in each description.
`ya-lsp.logLevel` is `[log] level` under its shipped name and no longer restarts anything;
`ya-lsp.serverPath` is the one setting with no TOML twin, and the one that still does — it decides
which binary is spawned. Commands: **ya-lsp: Restart Server**, **ya-lsp: Show Output**. A committed
`ya-lsp.toml` wins over all of them.

## Development

`make` lists every target. `make setup` installs the two pinned cargo subcommands; `make ci` runs
what CI runs — format, clippy, tests and a coverage gate at three bars. Rust 1.93.1, pinned in
`.tool-versions`.

`make canary` is the one target needing a network: it fetches a pinned commit of a real,
open-source Rails application, opens it the way an editor does, and checks that the expected files
index, that none fails to parse, and that the cold index stays inside a generous ceiling. It is a
canary, not a benchmark, and does not cover gems. CI runs it as its own job.

`make audit` asks the question a timing run does not: **is the answer right?** It draws a
stratified sample of real cursors over six pinned Rails applications and scores them in three lanes
— keys that say *wrong* against the source, checks that say *inconsistent* against the server's own
other answers, and a residue a person rules on once. It needs the corpora on disk, so it is not in
`make ci`.

## Licence

MIT — see [LICENSE.txt](LICENSE.txt). ya-lsp embeds Ruby's own RBS signatures, which are
BSD-2-Clause/Ruby; [NOTICE.txt](NOTICE.txt) carries their notice and
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) every linked crate's licence.
