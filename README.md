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
| **Highlight occurrences** | Every place in the file that means the same name, reads and writes apart. |
| **Document symbols** | The file's outline, nested. |
| **Workspace symbols** | Fuzzy search across the project, its gems, core and stdlib. |
| **Completion** | Ancestor-aware, visibility-aware, with keyword arguments. See below. |
| **Signature help** | The parameters of the call you are inside, with the one you are writing marked. |
| **Folding** | Definitions, branches, blocks, literals, heredocs, comment blocks, `#region`. |
| **Expand selection** | Out from the cursor through the steps Ruby has, not the ones a word has. |
| **Type hierarchy** | Both directions: every ancestor of a class, and everything below it. |
| **Rename** | Locals, parameters and constants, across every file. Refuses what it cannot do exactly. |
| **Gem indexing** | Every gem in `Gemfile.lock`, read from disk, with progress reported. |
| **Core & stdlib** | `String`, `Array`, `Hash`, `Kernel`, plus `CSV`, `URI`, `Logger` and ~57 more. |
| **Multi-root** | One server per folder; a folder with no Ruby in it never gets one. |
| **Stays current** | A `git checkout`, a rebase or a generator re-indexes what it changed. |

Which Ruby's library to index is read from `.ruby-version` or `.tool-versions` — in the project or
any directory above it, the way rbenv, chruby, RVM, asdf and mise all resolve it — falling back to
`RUBY VERSION` in `Gemfile.lock`. ya-lsp refuses to guess when none of them answer, and says so
rather than silently indexing nothing: guessing once put macOS's vestigial Ruby 2.6 into the index.
Core and stdlib signatures come from the `rbs` gem when the machine has one and from a copy inside
the binary when it does not.

Files that change outside the editor are picked up as they change. A `git checkout`, a `git pull`,
a rebase or a `rails g model` re-indexes exactly the files it touched — including deletions, which
nothing else in the protocol ever reports — with no restart and without opening anything. It needs
an editor that accepts a file watcher registration, which most do; ya-lsp says in its log when one
does not, because everything about the alternative looks like the server being wrong rather than
the server being blind.

Typing `(` or `,` inside a call shows what that method takes, with the argument you are on
marked — including keyword arguments, which are matched by **name** rather than by position, so
reordering them keeps the right one highlighted. `Person.new(` shows what `Person#initialize`
takes rather than `Class#new`. Where RBS declares several forms of a method, all of them are
offered and the one your call fits is selected — `String#gsub` has three. It appears only where a
method resolves exactly, which is the next section's subject.

Folding is by syntax rather than by indentation, which is what an editor guesses with when no
language server answers. `if`, `elsif` and `else` fold as three regions rather than one; a
heredoc's body folds even though it sits nowhere near the expression that opens it; runs of
comment lines and `#region` markers fold as themselves and are labelled as such, so **Fold All
Comments** does what it says. Every `end`, `}` and `]` stays on screen — a collapsed `def foo`
with nothing closing it reads as broken code — and a construct on one line gets no chevron,
since clicking it would do nothing. Where ya-lsp finds nothing at all it says so, which puts the
editor's own indentation guess back in play rather than replacing it with silence.

Expanding the selection walks out through what Ruby actually has: the inside of a string before
its quotes, one argument before the argument list, a method name and then its receiver before the
next call in the chain, a body before the `def` and the `def` before the `class`. Every step
contains the one before it even while the file is half-typed, which is the part the protocol
defines the answer by and the part a parser recovering from an error will happily break.

The type hierarchy answers both directions, up the chain and down it. Supertypes are
`Module#ancestors` minus the class itself, which is the linearization Ruby's own method lookup
walks: so **modules are in it** — `Comparable` is an ancestor of `String`, and a `prepend`ed module
sits above the class that prepends it. That surprises anyone expecting single inheritance and it is
what Ruby means by an ancestor, so it is not filtered out to look familiar. Subtypes are the mirror
of it rather than one generation of it: expanding `Base` lists every class below it, not only the
ones written `< Base`. An ancestor that could not be resolved — a superclass in a gem that is not
installed — is shown as a row saying so rather than left out of a chain that would otherwise read
as complete.

Putting the cursor on a name marks every other place in the file that means the same thing, with
assignments marked differently from reads. Locals are scoped properly, so the `total` in one method
and the `total` in the next are not confused for each other, a block parameter is separate from the
local it shadows, and an instance variable in `def self.build` is separate from the one in an
instance method — which is what Ruby does. What it will not mark is the same word in a comment or
inside a string, which is where an editor's own occurrence highlighting spends most of its time
being wrong. Where ya-lsp does not know, it says nothing and your editor's word matching takes over.

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
  **Signature help and keyword-argument completion say nothing at all here**, rather than showing
  whichever class's parameters matched the name: a list of arguments you are typing into is
  believed, and a wrong one is valid Ruby that does the wrong thing.
