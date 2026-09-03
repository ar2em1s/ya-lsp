# Changelog

The server and the VS Code extension ship as one version, so one file covers both. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`textDocument/rename`, with `prepareRename`.** F2 on a local variable, a parameter or a
  constant. Constants are renamed across every file and only where they really are that constant:
  `Person` inside `module HR`, top-level `HR::Person` and the `class Person` line are one name and
  change together, a `Person` in another namespace does not, and only the segment being renamed
  moves. Measured on solargraph, renaming all 249 of the names its 899 `class` and `module` lines
  declare: **780,163 replacements, every one of them landing exactly on the old name** in the
  ranges the client receives, in UTF-16 units. 0.46 ms median to answer a prepare and 0.80 ms to
  answer a rename; the largest, `Solargraph` itself, is 2,877 edits across 354 files in 140 ms.
- **Nothing is edited until every place it would be edited has been read back and confirmed to
  hold only the old name**, and if one cannot be, the whole rename is declined rather than part of
  it applied. That check is not a formality: rubydex records the name span of
  `Error = Class.new(StandardError)` as the *entire assignment*, so a rename that trusted the span
  would have replaced the class with its new name. Sixty renames were applied over 6,882 files and
  re-parsed, with no file gaining a diagnostic it did not already have.
- **Four kinds of rename are declined out loud, with a sentence saying which and why.** Methods,
  because their uses are found by name alone and the rename would change every unrelated one;
  instance variables, because a subclass writing the same `@name` is writing the same variable;
  anything defined outside your own code, a class of yours that reopens a gem's included; and a
  variable also written as a keyword or a hash key — `def call(host:)`, and Ruby 3.1's
  `{ host:, port: }` and `connect(host:)`, where the one word is the key *and* the value, so
  replacing it changes the hash rather than the variable and still parses. That last rule costs 6
  of 400 real parameters measured, and 389 of the 400 renamed.
- Ruby's own rules decide whether a new name is usable, because Prism decides it rather than a
  pattern of ours: `Ünicode` is a constant where `é` is a variable, `nil`, `_1` and `__FILE__`
  cannot be assigned to, `x!` is a call, and `first second` parses cleanly as something that is
  not a name at all.
- **`textDocument/prepareTypeHierarchy` and both of its follow-ups.** Right-click a class or a
  module and walk the inheritance in either direction. Supertypes are `Module#ancestors` minus the
  class itself — the linearization Ruby's own method lookup walks, so `Comparable` is in `String`'s
  chain and a `prepend`ed module sits above the class that prepends it — and subtypes are the
  mirror of that rather than one generation of it: expanding `Base` lists every class below it,
  not only the ones written `< Base`, out of the reverse index rubydex keeps while it linearizes.
- **An ancestor that could not be resolved is a row that says so**, spelled as it was written and
  placed where it is written — which is not always the class being expanded, since an unresolved
  superclass propagates down the chain. A superclass in a gem that is not installed would otherwise
  leave a chain that reads as complete and is short by everything above the gap.
- Subtypes cost nothing to find and something to draw, so the cap is on the rows rather than on the
  search: measured on solargraph with Ruby's signatures indexed, 0.07 ms median across 899 of its
  own namespaces, 4.3 ms for `StandardError`'s 458 subtypes and 25.1 ms for `Object`'s 1,978.
  Everything solargraph itself defines has at most 65. Reaching the cap is said out loud, because a
  short list of subtypes is indistinguishable from a complete one.
- **`textDocument/foldingRange` and `textDocument/selectionRange`.** Folding now follows the
  syntax rather than the indentation an editor guesses from, and expand-selection walks out
  through what Ruby actually has. Definitions, blocks, lambdas, literals, argument lists, heredoc
  bodies, `begin`/`rescue`/`else`/`ensure`, `case` and each of its branches, `if`/`elsif`/`else`
  as three regions rather than one, runs of comment lines, `=begin` blocks and `#region` markers
  all fold, and the last three carry their proper kind so **Fold All Comments** does what it says.
  Measured over solargraph's `lib/` — 247 files, 26,227 lines — at 5,979 ranges and 0.09 ms a
  request, 0.55 ms on its largest file.
- **Every `end`, `}` and `]` stays on screen.** A collapsed range hides the lines after its first,
  so a `def foo` folded with nothing closing it reads as broken code rather than as folded code;
  and a construct written on one line gets no chevron at all, since clicking it would do nothing.
  Two node locations lie about this and both had to be caught: a `def` with a bare `rescue` in it
  is given an implicit `begin` whose location runs to the *enclosing* `end`, and an `else` clause
  carries the `end` keyword inside its own. A 1,058-line file folds into 207 ranges with every
  closer still on screen.
- **Nothing is answered where nothing was found**, which matters more here than anywhere else: a
  client that has a folding provider stops guessing from indentation, so an empty array would take
  the guess away *and* put nothing in its place. A `null` hands it back.
