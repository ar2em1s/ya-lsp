# Changelog

The server and the VS Code extension ship as one version, so one file covers both. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] — 2026-08-25

### Added

- The Ruby version is now looked for in every directory from the project up to your home
  directory, not only in the project itself. `.ruby-version` and `.tool-versions` are how rbenv,
  chruby, RVM, asdf and mise are told which Ruby a *tree* of projects uses, and every one of
  those tools walks the ancestors — a file one directory up is the documented way to say
  "everything under here uses this Ruby", and ya-lsp read none of them. Measured over eleven real
  Ruby workspaces on one machine: six went from "no Ruby version" to a resolved one, and all six
  gained Ruby's own library with it. The nearest directory wins whichever kind of file it holds,
  and the whole chain now outranks `RUBY VERSION` in `Gemfile.lock`, which records what the last
  person to run bundler used rather than what will run this code. The startup log line names the
  file that answered, by path, because with a chain to walk "from a .tool-versions" no longer
  names one.
- ya-lsp says so when it cannot tell which Ruby a project uses. Ruby's own library — `json`,
  `uri`, `forwardable` and the other ~40 default gems, 727 files on a 3.4 install — is indexed
  only when a `.ruby-version` or a `.tool-versions` — in the project or above it — or a
  `RUBY VERSION` in the lockfile says which Ruby to look for. ya-lsp refuses to guess, because guessing once put macOS's vestigial Ruby 2.6
  standard library into the index and answered `"hello".u` with `unspace`. Refusing is right;
  refusing in silence was not, and for an ordinary gem or script — which often has none of those
  three files — it cost the whole standard library with nothing said. `require "json"` answered
  nothing, `JSON.parse` hovered as nothing, and the only trace anywhere was a debug line about
  the *bundle*, which is not what went missing. There is now a message naming what was skipped
  and what to do about it, and a second one for the case where the version is known and that Ruby
  is not installed. Both name `gems.default_gems = false` as the way to silence them.
- The first line ya-lsp logs now names its version. A pasted log was the one thing a bug report
  reliably carried and the one thing that never said which build wrote it — `--version` answers
  that, but nobody runs it on a server their editor spawned. It is logged before the handshake
  rather than beside the workspace line that follows it, because a client that sends a malformed
  `initialize`, or none at all, is precisely when the question gets asked and precisely when the
  later line never runs. Spelled `ya-lsp <version> starting`, the same way `--version` spells it,
  so one pattern finds both.

### Changed

- Completion is ranked by how close a method's owner sits on the receiver's ancestor chain, so a
  list opens on the receiver's own members. Previously, with nothing typed after the dot, nothing
  in the ranking key varied and the list came out alphabetical: `"hello".` opened on
  `DelegateClass`, `Digest`, `append_as_bytes`. It now opens on `append_as_bytes`, `ascii_only?`,
  `b`, and `Object`, `Kernel` and `BasicObject` sort to the bottom where they belong. The same
  applies to the cap — a truncated list keeps the nearest names rather than the alphabetically
  first ones.
- Completion on a receiver whose type cannot be known — an instance variable, another call's
  return value — is ranked by how near the code is to the cursor: the current file first, then
  outwards by directory. It was alphabetical, so `@foo.` in a Rails app opened on `account_type`
  and `add_bank_account_to_bank_accounts` regardless of where the cursor was. It now opens on the
  file's own methods and then its neighbours'. This is still a guess — ya-lsp does not infer types
  — but it is a guess made from where you are rather than from the alphabet.
- Go to definition and hover on the `new` in `Foo.new` answer with `Foo#initialize`. `Foo.new`
  really is `Class#new`, so the exact answer was `core/class.rbs` and a signature reading
  `(*args, **kwargs, &block)` — measured on a Rails app, all 25 `.new` call sites landed there.
  They now land on a constructor with its real parameter list, gems and Ruby's own classes
  included: `Setting.new` reaches `ActiveRecord::Core#initialize`, `Class.new` reaches
  `Class#initialize`. Keyword arguments complete inside `Foo.new(` for the same reason. A class
  that writes its own `def self.new` keeps that answer, since it is the method the call reaches,
  and a class with no constructor of its own keeps `Class#new` rather than being sent to the
  empty one every object inherits.

### Fixed

- Methods declared inside an RBS `interface` are no longer treated as methods of the surrounding
  class. RBS interfaces describe a shape a value can have, not a class anyone can call — but they
  were indexed as though their contents belonged to whatever enclosed them, which for most of them
  was `Object`, the ancestor of everything. `"hello".` offered `begin`, `exclude_end?`, `read` and
  `rewind`; `[].rand` and `Object#each_entry` appeared in the symbol picker and as
  goto-definition targets. Ruby's own signatures declare 99 of these blocks, and on an ordinary
  receiver they were 27 of the 30 suggestions sitting between the class's own methods and
  `Kernel`'s.
- Completion no longer offers a method Ruby would refuse to call. `initialize` was suggested on
  every receiver — `"str".initialize`, `Foo.new.initialize` — because Ruby privatises it at the
  point of definition and neither RBS nor the index records that; the same went for
  `initialize_copy`, `initialize_clone`, `initialize_dup` and `respond_to_missing?`. A private
  method was also offered on any receiver that happened to be the same class as the caller, so
  `other.secret` was suggested inside the class declaring `secret`, where Ruby raises
  `NoMethodError`. All five names and every private method are still offered where Ruby allows
  them: with no receiver written, or on one written `self`.