- **Find-references never looks inside gems.** A bundle spells `name` tens of thousands of times
  and none are an answer; go-to-definition and symbol search do reach in, because there the answer
  is useful even when you cannot change it.
- **The type hierarchy is exact, and it is the one place that is not a trade-off.** Inheritance
  and mixins are constants, so there is nothing to infer: the chain is the one rubydex linearized
  and the classes below it come from the reverse index it keeps while doing so. What it will not
  show is a class assembled at runtime — `Class.new` with nothing to call it, or an `include` done
  from a variable.
- **Occurrence highlighting inherits the same split, one file at a time.** Locals and instance
  variables are exact, because scope is syntax and needs no types. A method is matched by name —
  the trade above — but confined to the file you are looking at, where the other `render` really
  is likely to be the same `render`.
- **Rename is the one feature that declines rather than degrades**, because it is the only one
  that edits your files. Local variables, parameters and constants are exact and are renamed
  everywhere, across every file in your project. **Methods are refused**: they are matched by
  name, and a rename built on that would change every unrelated `call` and `id` in the project
  while looking as though it had worked. **Instance variables are refused** as well, because a
  subclass or an included module writing the same `@name` is writing the same variable and those
  are not found. In both cases ya-lsp says which it is and why, rather than leaving your editor
  to report only that nothing can be renamed here.

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

### Rename

Press your editor's rename key on a local variable, a parameter or a constant. A constant is
renamed everywhere it is written, in every file, and only where it really is that constant:
`Person` inside `module HR`, `HR::Person` at the top level and `class Person` on the definition
line are one name and all change together, while a `Person` in another namespace does not. Only
the segment being renamed moves, so `HR::Person` becomes `HR::Employee` rather than losing its
namespace.

**Nothing is edited unless every place it would be edited has been read back and confirmed to hold
only that name.** If any one of them cannot be confirmed, the whole rename is declined and says
so — a rename that changed most of the places a name is written would leave code that no longer
runs, which is worse than one that changed none of them.

Four things are declined, each with a sentence saying why:

- **Methods**, and **instance variables** — see the precision list above.
- **A name defined outside your own code.** Renaming `ActiveRecord::Base` would either edit the
  gem or leave the gem defining the old name. That includes a class of your own that reopens one
  a gem or Ruby defines, since only half of it is yours to change.
- **A variable that is also written as a keyword or a hash key.** In `def call(host:)` the name is
  part of what every caller has to write, and in Ruby 3.1's `{ host:, port: }` or `connect(host:)`
  the one word is the key *and* the value. Replacing it there would change the hash rather than
  the variable, and would still parse.

Ruby's own rules decide what a new name may be, because Prism decides it rather than a pattern:
`Ünicode` is a constant and `é` is a variable, `nil` and `_1` cannot be assigned to at all.

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

VS Code exposes every one of these except `gems.max_files`, grouped into Gems, Ruby signatures,
Problems and Indexing, with the TOML key named in each description — the editor spells them
`gems.defaultGems` and `index.maxFiles` where the file spells them `default_gems` and `max_files`.
`ya-lsp.diagnostics.rules` declares all ten rules, so the editor completes their names and says
what each fires on. Two settings are the extension's own and have no TOML twin:
`ya-lsp.serverPath`, and `ya-lsp.logLevel` — both restart the server. Plus the commands **ya-lsp:
Restart Server** and **ya-lsp: Show Output**. A committed `ya-lsp.toml` wins over all of them.

## Development

`make` on its own lists every target. `make setup` installs the two pinned cargo subcommands,
`make ci` runs what CI runs — format, clippy, tests, and a coverage gate at 95% of lines and
branches. Rust 1.93.1, pinned in `.tool-versions`.

`make canary` is the one target that needs a network: it fetches one pinned commit of
[lobsters](https://github.com/lobsters/lobsters) — a real Rails application, BSD-3-Clause — opens
it the way an editor does, and checks that 476 files are indexed, that none of them fails to
parse, that the 14 warnings it produces are still the 14, and that the cold index is inside a
ceiling an order of magnitude above the measurement. It is a canary rather than a benchmark, and
it does not cover gems; CI runs it as its own job.

## Licence

MIT — see [LICENSE.txt](LICENSE.txt). ya-lsp embeds Ruby's own RBS signatures, which are
BSD-2-Clause/Ruby; [NOTICE.txt](NOTICE.txt) carries their notice and
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) the licences of every crate linked in.