- Expanding the selection steps through the string's contents before its quotes, one argument
  before the argument list, a method name and then its receiver before the next call in the chain,
  and a body before the `def` and the `def` before the `class`. Every link contains the one before
  it even while the file is half-typed — which is the property the protocol *defines* the response
  by, and the one a parser recovering from an error will happily break. 0.37 ms at every one of
  the 4,131 identifier positions of that file, with a median chain seven links deep.
- Neither request waits for the index to settle, since neither reads it: on a 1,058-line file that
  is 8.0 ms from keystroke to folded outline instead of 14.3 ms.

- **`textDocument/documentHighlight`.** Putting the cursor on a name now lights up every other
  place in the file that means the same thing, with assignments drawn differently from reads.
  Without it an editor matches words, which is wrong in three ordinary ways: it highlights
  `name` inside a comment, inside a string, and in a method that has nothing to do with the one
  you are in. Locals, parameters, block parameters and instance variables are answered by a new
  scope walk over Prism — rubydex models none of them — and constants and methods come from the
  graph, restricted to the file you are looking at.
- The scope walk uses **Prism's own resolution rather than a rule of its own**, so
  `x = 1; [1].each { |x| x }` separates the two `x`es without anything in ya-lsp knowing what
  shadowing is, a block sees the locals around it and a `def` does not, and `rescue => e`, a
  `case/in` capture and a destructured `def f(a, (b, c))` all arrive as ordinary targets. An
  instance variable is scoped by **what `self` is**: `@v` in `def a` and `@v` in `def self.b`
  are two different variables, and a `def c` inside `class << self` shares the second, which is
  what Ruby does and what a highlight joining them would get wrong.
- `null` is answered wherever ya-lsp does not know — a comment, a string, a keyword — which is
  what leaves the editor's own word matching in play for exactly those positions rather than
  replacing it everywhere with an empty list. Measured on solargraph over a 1,059-line file,
  asking at every one of its 4,131 identifier positions the way a moving cursor does: 0.41 ms
  median and 0.96 ms at the 90th percentile, with 1,620 answered and the rest `null`.

- **`textDocument/signatureHelp`.** Typing `(` or `,` inside a call now shows the method's real
  parameters with the one being written underlined — `Person#initialize(name, age = ...,
  *nicknames, admin: ..., **extra, &block)`, from the project, from a gem, or from Ruby's own
  signatures. `Person.new(` shows what `Person#initialize` takes, not `Class#new`. Overloads
  are kept as overloads: RBS declares three arms for `String#gsub` and the editor is handed all
  of them with the one the call fits marked, rather than the first one flattened out of the
  rest. A keyword argument is found by **name**, so `create(role: :admin, name: ` underlines
  `name:` however the two were ordered, and a keyword the method never declared underlines its
  `**opts`. A `*rest` absorbs every positional argument after it, so the fifth argument to a
  three-parameter method is still the splat rather than something past the end of the list.
  Measured on solargraph at typing speed — a `didChange` and a request for every character
  of a call, as an editor sends them — at 2.1 ms median and 3.9 ms worst over 114 keystrokes.
- Signature help fires **only when the callee resolved exactly** — the same rule keyword-argument
  completion has always applied, for the reason the README gives about method matching: for
  `person.shout(` there is nothing to resolve against, and another class's parameter list under
  the cursor while you type into it would be a syntactically valid wrong answer. `null` is the
  honest answer there, and it is what closes the popup rather than leaving a wrong one up.
- **Ruby that changes outside the editor is now indexed as it changes.** A `git checkout`, a
  `git pull`, a rebase or a `rails g model` writes files the editor never opened, and until now
  ya-lsp answered against the tree as it stood when the server started — for the rest of the
  session. A deleted file kept every declaration it had, which is the one place ya-lsp was
  *wrong* rather than merely absent: go-to-definition landed in a file that no longer existed.
  The watcher registration now covers `index.include` as well as `ya-lsp.toml`, and each of the
  three things a watcher reports maps to what it means — created and changed re-index that one
  file, deleted drops it, and everything else is ignored and traced. Measured on a solargraph
  checkout that moves 310 Ruby files between two releases: 478 ms to re-index all of them, and
  555 ms from the notification to a correct answer, against 6 ms to re-index one file.
- Four rules decide whether a watched change is acted on, and each is a way to be wrong that
  nothing else would have caught. **An open buffer beats the disk**, so a rebase under a file
  you are editing does not overwrite what is on your screen. **A gem is not your code**, so a
  `bundle install` is left to the background gem index rather than re-indexing tens of thousands
  of files on the thread that answers requests. **The walk decides what belongs**: the new
  `Workspace::indexes` is the same compiled globs and the same walker `Workspace::discover`
  uses, restricted to the directories between the root and one path, because two implementations
  of `.gitignore` that disagree would mean a file that silently never refreshes — the two are
  asserted against each other over every path in a fixture tree rather than each against a
  hand-written list. And **`index.max_files` still applies**, since a watcher can add the files
  the startup walk stopped before.

