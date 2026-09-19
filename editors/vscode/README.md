# ya-lsp for VS Code

**Ruby language support that never runs Ruby.** Install the extension and open a `.rb` file. That
is the whole setup.

## Why use it

1. **No Ruby needed.** It never runs `ruby`, `bundle` or `gem`. It reads your code, `Gemfile.lock`
   and the gems on disk. The binary ships inside the extension, so nothing is added to your
   `Gemfile`.
2. **Every answer says how far to trust it.** Hover cards and completion rows are labelled
   *Resolved*, *Derived* or *Guessed*. Guessing can be switched off.
3. **Rails without booting Rails.** Columns, associations, `enum`s, routes, mailers and jobs all
   resolve, because the schema and the macros are read as text.
4. **Fast, no cache.** A real 612-file Rails app indexes in well under a second. Nothing sits on
   disk to go stale after a branch switch.
5. **Your gems too.** Definitions, hover and completion follow into the bundle, Ruby's own library
   and their RBS signatures.

```ruby
"x".upcase          # String  — Resolved: the code names the type
@title.upcase       # String  — Derived: from the assignment, and the card says so
@user               # User    — Guessed: from the name alone
```

---

## Getting started

1. **Install.** Press `Ctrl+P` / `Cmd+P` and run `ext install ar2em1s.yalsp`, or open
   [the Marketplace page](https://marketplace.visualstudio.com/items?itemName=ar2em1s.yalsp).
2. **Open a Ruby project.** The server starts on the first `.rb` or `.erb` file. It indexes the
   workspace, then the bundle, with progress in the status bar.
3. **Check it works.** Hover any constant: the card names the declaration and its tier. If nothing
   appears, run **ya-lsp: Show Output**.
4. *(Optional)* **Commit a `ya-lsp.toml`** in the workspace root so the whole team gets the same
   setup in every editor. It overrides every setting below, and changes apply without a restart.

   ```toml
   [types]
   guess_from_names = false   # keep only answers that can be checked
   ```

**Requirements:** VS Code 1.108 or newer. A binary is bundled for:

- macOS on Apple silicon
- Linux x64 and arm64
- Windows x64 and arm64

Anywhere else (Intel macOS included), build the server from
[the repository](https://github.com/ar2em1s/ya-lsp) and point `ya-lsp.serverPath` at it.

---

## What you get

| Feature | What it does |
| --- | --- |
| **Diagnostics** | Parse errors and warnings as you type. Severity is set per rule. |
| **Go to definition** | Constants, methods and `require "..."` paths, including inside gems. |
| **Go to implementation** | The method, then every override below the receiver's class. |
| **Go to type definition** | The class of the value under the cursor. |
| **Go to declaration** | The RBS signature of a method. |
| **Hover** | Signature, docs, and where the type came from. |
| **Completion** | Knows ancestors and visibility. Completes keyword arguments and model columns. |
| **Signature help** | The parameters of the current call, with the current one marked. |
| **Inlay hints** | Block parameters, locals and returns. A guess is never drawn. |
| **References** | Exact for constants, by name for methods. Searches your own code only. |
| **Type and call hierarchy** | Ancestors and every descendant; callers (by name) and callees (exact). |
| **Rename (F2)** | Locals, parameters and constants. It refuses anything it cannot do exactly, and says why. |
| **Rename a Rails file** | Move `order.rb` to `purchase.rb` in the Explorer and `Order` becomes `Purchase`. |
| **Refactorings** | Extract a variable or method, toggle `{ }` ↔ `do … end`, declare an `attr_`. |
| **Generated RBS** | The lightbulb opens what a schema, macro or route declared, read-only. |
| **Folding** | Follows the syntax: `if`/`elsif`/`else`, heredocs, comment blocks and `#region`. `end` stays visible. |
| **Templates** | `.erb` gets full answers. `.jbuilder`, `.builder` and `.ruby` are read as plain Ruby. |
| **Unsaved buffers** | An `Untitled-1` set to Ruby gets answers too. |

- **Every refactoring is tested on a copy of the file first.** Nothing in the menu can leave your
  buffer unparseable.
- **Guessed answers never reach the margin, and go-to implementation, type definition and
  declaration refuse them too.** Set `ya-lsp.types.guessFromNames` to off to drop guesses entirely.
- **Methods are matched by name once the receiver is untyped.** The
  [repository README](https://github.com/ar2em1s/ya-lsp#readme) has the full tier table and the
  limits.

---

## Which Ruby it indexes

1. **Read** `.ruby-version` or `.tool-versions`, in the project or any directory above it.
2. **Fall back** to `RUBY VERSION` in `Gemfile.lock`.
3. **If neither answers, it says so rather than guessing**, and Ruby's own library stays out of the
   index.
   - Fix it with `ya-lsp.gems.rubyVersion`.
   - Silence it with `ya-lsp.gems.defaultGems`.

---

## Settings

A committed `ya-lsp.toml` **overrides every one of these**. Its key is listed beside each setting.

| Setting | `ya-lsp.toml` | What it does |
| --- | --- | --- |
| `ya-lsp.serverPath` | — | Run your own binary instead of the bundled one. Takes `~` and `${workspaceFolder}`. Restarts the server. |
| `ya-lsp.logLevel` | `[log] level` | How much goes to the output channel. Applies without a restart. |
| `ya-lsp.log.file` | `[log] file` | Also write the log to a file, for bug reports. Off by default. |
| `ya-lsp.log.filePath` | `[log] file_path` | Where that file goes. Relative to the folder, or absolute. Never truncated. |
| `ya-lsp.log.fileLevel` | `[log] file_level` | How much goes in the file. |
| `ya-lsp.trace.server` | — | Log the LSP traffic. For debugging this extension. |
| `ya-lsp.gems.enabled` | `[gems] enabled` | Follow answers into the project's gems. On by default. |
| `ya-lsp.gems.defaultGems` | `[gems] default_gems` | Also index the gems that ship inside Ruby (`json`, `uri`, `optparse`). |
| `ya-lsp.gems.rubyVersion` | `[gems] ruby_version` | Which Ruby's gems to index, such as `3.3.0`. Detected when empty. |
| `ya-lsp.gems.paths` | `[gems] paths` | Extra gem roots, searched first. For a container image or a Nix store path. |
| `ya-lsp.rbs.enabled` | `[rbs] enabled` | Ruby's core signatures, so `String` and `Array` have members. |
| `ya-lsp.rbs.stdlib` | `[rbs] stdlib` | Also the standard library signatures (`CSV`, `URI`, `Logger`). |
| `ya-lsp.rbs.path` | `[rbs] path` | An explicit directory of RBS signatures. Found automatically when empty. |
| `ya-lsp.types.guessFromNames` | `[types] guess_from_names` | Answer from a receiver's name when nothing else can. Always labelled a guess. |
| `ya-lsp.types.structs` | `[types] structs` | Read `Struct.new` and `Data.define`. |
| `ya-lsp.types.annotations` | `[types] annotations` | Read Sorbet `sig`s and YARD `@return` tags. |
| `ya-lsp.hints.blockParameters` | `[hints] block_parameters` | Label what a block parameter holds. |
| `ya-lsp.hints.locals` | `[hints] locals` | Label what a local holds when it is assigned from a call. |
| `ya-lsp.hints.returns` | `[hints] returns` | Label what a method returns. |
| `ya-lsp.diagnostics.enabled` | `[diagnostics] enabled` | Report problems found while indexing. |
| `ya-lsp.diagnostics.rules` | `[diagnostics.rules]` | Per-rule severity, keyed by the rule's `code`. All rules complete by name. |
| `ya-lsp.rubocop.hint` | — | Offer RuboCop's extension, once, in a project that uses it. |
| `ya-lsp.rails.enabled` | `[rails] enabled` | Rails conventions. `auto` looks for `config/application.rb`, then railties in `Gemfile.lock`. |
| `ya-lsp.rails.schema` | `[rails] schema` | Read `db/schema.rb` or `db/structure.sql`. |
| `ya-lsp.rails.models` | `[rails] models` | Read model macros (associations, `enum`, `delegate`, `scope`, …) and type `Story.where(...)`. |
| `ya-lsp.rails.routes` | `[rails] routes` | Read `config/routes.rb`, so `stories_path` completes and jumps. |
| `ya-lsp.rails.entrypoints` | `[rails] entrypoints` | Read mailers, jobs and Sidekiq workers. |
| `ya-lsp.rails.views` | `[rails] views` | Answer inside templates: helpers, and the controller's `@story`. |
| `ya-lsp.trees.test` | `[trees] test` | Directory names holding your tests. Their `def`s are offered only from inside them. **Replaces** the list; `[]` turns it off. |
| `ya-lsp.trees.testSupport` | `[trees] test_support` | Extra test-scaffolding names, for the cursor only. Added to the built-in list. |
| `ya-lsp.trees.migration` | `[trees] migration` | Where migrations live, as `parent/mark` pairs. |
| `ya-lsp.index.include` | `[index] include` | Globs of files to index, relative to the folder. |
| `ya-lsp.index.exclude` | `[index] exclude` | Globs to skip. |
| `ya-lsp.index.loadPaths` | `[index] load_paths` | Extra roots to index and to resolve `require` against, such as a monorepo's shared tree. |
| `ya-lsp.index.maxFiles` | `[index] max_files` | Refuse to index a workspace larger than this. |

`gems.max_files` exists only in `ya-lsp.toml`. `index.max_files` is the limit that actually applies.

---

## Running RuboCop alongside

ya-lsp never runs Ruby, so it gives no cop offences and no autocorrect. For those:

1. **Install [RuboCop](https://marketplace.visualstudio.com/items?itemName=rubocop.vscode-rubocop).**
   Both servers run side by side with nothing to configure: no competing formatter, separate
   diagnostics, and lightbulbs that merge.
2. **Set `ya-lsp.diagnostics.rules` to `{ "parse-warning": "off" }`**, so unused-variable warnings
   arrive once. A file that does not parse still gets its squiggle.
3. **Use RuboCop 1.89 or newer** for *Autocorrect* and *Disable for this line* in the lightbulb.

ya-lsp offers the install once per project, when it finds `.rubocop.yml` or `rubocop` in
`Gemfile.lock`. **Don't show again** or `ya-lsp.rubocop.hint` turns the offer off.

---

## Commands

- **ya-lsp: Restart Server**
- **ya-lsp: Show Output**

---

## Multi-root workspaces

- **One server per folder.** In a multi-root workspace a folder's server starts when you open a
  Ruby file in it, so a folder with no Ruby never gets one.
- **Nested folders work.** A file in the inner folder is answered by the inner folder's server
  only, with its own bundle.
- **A tree several apps share** goes in `ya-lsp.index.loadPaths`. It then counts as your own code
  in every folder that names it.

---

## Licence

MIT. Ruby's RBS signatures are vendored under their own licence: run `ya-lsp --licenses` or see
[`NOTICE.txt`](https://github.com/ar2em1s/ya-lsp/blob/master/NOTICE.txt).
