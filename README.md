# Y(et) A(nother) LSP

The ya-lsp is a yet another implementation of the language server protocol for Ruby, which focuses on being lightweight, fast and standalone.

Standalone means what it says: ya-lsp never executes Ruby, and never shells out to `ruby`,
`bundle`, or `gem`. It reads `Gemfile.lock` and the gems on disk itself, so it works on a machine
where the project's Ruby is not even installed.

## How precise are the answers?

ya-lsp resolves constants and does not infer types. That line runs straight through the middle of
every feature, so it is worth stating rather than discovering:

- **Constants — classes, modules, and `CONSTANT` names — are exact.** Go-to-definition, hover and
  find-references work from a resolved graph, not from matching text. `Person` written inside
  `module HR` and `HR::Person` written at the top level are understood to be the same constant,
  and a different top-level `Person` that merely shares the name is understood not to be.

- **Ruby's own classes are there.** `String`, `Array`, `Hash`, `Integer`, `Kernel` and the rest of
  core have their methods, their documentation and a file to jump to, and so do the ~60 standard
  library signatures — `CSV`, `URI`, `Logger`, `OptionParser`. They come from the `rbs` gem when
  the machine has one, and from a copy inside the ya-lsp binary when it does not, so this works
  with no Ruby installed like everything else here. `require "json"` navigates too.

- **Methods are matched by name.** ya-lsp uses the receiver when it can name one — `Foo.bar`,
  `self.bar`, a literal like `"hello".`, `Foo.new.`, and a call in a class body all resolve
  properly — but for `person.name`, where `person` came from anywhere but a literal, there is
  nothing to resolve against. Find-references on a method
  therefore returns every call spelled that way in your project. For an unusual name that is the
  answer you wanted; for `call`, `name`, or `id` it is a scoped text search with a nicer interface,
  and ya-lsp does not pretend otherwise.

  Closing this means inferring the type of every expression, which is a different project. If you
  need it today, solargraph does it.

- **Find-references only ever looks at your own code**, never inside gems — for methods because a
  bundle spells `name` tens of thousands of times and none of them are an answer, and for
  constants because the result is a work list and nobody is going to edit a gem. Go-to-definition
  and the symbol search do reach into gems; those are questions where the answer is useful even
  when you cannot change it.

## Installing

**VS Code** — install the extension from `editors/vscode`. It bundles a `ya-lsp` binary for your
platform, so there is nothing else to install and nothing to add to your `Gemfile`. Point
`ya-lsp.serverPath` at a binary of your own to develop against a local build.

**Any other LSP client** — download the archive for your platform from the
[latest release](https://github.com/ar2em1s/ya-lsp/releases/latest), or build with
`cargo build --release`. Either way, run `ya-lsp --stdio`.

```bash
tar xzf ya-lsp-aarch64-apple-darwin.tar.gz
./ya-lsp-aarch64-apple-darwin/ya-lsp --version
```

Archives rather than bare binaries, because a release asset downloaded over HTTP arrives without
its executable bit. Each holds the binary, `LICENSE.txt`, `NOTICE.txt`, `README.md` and
`THIRD-PARTY-NOTICES.txt`. `SHA256SUMS` covers every asset on the release; `sha256sum -c
SHA256SUMS` checks whichever ones you downloaded.

`ya-lsp --licenses` prints all of it — ya-lsp's own terms, the notice for the Ruby signatures
embedded in the binary, and the licences of every crate linked into it. It is inside the binary
rather than only beside it, so a copy that travels on its own still carries what it owes.

Configuration lives in a `ya-lsp.toml` at the workspace root, which overrides whatever the editor
sends, so a team can commit one setup that works in every editor. Changing it takes effect without
a restart.

## Completion

The same line runs through completion, and you can see which side you are on from what you typed.

`Foo::`, `Foo.`, `self.` and a bare word are **exact**: ya-lsp walks the real ancestor chain and
applies real visibility, so a `private` method is offered inside its class and nowhere else, and
`Foo.` offers class methods rather than instance ones. Inside a call's parentheses you also get
the keyword arguments that method actually takes — and only when the method was resolved exactly,
because a wrong keyword argument is a syntactically valid wrong answer.

A literal is exact too. `"hello".`, `[1, 2].`, `{}.`, `42.`, `:name.` and `Foo.new.` are typed by
reading what the parser already decided, so they reach the real class — `String`'s 206 methods,
not a word list. A local variable assigned one of those earlier in the same scope carries the type
along: write `greeting = "hello"` and `greeting.` knows. That last one is the one place ya-lsp
guesses in a way that can be wrong rather than merely absent — reassign the variable inside a
branch and it will answer with whichever assignment is textually last.

Everything else after a `.` is a **guess**. `foo.bar.` , an instance variable, a method's return
value: there is no type to look up, so the list is every method name in your project,
deduplicated, with your own code first. That is less than an editor with type inference gives you
and more than one without it: your editor's built-in suggestions cannot see a method defined in a
file you do not have open.

Two things Ruby projects lean on that no static tool sees:

- **Rails' `validates`, `has_many` and `scope` are not offered.** `ActiveSupport::Concern`
  installs them at runtime from an `included` hook, so they exist in no source file that could be
  read. The class methods that *are* written down — everything `ActiveRecord::Base` declares with
  `def self.` or `class << self`, about 125 of them — are offered normally.
- **`define_method` and friends** define names that only exist while the program runs.

Completion never fires inside a comment or a string literal: ya-lsp parses rather than scanning
backwards from the cursor, so it knows the difference. Inside `#{}` it fires again, because that
is code.
