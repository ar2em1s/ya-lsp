# ya-lsp for VS Code

Ruby language support that does not need Ruby.

The extension bundles a `ya-lsp` binary for your platform. There is nothing to install and nothing
to add to your `Gemfile`: ya-lsp never executes Ruby, `bundle` or `gem`, so it works on a machine
where the project's Ruby is not installed at all.

It does *read* `.ruby-version` and `.tool-versions` — in the project or any directory above it, the
way rbenv, chruby, RVM, asdf and mise all resolve them — falling back to `RUBY VERSION` in
`Gemfile.lock`. That is how it knows which Ruby's own library to index. When none of them answer it
says so rather than guessing, and `json`, `uri` and the other ~40 gems inside Ruby stay out of the
index; `ya-lsp.gems.rubyVersion` settles it, and `ya-lsp.gems.defaultGems` silences it.

## What you get

Diagnostics, go-to-definition, hover, document symbols, workspace symbol search, find-references,
completion, signature help, occurrence highlighting, folding, expand-selection, rename, four
refactorings and the type hierarchy — across your project **and its gems**, which are read straight
from `Gemfile.lock` and the gem directories on disk.

Folding follows the syntax rather than the indentation VS Code otherwise guesses from: `if`,
`elsif` and `else` fold as three regions, a heredoc's body folds, comment blocks and `#region`
markers fold as themselves, and every `end` stays on screen.

**Show Type Hierarchy** works in both directions, on a class or a module, from the right-click menu
or the command palette. Upwards it lists Ruby's own ancestors — so included and `prepend`ed modules
are in it, the way `Module#ancestors` reports them; downwards it lists every class below, not only
the ones written `< Base`, so a subclass three levels down is in the list too.

**F2 renames** local variables, parameters and constants. A constant changes in every file it is
written in, and only where it really is that constant — `Person` inside `module HR` and
`HR::Person` at the top level are one name, a `Person` in another namespace is not. Nothing is
edited until every place it would be edited has been read back and confirmed to hold only that
name; if one of them cannot be, the whole rename is declined and a notification says which file to
look at. Methods, instance variables, anything defined in a gem, and a name that is also written
as a keyword or a hash key are declined the same way, each with its own reason — rename is the one
feature here that would rather say no than be approximately right.

**The lightbulb** offers four refactorings: extract the selection into a local variable or into a
method, toggle a block between `{ }` and `do … end`, and declare an `attr_reader`, `attr_writer` or
`attr_accessor` for the instance variable at the cursor. They are the four families that are
rewrites over a syntax tree; the fifth, autocorrecting a style offence, is RuboCop's own and
arrives if you run its server alongside.

Like rename, these decline rather than approximate — an extraction whose result would have to hand
a value back to the code after it, a block whose two spellings would bind to different calls, an
`attr_reader` that would read the class's `@count` rather than an instance's. And every action is
applied to a copy of the file and re-parsed before it is offered, so nothing in the menu can leave
your buffer unparseable.

## How precise are the answers?

ya-lsp resolves constants and does not infer types, and that line runs through every feature.
Constants are exact. Methods are matched by name once the receiver is a local variable, which
means find-references on `name` returns every call spelled that way. `Foo.`, `self.` and a bare
call in a class body all resolve properly. The repository README has the tier table and the
order a receiver is tried in.

## Settings

The settings UI groups these the way the table does. A committed `ya-lsp.toml` in the workspace
root **overrides every one of them**, so a team can agree on one setup that works in every editor;
changing it takes effect without a restart. The two spellings differ by convention rather than by
meaning, so the TOML key is named beside each setting.

