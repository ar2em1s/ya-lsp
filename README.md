# ya-lsp

*Y(et) A(nother) LSP* — a Ruby language server that never runs Ruby.

**Install:** VS Code → [ya-lsp on the Marketplace](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp),
then open a `.rb` file. Other editors: [Install](#install).

## Why use it

1. **No Ruby needed.** It never runs `ruby`, `bundle` or `gem`. It reads your code, `Gemfile.lock`
   and the gems on disk. It ships as one Rust binary.
2. **It says when it is guessing.** An answer read off a name alone is marked *Guessed from name
   alone.* on its card, is never drawn as an inlay hint, and can be switched off.
3. **Rails without booting Rails.** Columns, associations, `enum`s, routes, mailers and jobs all
   resolve, because the schema and the macros are read as text.
4. **Fast, no cache.** A real 612-file Rails app indexes in well under a second, and CI fails the
   build above 500 ms. Nothing sits on disk to go stale.
5. **Types from RBS.** Ruby's own signatures are built in. It also reads a gem's `sig/`, your `sig/`,
   `.gem_rbs_collection/`, Sorbet `sig`s and YARD `@return`.

```ruby
story = Story.where(published: true).first
story.title
#     ↑ hover: Story#title -> String      (the column, read from db/schema.rb)
```

**Not included:** formatting and RuboCop, because both are Ruby. Run
[RuboCop's server alongside](#running-rubocop-alongside) to get them.

---

- [Install](#install)
- [What you get](#what-you-get)
- [Trust tiers](#trust-tiers)
- [Rails](#rails)
- [Limits](#limits)
- [Running RuboCop alongside](#running-rubocop-alongside)
- [Configuration](#configuration)
- [Development](#development)

## Install

### VS Code

1. Install [ya-lsp from the Marketplace](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp).
2. Open a Ruby file.

The extension bundles the binary for your platform. To use a local build, set `ya-lsp.serverPath`.

### Claude Code

1. `/plugin marketplace add ar2em1s/ya-lsp`
2. `/plugin install ya-lsp@ya-lsp`
3. Run `/ya-lsp:ya-lsp-setup`. It installs or updates the binary, then asks your project three
   questions to prove it works. It asks you before downloading or writing anything.

**If another plugin already handles `.rb` files** (usually the official `ruby-lsp`), disable it in
`/plugin`. Otherwise whichever server loads first wins, and nothing tells you which one it was.

Two gaps come from how Claude Code works, not from ya-lsp:

- `Gemfile` and `Rakefile` get no answers, because the plugin routes by file extension.
- A jump into a git-ignored directory such as `vendor/bundle` is hidden.

### Any other editor

1. Download your platform's archive from the [latest release](https://github.com/ar2em1s/ya-lsp/releases/latest),
   or run `cargo build --release`.
2. Extract it and check that it runs:

   ```bash
   tar xzf ya-lsp-aarch64-apple-darwin.tar.gz
   ./ya-lsp-aarch64-apple-darwin/ya-lsp --version
   ```

3. Put `ya-lsp` on your `PATH` and point your editor at `ya-lsp --stdio`.

`SHA256SUMS` in the release covers every asset. `ya-lsp --licenses` prints all licence notices.

**Neovim 0.11+** (`init.lua`):

```lua
vim.lsp.config('ya_lsp', {
  cmd = { 'ya-lsp', '--stdio' },
  filetypes = { 'ruby', 'eruby' },
  root_markers = { 'ya-lsp.toml', 'Gemfile', '.git' },
})
vim.lsp.enable({ 'ya_lsp' })
```

**Helix** (`languages.toml`):

```toml
[language-server.ya-lsp]
command = "ya-lsp"
args = ["--stdio"]

[[language]]
name = "ruby"
language-servers = ["ya-lsp"]
```

**Zed:** there is no extension for it yet.

### Which Ruby version it reads

It checks these in order and uses the first that answers:

1. `.ruby-version`
2. `.tool-versions`
3. `RUBY VERSION` in `Gemfile.lock`

The first two are searched from the project directory upwards. If none answers, it says so and does
not guess.

## What you get

| Feature | What it does |
| --- | --- |
| **Diagnostics** | Parse errors and Prism warnings as you type. Severity set per rule. |
| **Go to definition** | Constants, methods, `require "..."` paths, including inside gems. |
| **Go to implementation** | The method, then every override below the receiver's class. |
| **Go to type definition** | The class of the value under the cursor. |
| **Go to declaration** | The RBS signature of a method. |
| **Hover** | Signature, docs and the type. A guess says it is one. |
| **Completion** | Knows ancestors and visibility. Completes keyword arguments and model columns. |
| **Signature help** | The parameters of the current call, with the current one marked. |
| **Inlay hints** | Types the line does not show. Guesses are never drawn. |
| **References** | Exact for constants, by name for methods. Searches your code only. |
| **Highlight** | The same name in the file, with reads and writes marked differently. |
| **Symbols** | File outline, plus fuzzy search over the project, gems, core and stdlib. |
| **Type and call hierarchy** | Ancestors and descendants; callers and callees. |
| **Rename** | Locals, parameters and constants. It refuses anything it cannot do exactly. |
| **Rename a Rails file** | Move `order.rb` to `purchase.rb` and `Order` becomes `Purchase`. |
| **Refactorings** | Extract variable or method, toggle block style, declare `attr_`. |
| **Generated RBS** | Opens what a schema, macro or route declared, as a read-only document. |
| **Folding, selection, highlighting** | Folding ranges, expand selection, local-vs-call colouring. |
| **Templates** | `.erb` and `.jbuilder` views get full answers. `.builder` and `.ruby` are read as plain Ruby. |
| **Unsaved buffers** | An `Untitled-1` set to Ruby gets answers too, including parse errors. |

## Trust tiers

| Tier | Meaning | Example |
| --- | --- | --- |
| **Resolved** | The code names the type | `Foo.bar`, `"x".upcase`, `Foo.new.bar` |
| **Derived** | A signature, an assignment or a Rails convention says so | `"x".upcase.strip`, `ENV.fetch`, a view's `@story` |
| **Guessed** | Only the receiver's name matched | `@user` → `User` |

- **A card shows Resolved and Derived the same way.** It says nothing about how an answer was found,
  and marks a guess *Guessed from name alone.*
- **Guessed** is the only tier that can be wrong. It never replaces a better answer.
- A guess is never drawn as an inlay hint. Go to implementation, type definition and declaration
  also refuse to answer from a guess.
- To turn guessing off, set `[types] guess_from_names = false`.

## Type coverage

`ya-lsp coverage [DIR]` prints what share of your production code's calls ya-lsp can type:

```console
$ ya-lsp coverage
Type coverage: 47.3% ± 2.1% (2,000 of 37,909 calls sampled)
```

- It indexes the project and its gems itself, reading `ya-lsp.toml`: a few seconds on a large
  application. No editor or running server is needed.
- It samples 2,000 of the calls whose value is used, in your own Ruby files outside test and
  migration folders (`[trees]`). A project with no more calls than that is counted in full, and
  the `±` goes away.
- A call counts as typed where ya-lsp is sure of its type, as an inlay hint would be. A guess does
  not count. The `±` is the sample's 95% error.
- Progress goes to stderr, the result to stdout.

## Rails

Nothing boots. Each item below is read as text:

1. **Columns** come from `db/schema.rb` or `db/structure.sql`, for every database. Hover shows the
   table, the column and whether it can be nil.
2. **Model macros** such as associations, `enum`, `attribute`, `delegate`, `scope` and about twenty
   more, including those added through a concern. Each jumps to the line you wrote.
3. **The query interface.** `Story.first` is a `Story`, `Story.first(3)` is an array, and
   `Story.where` jumps into activerecord.
4. **Routes, mailers, jobs and Sidekiq workers.** Route helpers jump to their line in
   `config/routes.rb`. `perform_later` resolves to the `def` it ends up calling.
5. **Views and engines.** `@story` in `stories/show.html.erb` gets its type from
   `StoriesController`, and keeps its `nil` out where a `before_action` always sets it. A
   partial's locals come from the calls that render it, a jbuilder view reads like an ERB one, and
   a gem's `app/` is indexed, so `ActiveStorage::Blob` resolves.
6. **`ActiveSupport::CurrentAttributes`.** `Current.user` is what the application assigns to it, or
   `nil`.

Outside Rails, ya-lsp also reads **RSpec** (`describe` groups, `let`, `subject`), **FactoryBot**
(`create(:user)` is a `User`) and **translation files** (`t("users.show.title")` completes its keys
and is typed by what the key holds).

## Limits

1. **Method calls with an unknown receiver are matched by name.** In that case signature help and
   keyword completion show nothing, instead of guessing.
2. **References never search gems.** A common name can appear there tens of thousands of times.
3. **Rename refuses methods and instance variables**, because neither can be found exactly. It also
   refuses the whole rename if any one site cannot be confirmed.
4. **A chain stops at an untyped method.** It does not infer types for arbitrary expressions.
5. **Runtime metaprogramming is invisible**, such as a `define_method` whose name is computed or a
   `Class.new` nothing names.

**Not included:** formatting, RuboCop, quick fixes, code lenses, test running, a debugger, a plugin API.

## Running RuboCop alongside

1. Run `rubocop --lsp` as a second server. Quick fixes need RuboCop **1.89+**.
2. Turn off the duplicate warning in `ya-lsp.toml`. Otherwise "assigned but unused variable" shows
   twice:

   ```toml
   [diagnostics.rules]
   parse-warning = "off"
   ```

**VS Code:** install the [RuboCop extension](https://marketplace.visualstudio.com/items?itemName=rubocop.vscode-rubocop).
There is nothing to configure. To stop ya-lsp suggesting it, set `ya-lsp.rubocop.hint`.

**Neovim:**

```lua
vim.lsp.config('rubocop', {
  cmd = { 'rubocop', '--lsp' },
  filetypes = { 'ruby' },
  root_markers = { '.rubocop.yml', 'Gemfile', '.git' },
})
vim.lsp.enable({ 'ya_lsp', 'rubocop' })
```

**Helix:**

```toml
[language-server.rubocop]
command = "rubocop"
args = ["--lsp"]

[[language]]
name = "ruby"
language-servers = ["ya-lsp", "rubocop"]
```

**Zed:** `{ "languages": { "Ruby": { "language_servers": ["rubocop", "..."] } } }`

The two servers share no state and advertise no overlapping capability.

## Configuration

Put a `ya-lsp.toml` at the workspace root. It overrides editor settings and applies without a
restart. These are the defaults:

```toml
[index]
include           = [                     # yours replaces this list; it does not add to it
  "**/*.rb", "**/*.erb", "**/*.jbuilder", "**/*.builder", "**/*.ruby",
  "**/*.rbs", "**/*.rake", "**/*.gemspec", "**/*.ru",
  "**/Rakefile", "**/Gemfile",
]
exclude           = ["vendor/**/*", ".bundle/**/*", "tmp/**/*", "node_modules/**/*"]
load_paths        = ["lib", "app"]        # extra roots, also used to resolve `require`
max_files         = 50000

[gems]
enabled      = true
default_gems = true                       # the ~40 gems shipped inside Ruby
# ruby_version = "3.4.1"                  # unset: read from .ruby-version / .tool-versions
paths        = []                         # extra gem roots
max_files    = 300000

[rbs]
enabled = true
stdlib  = true                            # the ~60 stdlib libraries beyond core
# path  = "/path/to/rbs"                  # unset: the rbs gem, else the copy in the binary

[log]
level      = "info"                       # stderr; YA_LSP_LOG overrides it
file       = false                        # also write a log file, for bug reports
file_path  = "tmp/ya-lsp.log"             # relative to the workspace root
file_level = "debug"

[rails]
enabled     = "auto"                      # "auto" | true | false
schema      = true                        # db/schema.rb, db/structure.sql, table_name
models      = true                        # associations, enum, attribute, delegate, scope, ...
routes      = true                        # config/routes.rb and its helpers
entrypoints = true                        # mailers, jobs, Sidekiq workers
views       = true                        # what a template can call, and its @ivars

[trees]
# test         = ["spec", "test", "tests", "features"]   # replaces; [] turns the fence off
test_support   = []                       # adds to the built-in `testing_support`
# migration    = ["db/migrat"]            # parent/mark pairs; replaces; [] turns it off

[rspec]
enabled = "auto"                          # "auto" (rspec-core in Gemfile.lock) | true | false

[i18n]
enabled = "auto"                          # "auto" (i18n in Gemfile.lock) | true | false
locale  = "en"                            # the one locale read
# paths = ["**/config/locales/**/*.yml", "**/config/locales/**/*.rb"]   # replaces; gems' files are read either way

[types]
guess_from_names = true                   # `@user` is a `User`, always labelled as a guess
structs          = true                   # Struct.new and Data.define
annotations      = true                   # Sorbet `sig`, YARD `@return`
factories        = true                   # FactoryBot: `create(:user)` is a `User`

[hints]
block_parameters = true                   # what a method yields
locals           = true                   # what the call on the right returns
returns          = true                   # what a signature says a `def` returns

[diagnostics]
enabled = true
rules   = {}                              # e.g. { parse-warning = "error" }
```

What some of these settings do:

- **`[rails]`** controls which answers you get, not speed. `auto` checks for `config/application.rb`,
  then for `railties` in `Gemfile.lock`.
- **`[rspec]` and `[i18n]`** are their own tables because neither needs Rails. `auto` looks for the
  gem in `Gemfile.lock`. RSpec is read in the spec files the editor has open.
- **`[trees]`** marks your test tree. A method defined in the test suite is offered, and jumped to,
  only from inside that suite.
- **`[diagnostics]`**: only `parse-error` and `parse-warning` are on by default. ERB templates
  report no diagnostics.
- **File watching** needs no setup. `git checkout`, rebases and `rails g` re-index only the files
  they changed. If your editor cannot watch files, ya-lsp watches them itself. A buffer with
  unsaved changes always wins over the file on disk.
- **VS Code** exposes every setting except `gems.max_files`, and each description names its TOML
  key. `ya-lsp.serverPath` is the one setting that restarts the server. A committed `ya-lsp.toml`
  wins over VS Code settings.

## Development

```bash
make          # list targets
make setup    # install the pinned toolchain pieces
make ci       # what CI runs: fmt, clippy, tests, coverage
make canary   # open a pinned real Rails app and check the counts (needs network)
make audit    # score answers against six pinned Rails apps (needs the corpora)
```

Rust 1.93.1, pinned in `.tool-versions`.

## Licence

MIT: see [LICENSE.txt](LICENSE.txt). The Ruby RBS signatures embedded in the binary are
BSD-2-Clause/Ruby, and [NOTICE.txt](NOTICE.txt) carries their notice.
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) lists every linked crate's licence.