- The outline no longer disappears while a `def` is being typed. Typing the keyword inside a class
  made VS Code throw `selectionRange must be contained in fullRange` and discard the whole
  `textDocument/documentSymbol` response, so the file's structure vanished until another keystroke
  fixed it. Prism recovers a bare `def` into a node whose span is the three keyword bytes and
  whose *name* span is the whitespace after them, and the protocol requires the second to sit
  inside the first. Go to definition carried the same broken pair in `targetSelectionRange`, where
  it parked the cursor outside the construct it named. Half-typed code is the normal state of a
  buffer, so the containment rule is now enforced wherever the pair is sent.
- A half-typed `def` no longer adds a blank row to the outline. There is no method there yet.
- An anonymous rest, keyword-rest or block parameter is written once in a hover. `def f(*, **, &)`
  is ordinary Ruby 3 and each of the three came out doubled — `**`, `****`, `&&`.
- Hover no longer loses the markup Ruby's own documentation is written in. RDoc extracted those
  comments from Ruby's C source and left HTML in them — 1,867 `<code>` spans in the signatures
  ya-lsp carries, plus `<em>`, `<strong>`, `<tt>`, `<b>` and `<i>` — and an editor renders a hover
  as markdown, which means it strips them. `<code>:ascii</code>` arrived as a bare `:ascii` with
  no code formatting; worse, prose that merely *looks* like a tag (`<vowel>`, `<rhs>`, `<main>`)
  vanished along with everything up to the next `>`, invisibly. Each is now the markdown that
  means the same thing, and anything angled that is not markup keeps its brackets. Ruby in an
  example — `Hash<Symbol, untyped>` — is left exactly as written.
- RDoc's own cross-references no longer render as dead links. `[Case Mapping](rdoc-ref:…)` points
  into a documentation tree an editor has never seen; there are 912 of them in Ruby's core
  signatures alone. The words stay, the link goes.
- Every hover card now puts what ya-lsp knows *about* an answer in the same place: an italic line
  under the answer, one per fact. A guessed single match already did this; a guessed list said the
  same sentence in bold at the top, after an em dash. Same uncertainty, same words, two shapes.
- The messages ya-lsp shows in the editor are written to one rule. Settings are named the way
  `ya-lsp.toml` spells them (`gems.max_files`, not `[gems].max_files`); what happened and what it
  means for you are joined one way rather than three; there is a remedy wherever ya-lsp knows one;
  and the internal phrase "gem intelligence is incomplete" is gone in favour of what actually
  stops working. Two of the messages were the same sentence written out twice in two files, so a
  fix to either left the other.
- `ya-lsp.toml` now reloads without a restart in every editor, not only in VS Code. 0.1.0 said
  "changes take effect without a restart" and that was true in exactly one client: nothing in the
  server ever sent `client/registerCapability`, and the VS Code extension covered the gap with a
  file watcher of its own. Neovim, Helix, Emacs and everything else edited the file and waited for
  nothing to happen. The server now asks the client to watch the file itself, during the
  handshake, and says so in the log when the client is one that cannot be asked — which is the
  only case left where a restart is needed. The extension's own watcher is gone with it: two
  watchers on one file meant the reload ran twice per save, and a reload re-reads the
  configuration and re-indexes the bundle.
- A change to a watched file that is not `ya-lsp.toml` no longer re-indexes the project. File
  watchers belong to the editor and are shared across every language server it runs, so ya-lsp
  could be handed changes it never asked about — and it answered every one of them by dropping
  the whole index and re-reading the bundle. In an editor that watches broadly that was a full
  re-index per saved file.
- Projects in a directory whose name contains `[`, `]`, `^` or `|` work. Every answer that names
  a file — diagnostics, go-to-definition, find-all-references, the outline, the symbol picker —
  was silently dropped for them: the two URL standards involved disagree about those four
  characters, one writing them literally and the other refusing to parse them, and the conversion
  failed on the way out to the editor. A project under `~/work/[wip]/app` got no squiggles and no
  navigation, with nothing said anywhere.
- A multi-root workspace no longer starts a server on its first folder regardless of what is in
  it. Activation is `onLanguage:ruby`, so opening a Ruby file anywhere started a server on the
  first folder the `.code-workspace` lists — which says nothing about whether it holds Ruby. A
  workspace whose first folder is infrastructure, docs, or a service in another language got that
  folder indexed, found nothing, and warned about it, for a folder the user had not opened and
  had no reason to think ya-lsp knew about. The eager start exists so a single-folder project —
  nearly all of them — has a warm server before the first keystroke, and now happens only when
  the workspace has exactly one folder. Every folder of a multi-root workspace starts when a Ruby
  file inside it is opened, which is what folders two onward always did.
- A folder with no Ruby in it is no longer reported as a misconfiguration. "Nothing matched
  index.include" carried a remedy — widen it, or check what is excluding everything — that is
  wrong advice for someone who never narrowed anything, and a folder with no Ruby beside a folder
  with Ruby is an ordinary thing to have open rather than a mistake. The warning now fires only
  when `index.include` or `index.exclude` was written by hand and matched nothing, which is the
  case it was always for; untouched globs over a folder with no Ruby say so in the log instead,
  naming what the folder loses rather than what was looked for.

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

[Unreleased]: https://github.com/ar2em1s/ya-lsp/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.2.0
[0.1.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.1.0