| Setting | `ya-lsp.toml` | What it does |
| --- | --- | --- |
| `ya-lsp.serverPath` | — | Run a binary of your own instead of the bundled one. Takes `~` and `${workspaceFolder}`. Restarts the server. |
| `ya-lsp.logLevel` | — | How much the server writes to its output channel. `off` is off. Restarts the server. |
| `ya-lsp.trace.server` | — | Log the LSP traffic between VS Code and the server. For debugging this extension. |
| `ya-lsp.gems.enabled` | `[gems] enabled` | Follow definitions, hover and completion into the project's gems. On by default; it is most of the value. |
| `ya-lsp.gems.defaultGems` | `[gems] default_gems` | Also the ~40 gems that ship inside Ruby itself — `json`, `uri`, `optparse`. |
| `ya-lsp.gems.rubyVersion` | `[gems] ruby_version` | Which Ruby's gems to index, as in `3.3.0`. Detected when empty. |
| `ya-lsp.gems.paths` | `[gems] paths` | Extra gem roots, searched first. The escape hatch for a layout ya-lsp guesses wrong about — a container image, a Nix store path. |
| `ya-lsp.rbs.enabled` | `[rbs] enabled` | Ruby's core signatures, so `String`, `Array` and `Kernel` have members. |
| `ya-lsp.rbs.stdlib` | `[rbs] stdlib` | Also the ~60 standard library signatures — `CSV`, `URI`, `Logger`. Adds ~3.5 ms to every request. |
| `ya-lsp.rbs.path` | `[rbs] path` | An explicit directory of RBS signatures. Found automatically when empty. |
| `ya-lsp.diagnostics.enabled` | `[diagnostics] enabled` | Report problems found while indexing. |
| `ya-lsp.diagnostics.rules` | `[diagnostics.rules]` | Per-rule severity, keyed by the rule name in the problem's `code`. All ten complete by name and say what they fire on. |
| `ya-lsp.index.include` | `[index] include` | Globs, relative to the folder, of the files to index. |
| `ya-lsp.index.exclude` | `[index] exclude` | Globs to skip. Where vendored or generated Ruby goes when git does not ignore it. |
| `ya-lsp.index.loadPaths` | `[index] load_paths` | Extra roots to index, also used to resolve `require "..."`. |
| `ya-lsp.index.respectGitignore` | `[index] respect_gitignore` | Skip what git ignores. Turn it off for a project whose Ruby is generated into an ignored directory. |
| `ya-lsp.index.maxFiles` | `[index] max_files` | Refuse to index a workspace larger than this. |

`gems.max_files` is the one setting `ya-lsp.toml` has and the editor does not: it is a ceiling on
gem files nobody has needed to tune, and `index.max_files` is the one that actually fires.

### When something else is already linting the file

Switch the rule off rather than the category. `ya-lsp.diagnostics.rules` set to
`{ "parse-warning": "off" }` silences the "assigned but unused variable" class of warnings — the
ground RuboCop's and Standard's `Lint/UselessAssignment` covers — while a file that does not parse
still gets its squiggle. Every rule can be set that way, all ten complete by name, and each says on
hover what it fires on.

### Running RuboCop alongside

ya-lsp never runs Ruby, so no cop offence and no autocorrect ever comes from it. Install
[RuboCop](https://marketplace.visualstudio.com/items?itemName=rubocop.vscode-rubocop), published by
the RuboCop team, and run both — LSP allows a language to be served by more than one server, and
these two divide the work rather than competing for it. There is nothing to configure: ya-lsp
advertises no formatting capability, so there is no default formatter to pick between; each
extension owns its own diagnostic collection rather than overwriting the other's; and the two
lightbulbs merge, because ya-lsp advertises only `refactor.extract` and `refactor.rewrite` while
RuboCop advertises only `quickfix`. Turn `parse-warning` off, as above, so
that the one class of warning they both report arrives once.

You get offences, formatting, and — **on RuboCop 1.89 or newer** — a lightbulb on each offence
offering *Autocorrect* and *Disable for this line*. 1.89 is where RuboCop's server started
answering `textDocument/codeAction`; on an older one the quick fixes are absent and only the
whole-document **RuboCop: Format with Autocorrects** command applies them.

ya-lsp offers this once per project, when it finds a `.rubocop.yml` or `rubocop` in
`Gemfile.lock` and the extension is not installed. `ya-lsp.rubocop.hint` turns the offer off, and
so does choosing **Don't show again**.

## Commands

- **ya-lsp: Restart Server**
- **ya-lsp: Show Output**

## Multi-root workspaces

One server per folder, because everything a server does — the index, gem discovery, `ya-lsp.toml`
— is scoped to a single root. A workspace with a single folder starts its server with the window.
In a multi-root workspace each folder starts when you open a Ruby file inside it, so a folder with
no Ruby in it — infrastructure, docs, a service in another language — never gets one.