- **Every VS Code setting reviewed, regrouped and rewritten**, and five that only `ya-lsp.toml`
  could reach are now in the editor: `ya-lsp.index.include`, `index.exclude`, `index.loadPaths`,
  `index.respectGitignore` and `gems.paths` — the two "why is my code not indexed" settings, and
  the escape hatch for a machine whose gem layout ya-lsp guesses wrong about. Twelve settings under
  one heading became five titled groups, and every description now leads with what you will see
  change rather than with what the server does internally, names its `ya-lsp.toml` twin (the same
  setting is spelled `maxFiles` in one and `max_files` in the other, and nothing mapped them), and
  keeps a measured cost where there is one. Every drop-down explains its choices; none did before.
  `gems.max_files` stays a file-only setting: it is a ceiling nobody has needed to tune, and
  `index.max_files` is the one that fires.
- **The ten diagnostic rules are declared one by one**, so `ya-lsp.diagnostics.rules` completes
  their *names* in settings.json and documents each on hover — what it fires on, and what it
  reports as when you leave it alone. It was an open object: the values completed, the names did
  not, and an empty `{}` was the only hint that ten of them exist. This is how you find
  `parse-warning` in order to switch it off when RuboCop or Standard is already reporting the same
  unused variables — a real project's `.standard.yml` disables exactly that check on purpose, and
  ya-lsp reported all fourteen of them anyway. Two tests keep the second copy of the list honest:
  one in Rust asserting the names are exactly rubydex's ten, and one asserting every severity the
  manifest offers is one the server can read.

### Fixed

- **`ya-lsp.logLevel: "off"` no longer produces more output than any other value in the list.** It
  was treated as "the user said nothing", which fell through to the server's own fallback of
  `info` — louder than the `warn` or `error` sitting above it in the same drop-down. `off` is a
  perfectly good filter directive and is now sent as one. An unset setting still leaves a
  `YA_LSP_LOG` inherited from your shell alone, which is the case that branch was written for.
- **The same setting documented a default the server does not have.** The manifest said `warn`;
  the server does `info`. Defaults are never sent — only settings you actually set are — so it was
  documentation and nothing else, and it was wrong on the one setting anybody reads once something
  has already gone wrong. Every documented default is now checked against the server's own by a
  test that reads the manifest.
- **Settings can be set per folder in a multi-root workspace again.** Everything the server reads
  is now `scope: "resource"`. Only two of the twelve settings declared a scope at all, so the
  other ten were `window`-scoped, and VS Code does not read a `window`-scoped setting from a
  folder's `.vscode/settings.json` — while the extension has always read them per folder, because
  it runs one server per folder. Folder A could not be given different gem settings from folder B
  however plainly they were written. A test over the manifest now asserts the scope of every
  setting the extension sends, because two releases of review did not catch this one.
- **ya-lsp no longer dies silently when rubydex's resolver crashes.** rubydex 0.2.5 panics
  inside `Resolver::resolve` after a document is deleted — `Graph::delete_document` invalidates
  first and untracks the deleted document's strings second, so the work the invalidation queued
  can name a string that is no longer there. Reproduced by deleting one file
  (`lib/solargraph/yard_map/to_method.rb`) from a solargraph v0.58.2 checkout. This was already
  reachable in 0.2.0, through `didClose` on a file that had been deleted, and the file watcher
  above would have made it an ordinary Tuesday. Uncaught it is the worst failure this server
  has: the analysis thread dies, the editor keeps sending requests, and a language server that
  answers nothing at all is indistinguishable from one that is thinking, so nobody restarts it.
  There is no newer rubydex to upgrade to, so it is contained: the panic is caught, you are told
  in one sentence, and the index is rebuilt from scratch, which is the only honest recovery from
  a graph that crashed half way through being linked. A rebuild that crashes again stops rather
  than recurring.
- A panic anywhere else on the analysis thread now ends the server instead of leaving it
  running with nothing behind it. Every editor restarts a language server that stops and none
  restarts one that goes quiet.
- **Typing `@` no longer offers `$@`.** A sigil is not a letter to fuzzy-match on: `@foo`,
  `@@foo` and `$foo` are three different namespaces in Ruby, and the subsequence match that
  makes completion forgiving about spelling was reading the sigil as one more character — so a
  global whose entire name after the `$` is an `@` answered an instance-variable prefix. A prefix
  now only reaches names carrying the sigil it asked for, with the one asymmetry Ruby needs: `@`
  still offers class variables, because the second `@` may be the next thing you type, and `@@`
  offers no instance variables, because nothing you can type turns one into the other.
- **A `class << self` inside a nested class hovers as `class << Shelf::Book` rather than
  `class << Book`.** Every other card on the same page qualifies the name; this one read the
  attached class out of rubydex's `Shelf::Book::<Book>`, where it is written unqualified. Which
  `Book` it meant was a guess in any project with two of them.

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
