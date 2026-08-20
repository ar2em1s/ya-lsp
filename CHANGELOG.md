# Changelog

The server and the VS Code extension ship as one version, so one file covers both. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-08-20

First release.

ya-lsp resolves constants and does not infer types. That line runs through every feature below, so
it is worth stating once rather than discovering: constants are exact, and methods are matched by
name wherever the receiver cannot be named. The README says which side of it each feature falls on.

### Added

- **Diagnostics.** Reported as the workspace is indexed, and only for your own code — a vendored
  bundle sits inside the workspace root by construction, so gem directories, Ruby's own library
  and the signature root are excluded. Most rules ship `off`, because they fire on correct Ruby;
  `ya-lsp.diagnostics.rules` turns individual ones back on by name.
- **Go to definition, hover, and document symbols.** Answered from a resolved graph rather than
  from matching text, so `Person` inside `module HR` and top-level `HR::Person` are one constant,
  and an unrelated `Person` that merely shares the name is not.
- **Workspace symbol search and find-references.** The picker ranks your own code above match
  quality on purpose: a project has ~3k declarations and its bundle ~150k. Find-references looks
  only at your own code, and says so when it reaches its cap, because a truncated result looks
  exactly like a complete one.
- **Completion.** `Foo::`, `Foo.`, `self.`, a bare word, and a call in a class body walk the real
  ancestor chain and apply real visibility, so a `private` method is offered inside its class and
  nowhere else. Literals are typed by reading the parse — `"hello".` reaches `String`'s methods,
  not a word list — and a local assigned a literal earlier in the same scope carries the type
  along. Keyword arguments are offered only when the method resolved exactly. Nothing fires inside
  a comment or a string, but `#{}` fires again, because that is code.
- **Gems, read from disk.** `Gemfile.lock` and the gem directories are parsed directly. ya-lsp
  never executes Ruby and never shells out to `ruby`, `bundle`, or `gem`, so it works on a machine
  where the project's Ruby is not installed. Indexing runs in the background behind `$/progress`.
- **Ruby's own core and standard library.** `String`, `Array`, `Hash`, `Integer`, `Kernel` and the
  ~60 stdlib signatures have members, documentation, and a file to jump to. They come from the
  `rbs` gem when the machine has one and from a copy embedded in the binary when it does not.
  `require "json"` navigates.
- **`ya-lsp.toml`**, read from the workspace root and overriding whatever the editor sends, so a
  team can commit one setup that works in every editor. Changes take effect without a restart.
- **VS Code extension**, bundling a server binary for the platform it was built for. One server
  per workspace folder; the first starts with the window and the rest when a Ruby file inside them
  is opened. Ships for `darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`, `win32-x64` and
  `win32-arm64`.
- **Standalone server archives** for any other LSP client, each carrying the binary, `LICENSE.txt`,
  `NOTICE.txt`, `README.md`, `THIRD-PARTY-NOTICES.txt` and this file. `ya-lsp --licenses` prints
  all of it from inside the binary, so a copy that travels alone still carries what it owes.

### Known limitations

- **Methods on a receiver that cannot be named are matched by name.** For `person.name`, where
  `person` did not come from a literal, there is nothing to resolve against — find-references then
  returns every call spelled that way in your project. For an unusual name that is the answer you
  wanted; for `call`, `name`, or `id` it is a scoped text search with a nicer interface.
- **Rails' `validates`, `has_many` and `scope` are not offered.** `ActiveSupport::Concern` installs
  them at runtime from an `included` hook, so they exist in no source file that could be read. The
  class methods that *are* written down are offered normally.
- **`define_method` and friends** define names that only exist while the program runs.
- **A local variable's type comes from the textually last preceding assignment**, which is the one
  place ya-lsp can be confidently wrong rather than merely absent.
- **A Ruby file outside every workspace folder gets no server**, because there is no root to index.

[Unreleased]: https://github.com/ar2em1s/ya-lsp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.1.0
