# Changelog

The server and the VS Code extension ship as one version. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] — 2026-09-10

### Added

**Rails**

- **A model answers ActiveRecord's whole query interface.** `Story.create!`, `Story.pluck(:title)`,
  `Story.find_each`, `Story.update_all`, `story.comments.delete_all` — the list is Rails' own
  (`ActiveRecord::Querying::QUERYING_METHODS` in full, plus the class methods in
  `ActiveRecord::Persistence`). How the call is written decides the answer: `Story.first` is a
  `Story` and `Story.first(3)` an array; `Story.select(:id)` a relation and `Story.select { }` an
  array. A name whose type depends on your database — `pluck`, `pick`, `minimum`, `calculate`,
  every `async_*` — resolves and says nothing about its return. Each name lands only on the side
  Rails puts it on: `Story.size` and `Story.each` raise in Ruby and are not offered. A relation is
  an `Enumerable`, so `User.where(...).select(:id).index_by(&:id)` resolves through it.
  <br>The interface is declared **once for the project**, on the base class every model inherits,
  so the card names `ActiveRecordRelation` or `ActiveRecord::Base` rather than your model. On a
  large application that is an order of magnitude fewer generated declarations, and it is why
  editing a model file is no longer slow. `Story.find(1)` is a `Story` and `Story.find` is nothing,
  because how many arguments a call wrote already tells signatures apart. Nothing in the
  interface is a place to jump to — no line of anybody's code declares `Story.where` — so a
  hover says so and go-to-definition declines rather than guessing. `Story.where.not(...)` is
  the one shape left out.

- **An association answers as much of itself as Rails writes.** `story.user = current_user`,
  `story.comments = []`, `story.comment_ids`, `story.comment_ids = [1, 2]`, `story.build_user`,
  `story.create_user`, `story.create_user!`, and `.create!`, `.build`, `.new`, `.reload` on the
  collection. Each jumps to the `belongs_to` or `has_many` that declared it.
  <br>The types are Rails': the writer takes and returns `nil` even where the association is
  required, because `story.user = nil` fails at *save*; `create_user` hands back a `User` even
  where `belongs_to :user, optional: true` reads a `User?`. `comment_ids` is an `Array` of whatever
  your primary keys are — an `Integer` would be wrong for every `id: :uuid` table.
  <br>Left out: `reload_user`, `reset_user`, `user_changed?`, `user_previously_changed?` — almost
  never written, and each costing a declaration per association. A polymorphic
  `belongs_to` still declares nothing.

- **A `scope` in a concern is a class method of every model that includes it.** `scope :expired`
  inside `app/models/concerns/expireable.rb` is `Poll.expired` *and* `Invite.expired`, each
  returning a relation of its own class, and both jump to the one `scope` line in the concern. A
  concern nobody includes declares nothing. A class with no table behind it is left alone. Where a
  `scope` of one name is written in both the concern and the model, both are real and the card says
  "Defined in 2 places".

- **A concern's associations type the classes that include it.** `has_many :comments` inside
  `included do` types `story.comments` in every class that says `include Storyish`, and the jump
  lands on the macro in the concern. `with_options` is read too and hands its keywords down:
  `with_options class_name: "Account", optional: true do belongs_to :approved_by end` is an
  `Account?`. Not declared: a concern's `enum` (its column belongs to the includer), or anything in
  `class_methods do`. Swept over five real applications: **better in many places, worse in none.**

- **`db/structure.sql` is read**, so a project on `schema_format = :sql` gets column types too.
  `story.title` is a member returning `String`, the card names the table and nullability, and
  go-to-definition lands on the line of SQL. Every dump Rails produces is read from one reader —
  pg_dump, mysqldump, `sqlite3 .schema` — with no new dependency. A secondary database's
  `db/<name>_structure.sql` is read too, and a table both a `structure.sql` and a `schema.rb`
  declare is declared by neither.
  <br>The file is **watched but never indexed**: editing and saving re-derives the columns, it
  produces no diagnostics, and it does not appear in the symbol picker. A project that excludes
  `db/` gets nothing read.

- **`story_path` goes to `resources :stories`.** Every helper `config/routes.rb` names is a method
  returning `String`, hovering with and jumping to the DSL line. Read: `resources`, `resource`, the
  seven verbs, `root`, `namespace`, `scope`, `member`, `collection`, `concern`, `only:`/`except:`,
  `as:`, `path:`, `shallow:`, and `draw :admin`. Checked against Rails' own router over six real
  applications: ya-lsp names nearly every helper `ActionDispatch::Routing::RouteSet` does, invents
  next to nothing (and what it invents is a real route behind an `if !Rails.env.production?`), and
  matches exactly on half of them. Exact in a controller, mailer or helper module; matched by name
  in a spec or template.
  <br>Not read: `mount`, `direct`, `resolve`, `devise_for`. An application that declares a
  `RouteHelpers` constant of its own keeps it, and this feature then says nothing.

- **A mailer's actions and a job's `perform` are class methods.**
  `UserMailer.welcome(user).deliver_later` resolves `welcome` to the `def welcome` and chains
  through `ActionMailer::MessageDelivery`. A class whose superclass ends `Job` (or is
  `ActiveJob::Base`) and defines `def perform` answers `perform_later` and `perform_now` with
  `perform`'s own parameters; one including `Sidekiq::Worker` or `Sidekiq::Job` answers
  `perform_async`, `perform_in`, `perform_at`. All jump to the `def`.
  <br>The gate is what the class **inherits or includes**, never the `def` alone: across six
  applications, hundreds of classes define a public `def perform` and are service objects rather
  than jobs. Not read: a `private` action, a `def self.`, `def initialize`, a name RBS cannot spell,
  or a `module` that includes `Sidekiq::Worker`.

- **`enum` is read, in both spellings.** `enum :status, { draft: 0, published: 1 }` declares
  `status`, `status=`, `Story.statuses`, and per value `draft?`, `draft!`, `Story.draft`,
  `Story.not_draft`. Going to `published?` lands on the `published: 1` you wrote. `prefix:`,
  `suffix:`, `scopes: false` and `instance_methods: false` are honoured, and the class-side scopes
  return the same relation class a `has_many` does. The older `enum status: { … }` spelling that
  Rails 8.1 removed is read too; one application in the corpus still writes it throughout.
  <br>**An `enum` fixes the type of the column underneath it**: `story.status` is the label, a
  `String`, not the integer the column holds. A non-literal value list still declares the
  attribute; an unspellable label is declined rather than transliterated. Swept over four
  applications: **better in many places, worse in none.**

- **Twenty-five more macros name members, and four that look like they should do not.**
  `class_attribute :setting` → `setting`, `setting=`, `setting?` on both sides;
  `store_accessor :settings, :colour` → `colour`, `colour=`, `colour_changed?`;
  `accepts_nested_attributes_for :author` → `author_attributes=`;
  `alias_attribute :headline, :title` → all three, **typed as the column it aliases**;
  `has_secure_password` → 8 names; `has_secure_token :auth` → `regenerate_auth`;
  `composed_of :balance, class_name: "Money"` → a `Money`; `has_one_attached :avatar` → an
  `ActiveStorage::Attached::One`; `has_rich_text` → an `ActionText::RichText`;
  `serialize :codes, type: Array` → an `Array`. Together they name hundreds of members across
  five applications.
  <br>Declaring nothing: `normalizes`, `encrypts` and `generates_token_for` (none defines a method
  per call, per Rails' own source), and `helper_method` (its method lives on the *view*). No macro
  declares a name your own `def` in the same class already answers.

- **`attribute :count, :integer` is a member, and the cast type is what makes it one.**
  `attribute :price, :decimal` is a `BigDecimal?`, jumping to the `:price` you wrote, and Rails'
  rule that a cast type **overrides** the column is followed — the column withdraws, so
  `story.price` is one declaration with one type.
  <br>A call naming **no** cast type declares nothing. `active_model_serializers` and
  `jsonapi-serializer` both spell a macro `attribute`, neither defines a method, and a call with no
  cast type is exactly their shape: most `attribute` calls across five applications are a
  serializer's.

- **`delegate :name, to: :user` is a method, and it goes to the `:name` you wrote.** `prefix: true`
  names it `user_name`, `allow_nil: true` makes it optional, `private: true` changes nothing.
  <br>The **type** is followed two hops where both answer, so `story.username.upcase` chains
  through `belongs_to :user` to the `users.username` column. Where a hop cannot be answered the
  method is **still declared, untyped** — the jump and the hover are most of what a `delegate` is
  worth, and an untyped member behaves in a chain exactly as no member would. A column of the same
  name wins; a `delegate` onto another `delegate` is declared and not typed.

- **A Rails engine shipped as a gem is indexed, models and all.** `ActiveStorage::Blob`,
  `Devise::Mailer`, `SolidQueue::Job` — every class an engine keeps under `app/` rather than `lib/`
  — now has a definition, a hover card and a jump. Over four applications, **constant references
  gain a definition and none loses one.** An engine's own macros, YARD tags, mailers and
  jobs are read, so a chain can run through one; the code is still not the user's own — no
  diagnostics, no rename, and `workspace/symbol` answers with your project alone.
  <br>An engine's `config/routes.rb` is read only where it names *your* helpers: activestorage,
  actionmailbox and turbo-rails draw into your route set; blazer, pghero and mission_control-jobs
  draw into their own and are ignored. Checked against Rails' own router: every helper named,
  none invented, none missed.

- **`has_and_belongs_to_many` is read like the `has_many` it is.** Rails implements the macro by
  calling `has_many`, so `class_name:` is honoured and the jump lands on the macro. It was left out
  on the evidence of six applications reporting zero uses — every one of which runs the RuboCop cop
  that forbids it.

**Not Rails**

- **A block parameter is typed by what the method says it yields.** RBS writes both halves in one
  line — `def each: () { (Story) -> void } -> Story::Relation` — and ya-lsp read only the return.
  So `.each do |story|` looked as though it worked and stopped working the moment you renamed the
  variable, and `.each do |instance|` answered nothing. Now
  `Story.where(...).each { |row| row.title }` types `row` from the signature, `"x".tap { |it| … }`
  hands `it` a `String`, and `5.times { |i| i.abs }` an `Integer`.
  <br>It answers nothing rather than guessing where the signature does not say: disagreeing
  overloads, a parameter typed `untyped` or by a type variable (which is why `[1, 2].each` still
  declines), and a block on a receiverless call. An assignment inside the block is asked **first**.

- **`Struct.new(:x, :y)` and `Data.define(:x, :y)` declare their members.** Assign either to a
  constant or inherit from it, and every name becomes a member: readers and writers for a `Struct`
  plus `[]`, `each`, `members`; readers only for a `Data` plus `with`, `to_h`, `deconstruct_keys`.
  `point.x` hovers with the call and **jumps to the `:x` you wrote**; `coord.with(lat: 1).lng`
  chains. Declares nothing when the class has no name (`Struct.new(:state).new({})`) or the names
  are not literal (`Struct.new(*NAMES)`). A `def` in the block keeps the member it replaces.

- **A bare call in an ERB template resolves and completes.** `<%= current_user %>` and
  `<%= time_ago(story.created_at) %>` have always jumped by matching the method name across the
  project, and completed to nothing. ya-lsp now knows what Rails puts in front of a template: every
  module under `app/helpers/**/*_helper.rb`, plus the controller methods exported with
  `helper_method`. Both hover and completion answer from the same list, and the card says which
  convention it came through.
  <br>A controller method that is **not** exported stays out. Where a helper module and an export
  both write a name, the export wins, as in Rails. A mailer's views get what the mailer asked for by
  `helper` and not every application helper. A template whose controller does not exist — most
  partials under `shared/` — still gets the helpers.

- **Four refactorings, on a server that still never runs Ruby.** `textDocument/codeAction` offers
  **extract to local variable**, **extract to method**, **toggle block style**, and **declare an
  `attr_reader` / `attr_writer` / `attr_accessor`**. The fifth family — autocorrecting a style
  offence — is RuboCop's, served over its own `codeAction` from 1.89.
  <br>Refusing is most of what this does. Not offered: an extraction that would hand a value back;
  one moving a `return`, `yield` or `super` where it means something else; one re-indenting a
  multi-line string; a block toggle where the two delimiters bind to different calls
  (`puts show [1, 2].map { … }`); an accessor for an `@count` inside `def self.count`; an
  expression lifted out of an `&&`, a ternary arm, a loop condition or a parameter default. Nothing
  is offered in a file that does not parse, in a template, or in a gem.
  <br>Every action is applied to a copy and re-parsed before being offered. Swept over a real
  Rails application: **every action offered leaves a file that still parses**, and a handful that
  every other guard allowed were caught by this last one.

**Types**

- **The answers have three tiers, and every one says which it is.** Before v0.4.0 a method call was
  exact or matched on its name. There is a rung between them: a receiver ya-lsp *derived* — from a
  return type Ruby's own signatures declare, or from an assignment in the same class. A hover card
  names what was followed. On a real Rails application, more calls with an explicit receiver are
  exact than before and none lost the answer it had.

- **Return types, read from Ruby's own RBS signatures.** `"hello".upcase.` reaches `String`,
  `"hello".length.` reaches `Integer`, and chains compose. What is *not* taken matters as much: a
  union, an interface, `untyped`, a type variable and a method whose overloads declare different
  classes are all dropped. Overloads told apart by a **block** or by **how many arguments the call
  wrote** are not a union: `"x".bytes.` is an `Array` and `"x".bytes { }.` a `String`, `3.7.round.`
  an `Integer` and `[1, 2].first(3).` an `Array`. A call the signature cannot accept — `"hi".scan`
  with no pattern — answers nothing rather than the nearest arm. Thousands of methods across the
  vendored signatures carry a usable type.

- **`@foo` is typed from the assignments in its own class.** `@title = "quarterly"` in `initialize`
  types `@title.` in every instance method. Instance variables are scoped the way Ruby scopes them,
  so a `@seed` in `def self.build` never answers for the `@seed` in an instance method.

- **Your database columns are methods.** ya-lsp reads `db/schema.rb` — it is Ruby, and the parser
  was already linked. `@story.title` completes, hovers and goes to the `t.string "title"` line;
  `@story.created_at.year` keeps going. **A column that is not `null: false` is typed as nullable**,
  which is easy to forget: a large share of the columns in a real application can be `nil`. The card names the file, table, column, schema type and nullability. A table is matched to
  a class by pluralizing, so a table no class claims and a class no table matches both declare
  nothing; `self.table_name = "..."` is honoured. **Multiple databases are covered** —
  `db/<name>_schema.rb` too, and a table two of them declare is declared by neither.
  **Calls that answered nothing now resolve, and none stopped**; more again that offered a list of
  same-named methods from unrelated gems now point at the column.

- **Your associations are methods too, and a collection chains.** `belongs_to :user` gives
  `story.user` a `User`; `has_one :profile` a `Profile?`; `has_many :comments` a relation, so
  `story.comments.first.title` keeps going. `class_name:` wins over the association's name, and
  `optional: true` is what makes a `belongs_to` nullable. A jump lands on the macro.
  `scope :recent, -> { ... }` becomes a class method returning the same relation, **and its body is
  never read**. Deliberately not answered: `polymorphic: true`, an association naming a class your
  application does not define, and a `has_many :through` whose intermediate is not on the same
  class. **Calls that answered nothing now resolve, none stopped, and more again moved from a
  list of same-named guesses to a single right answer.**

- **A Sorbet `sig` or a YARD `@return` types a method.** `sig { returns(String) }` and
  `# @return [Array<String>]` are both a human writing the type down — including `T.nilable`,
  `T::Boolean`, `T::Array[…]`, `@param` tags, and `[String, nil]`, which is how YARD spells
  optional. The card says which it read, because a comment is not a signature, and a `sig` wins over
  a tag that disagrees. Anything that is not exactly a class is declined rather than approximated:
  `T.any`, `T.proc`, duck types like `[#read]`, `Hash{Symbol=>String}`, and any union that is not
  `[Foo, nil]`.

- **A template's instance variables are typed by the controller that renders it.** `@story.` in
  `app/views/stories/show.html.erb` completes against whatever `StoriesController` assigns, and the
  card names the controller and the line. Bounded to the controller the path names: a view whose
  controller does not exist answers nothing rather than reaching for a similarly named class.
  Everything this rung types is in a template; nothing outside one changes.

- **A last resort that says it is guessing: `@user` is a `User`.** With nothing else to name a
  receiver, ya-lsp reads the name itself — `@user` → `User`, `person` → `Person` — and resolves it
  as a constant in the enclosing nesting. **It is the only answer ya-lsp gives that is allowed to be
  wrong**, so the hover card and the completion row both say what was guessed from, and it never
  displaces a type the code states, a signature's return type, or an assignment in the same class.
  Set **`[types] guess_from_names = false`** to keep only checkable answers. Most of the receivers
  it names would have answered the same way regardless, because a guessed class still has to declare
  the method being called.

- **Writing an assignment no longer makes an answer worse.** `story = Story.published.first` then
  `story.comments` used to fall back to every method in the project named `comments`, while a
  `story` with *no* assignment answered `Story`. A variable now keeps its spelling alongside
  whatever its assignment produced, and reaches the name-based rung only when the chain came back
  with nothing. **Calls get a better answer and not one gets a worse one** — some move from a list
  of same-named candidates to one class's own members, the rest to an answer derived from a
  declaration a reader can open.

**Indexing and requests**

- **ERB templates are indexed, and every request answers inside one.** `app/views/**/*.html.erb` is
  where a Rails application keeps **a large share of its method-call sites**, and none existed as far
  as ya-lsp was concerned. Templates are now indexed on the same walk as `.rb` files, so
  `references` is complete whether or not the view is open. Hover, go-to-definition, references,
  highlight, rename, the type hierarchy, `workspace/symbol` and semantic tokens all work at a cursor
  inside `<% %>`; completion works there and deliberately offers nothing in the markup around it.
  **`index.include` gains `**/*.erb` as a default**, which is a settings change: a project that has
  overridden `index.include` needs to add it.

- **No squiggles in a template.** What a template's Ruby cannot parse is not something you wrote
  wrongly — `<%= yield %>` is legal in the method a template compiles to — so diagnostics are
  suppressed there. Rare, but real: a couple of templates in a real Rails application hit it.

- **Folding in a template is left to the editor.** A folding provider that answers takes the
  editor's indentation guess out of play, and what this one sees is the Ruby: two folds for the
  `<% %>` blocks and nothing for the markup. Answering `null` gets the whole file folded instead.

- **Ruby that is not named `.rb` is indexed.** Your `Rakefile`, `Gemfile`, `config.ru`,
  `lib/tasks/*.rake` and your own `.gemspec` define constants and methods and were invisible.
  **`index.include` gains six defaults**: `**/*.rbs`, `**/*.rake`,
  `**/*.gemspec`, `**/Rakefile`, `**/Gemfile`, `**/config.ru`. A project that has overridden
  `index.include` needs to add the ones it wants. `Gemfile.lock` is deliberately not among them.

- **Every RBS signature your project already has on disk is read** — a gem's `sig/`, your own, and
  `.gem_rbs_collection/` if you have run `rbs collection install`. No configuration. Only a small
  fraction of a real bundle ships `sig/`, and it is **worth hundreds more methods with a usable
  return type.**

- **`textDocument/semanticTokens`.** The one thing a TextMate grammar cannot decide: `foo` alone on
  a line is a local variable or a method call. Keywords, strings, `@ivars` and `CONSTANT`s are left
  to the grammar, which already has them right. The full document, answered without waiting for the
  index, and no delta, which is a wire optimisation over an answer the server computes anyway.

- **Hover and go-to-definition use everything completion knew.** A local assigned a literal was
  exact in completion and matched on its name in a hover card. Both requests go through one
  resolution now, so a card and a jump cannot disagree about what `person.` is.

- **RuboCop, by running it rather than by pretending to.** ya-lsp still never executes Ruby, so no
  cop offence and no autocorrect comes from it — but RuboCop ships a language server of its own, and
  LSP allows a language to be served by more than one. Both READMEs now carry the configuration for
  VS Code, Neovim, Helix and Zed, including the one overlap worth knowing: ya-lsp's `parse-warning`
  and RuboCop's `Lint/UselessAssignment` report the same thing, so switch the rule off and keep the
  parse errors. On RuboCop 1.89 or newer you also get a lightbulb on each offence. In VS Code the
  extension offers RuboCop's own extension once, in a project with a `.rubocop.yml` or `rubocop` in
  its `Gemfile.lock`; `ya-lsp.rubocop.hint` turns the offer off.

### Changed

- **Suggestions, hover and go-to-definition no longer wait for the character you just typed to be
  indexed.** They answer against the last settled state and translate the cursor into it. Measured
  across six real Rails applications at typing speed, all three go from a pause you can feel to one
  you cannot, and the bigger the application the bigger the gain: on the largest, **no keystroke in
  a burst stalls any more, where every one used to.** Answers are unchanged at every position
  measured, and never nothing where there was something before.
  <br>**Cost:** diagnostics appear a fraction of a second later after you stop typing, because the
  debounce is longer.

- **Syntax colouring, folding and code actions no longer wait for the keystroke before them.**
  `semanticTokens`, `foldingRange`, `selectionRange` and `codeAction` answer from the buffer alone
  and never read the index; they were still queued behind indexing the character you had just
  typed. On the largest application tested, semantic tokens after each keystroke in a large model
  go from **a stall to imperceptible**. Hover, completion and go-to-definition still wait for the
  edit to be in the graph.

- **Your editor stops re-asking for a completion list it could have narrowed itself.**
  `isIncomplete` is now set only when the item cap actually dropped something. On a large
  application, typing a word costs **noticeably fewer round trips**, because the editor filters the
  list it already has instead of asking again. Some words save nothing — `self.` is unchanged,
  because a `.` always starts a fresh request. No suggestion changes and none is lost.

- **Typing in a file that declares nothing no longer regenerates the whole workspace.** ya-lsp keeps
  eight bytes per file describing what that file contributes and re-derives them for the one file
  you are typing in. On a large application, a keystroke in a file with no Rails macro **roughly
  halves what completion costs**, and the check that decides it is a rounding error where it used to
  dominate. Nothing is cached and no notification is trusted —
  every file read is still `stat`ed, so a `git checkout` that deletes `db/schema.rb` stops the
  columns answering at the very next request.

- **Typing in a model file no longer regenerates the whole workspace either.** Almost all of that
  cost was handing **one** unchanged generated document back to the indexer: declarations name the
  file and the macro, never a line number, so pressing return above a `has_many` produced
  byte-identical text whose positions had all moved. ya-lsp now updates the positions and leaves
  the index alone unless the declarations changed. On a large application, a keystroke in a model
  file costs **a fraction of what it used to**, nearly all of the saving being regeneration that no
  longer happens. A keystroke that really does change what a file declares still pays for the
  re-index.

- **Two classes of one name in two files resolve the same way on every run.** Where a project
  defines `class Story < ApplicationRecord` in `app/models/` and reopens `class Story` in a spec
  with a different superclass, which one ya-lsp believed depended on walk order. The file that
  sorts first now wins.

### Fixed

- **Two crashes are gone rather than survived.** rubydex moved forward this release: `extend self`
  inside a `Class.new` block, and a constant aliased to another and then reopened under the alias,
  no longer take a thread down. The containment around both stays where it was.
  <br>The visible gain is applications whose models live inside a module: a chain off a nested
  model class — `Shop::Product.where(...)`, then `.find`, `.select`, `.where` — used to fall back
  to name matching. Over five large applications, **more places answer precisely and none answers
  worse anywhere**; how much more depends on how namespaced the application is.

- **One file ya-lsp cannot read no longer stops it answering anything at all.** A crash in the
  indexer used to take the whole analysis thread with it — every request after it unanswered, with
  nothing on screen to say so. It reached real projects two ways: a gem in a real bundle writes the
  shape (`rice`, which `field_test` depends on), and typing those five lines into a file killed a
  working server.
  <br>Now a file that crashes the indexer costs **its own answers and nobody else's**. The file is
  named in a notification, a buffer typed into a bad state keeps the answers it had, and editing
  again retries. The same containment covers a **request**: a cursor position that crashes answers
  nothing and leaves every other request working. The naive version would have been worse than the
  bug — indexing in parallel, one crash used to take down whatever else that worker held, so a
  handful of bad files silently lost several times as many good ones.

- **A cursor in an ERB template is where the editor put it, whatever the markup beside it is made
  of.** An editor counts a column in UTF-16 units of the text it has; ya-lsp counted it against the
  blanked view, where markup is one space per byte. A `“`, a `café` or an emoji to the left of a tag
  moved the cursor left by one place per extra byte and moved every span answered back the same
  distance right. Hover, go-to-definition, completion, highlight, rename, semantic tokens and
  selection ranges were all affected, and the answer was **wrong** rather than missing.
  <br>Measured over six large applications: it happens in **dozens of templates**, and two of the
  six have none at all. Where it happens, ya-lsp now names the identifier under the cursor in most
  cases and answers about something else in **none**, against **none and many** before.

- **A serializer no longer claims to have the associations it serializes.**
  `active_model_serializers` and `jsonapi-serializer` spell `has_many`, `has_one` and `belongs_to`
  as ActiveRecord does and define **no method**. ya-lsp had been declaring all of them since
  v0.4.0 — every one a method that does not exist. What they cost was mostly the name-matched
  list, which is now **shorter at many positions and longer at none**. A class-side `belongs_to` a
  gem defines for something else (one admin framework uses it for nested-resource routing) is
  declined by the same rule.
  <br>Nothing a real model declares is affected, including a concern (which inherits nothing) and a
  model whose base class is in a gem (`class Tag < ActsAsTaggableOn::Tag`). `delegate` is untouched.

- **Typing `valid` in a model body offers `validates` — the Rails DSL completes now.** Hovering and
  jumping already worked; the completion list at the same cursor was **empty**, for every
  class-macro name Rails installs through `ActiveSupport::Concern` and for every one your own
  `app/models/concerns/` writes. Over five applications, **positions gained the word the file
  actually wrote and none lost one.**
  <br>What a class object cannot call is still not offered: a method a concern's macro adds to the
  *record* at run time belongs to instances, and a class writing its own `def self.validates` is
  offered its own and not a second copy.

- **`validates`, `scope`, `belongs_to` and `has_many` in a model body resolve to the method Rails
  really installs.** Hovering `scope` offered a long candidate list headed by a *routing* method,
  and `belongs_to` one headed by a migration method. The cause is one line of Rails:
  `ActiveSupport::Concern` writes `base.extend ClassMethods` and nobody's file contains it. ya-lsp
  now follows the convention — a module with a nested `module ClassMethods` puts those methods on
  the class side of everything that includes it, however the module installs it.
  <br>It is asked only after the ordinary lookup finds nothing. Where nothing resolves, the fallback
  list is narrower: a call on a **class** is no longer offered the *instance* methods of unrelated
  classes. `Link.find_each` went from a candidate list to `ActiveRecord::Batches#find_each`.

- **Every model answers `where`, `first` and `find`, and none answers with another model's
  relation.** ya-lsp gave a model the query interface only when some macro made it a collection, so
  `Category.order(:name)` on a model writing neither answered nothing. Worse, a `scope` in
  `ApplicationRecord` made **that** class the collection, so `Category.order(:name)` answered an
  `ApplicationRecord::Relation` — a class no row is an instance of. Both are gone: a class reaching
  `ActiveRecord::Base` by any number of superclasses gets its own relation and class side.
  <br>A class inheriting nothing still gets nothing. An **abstract** class answers none of the
  interface itself, because `ApplicationRecord.first` raises in Ruby, while a `scope` written in one
  still works on every class inheriting it. A model whose base is in a **gem** keeps whatever a
  macro gave it. An anonymous `Class.new(ApplicationRecord)` is not a model here.

- **A model inside a `module` gets its columns, and a throwaway model in a migration no longer takes
  them off the model that reads them.** ya-lsp claimed a table by pluralizing a **top-level** class
  name only, so `Shop::Order` and `Admin::Story` got no columns. It now computes the table the way
  Rails does, including the prefix a namespace declares — as `def self.table_name_prefix` or, far
  more commonly, as `isolate_namespace` in an engine's `engine.rb`. `table_name_suffix` is read too.
  <br>A table read by **more than one** class is the ordinary case and was treated as an ambiguity:
  a migration's throwaway `class Account < ApplicationRecord` reads the same `accounts` your model
  does, and both now get the columns. Two classes whose *different* names pluralize onto one table
  still get neither. A class writing `self.table_name = "posts"` no longer displaces the `Post` whose
  name implies it — in the corpus, test doubles and a migration's throwaway class were doing exactly
  that to the models those tables belong to. A class that is not a model still loses that contest.
  <br>A model **reopened in a second file** keeps its columns too: `class AuditLog` written again to
  nest a query object under it, or `class Reviewable` in six `lib/reviewable/` files, used to look
  like two claimants. `self.table_name = :accounts` counts now as well as the string form — the
  symbol spelling is the common one in real code and was invisible. A model nested inside another
  *model* is still declined, as is one whose namespace no file declares.

- **An association inside a `module` names the class Rails names, not the one at the top level.**
  `belongs_to :adjustment` in `Shop::LineItem` is `Shop::Adjustment` to Rails, which tries
  `Shop::LineItem::Adjustment`, then `Shop::Adjustment`, then the bare `Adjustment` **last**.
  ya-lsp asked only for the bare name, so a namespaced application got no member where nothing
  top-level matched — in an engine monorepo that is a large share of every association it writes —
  and jumped to the *wrong class* wherever both spellings exist. A written `class_name:` is resolved
  the same way; a leading `::` still means the top level. `composed_of` is unchanged, because
  ActiveRecord resolves that one absolutely. Over five applications: **answers improved in bulk and
  a handful changed for the worse**, every one of those a model defined inside another class whose
  columns the entry above made visible.

- **A module whose name Ruby never writes on its own line no longer loses its own methods.**
  `module Reports::Registry` is how an application spells a namespace `Reports` that Zeitwerk
  conjures and no file declares. A signature ya-lsp wrote for something inside that module spelled
  the name joined, which introduces `Reports::Registry` itself, and the module then silently lost
  `Reports::Registry.supported?` along with everything a subclass inherited. Such a signature is now
  written inside a body of its own. Where the namespace is one your code never writes `module` for,
  members are still skipped rather than guessed at. The same mistake in the other direction is fixed
  with it: a `def self.` carrying a `sig` or `@return` inside a `module` was written as `class`.

- **A Postgres array column no longer claims to be one of its elements.**
  `t.string "languages", array: true` — and `languages character varying[]` in a `structure.sql` —
  used to answer `String`, so `story.languages.upcase` looked correct and `story.languages.first`
  looked wrong. Both now answer `Array[String]`, and an unmapped element type gives
  `Array[untyped]`. Half the applications tested against write such a column.

- **A completion row taken from the name-based list is no longer presented as certain.**
  `completionItem/resolve` built its card with the "exact" flag set unconditionally, so the "matched
  on the method name alone" line never appeared on a completion item in two releases.

- **A hover card no longer claims a type came from Ruby's own signatures when it did not.** It now
  says the type was read off what those methods declare; which file a declaration came from is
  answered by hovering that declaration.

## [0.3.0] — 2026-09-04

### Added

- **`textDocument/rename`, with `prepareRename`.** Locals, parameters and constants. A constant is
  renamed across every file and only where it really is that constant: `Person` inside `module HR`,
  top-level `HR::Person` and the `class Person` line are one name; a `Person` in another namespace
  is not; and only the segment being renamed moves. Swept over a real Ruby project, renaming every
  name its `class` and `module` lines declare: **every replacement landed exactly on the old name**,
  and even the widest rename — thousands of edits across hundreds of files — is answered without a
  perceptible wait.
- **Nothing is edited until every place has been read back and confirmed to hold only the old
  name**, and one failure declines the whole rename. Not a formality: rubydex records the name span
  of `Error = Class.new(StandardError)` as the *entire assignment*. Renames were applied in bulk
  over a real project and re-parsed with no file gaining a diagnostic.
- **Four kinds of rename are declined out loud, each with a reason.** Methods (found by name alone);
  instance variables (a subclass writing the same `@name` writes the same variable); anything
  defined outside your own code; and a variable also written as a keyword or hash key —
  `def call(host:)`, and Ruby 3.1's `{ host:, port: }` and `connect(host:)`, where replacing the word
  changes the hash and **still parses**. That last rule declines a small fraction of real
  parameters.
- Prism decides whether a new name is usable: `Ünicode` is a constant where `é` is a variable;
  `nil`, `_1` and `__FILE__` cannot be assigned to; `first second` parses cleanly as something that
  is not a name.
- **`textDocument/prepareTypeHierarchy` and both follow-ups.** Supertypes are `Module#ancestors`
  minus the class itself, so `Comparable` is in `String`'s chain and a `prepend`ed module sits
  above. Subtypes are the mirror rather than one generation: expanding `Base` lists every class
  below it.
- **An unresolved ancestor is a row that says so**, spelled as written and placed where it is
  written — which is not always the class being expanded, since an unresolved superclass propagates
  down the chain.
- Subtypes cost nothing to find and something to draw, so the cap is on rows. With Ruby's own
  signatures indexed, an ordinary namespace answers instantly and only the very widest — everything
  below `Object` — costs anything at all. Reaching the cap is said out loud.
- **`textDocument/foldingRange` and `textDocument/selectionRange`.** Folding follows syntax rather
  than indentation: definitions, blocks, lambdas, literals, argument lists, heredoc bodies,
  `begin`/`rescue`/`else`/`ensure`, `case` and each branch, `if`/`elsif`/`else` as three regions,
  comment runs, `=begin` blocks and `#region` markers — the last three carrying their proper kind,
  so **Fold All Comments** does what it says. Swept over a real project's `lib/`, every file is
  answered in well under a millisecond.
- **Every `end`, `}` and `]` stays on screen**, and a one-line construct gets no chevron. Two node
  locations lie about this: a `def` with a bare `rescue` gets an implicit `begin` whose location
  runs to the *enclosing* `end`, and an `else` clause carries the `end` keyword inside its own.
- **Nothing is answered where nothing was found.** A client with a folding provider stops guessing
  from indentation, so an empty array takes the guess away and puts nothing in its place; `null`
  hands it back.
- Expanding the selection steps through a string's contents before its quotes, one argument before
  the list, a method name then its receiver, a body before the `def` and the `def` before the
  `class`. Every link contains the one before it even while the file is half-typed, and a chain is
  several links deep.
- Neither request waits for the index, so a keystroke reaches a folded outline in about half the
  time it used to.
- **`textDocument/documentHighlight`.** Every place in the file meaning the same thing, with
  assignments drawn differently from reads. Locals, parameters, block parameters and instance
  variables come from a scope walk over Prism; constants and methods from the graph, restricted to
  the file.
- The scope walk uses **Prism's own resolution**, so `x = 1; [1].each { |x| x }` separates without
  anything in ya-lsp knowing what shadowing is, and `rescue => e`, a `case/in` capture and a
  destructured `def f(a, (b, c))` all arrive as ordinary targets. An instance variable is scoped by
  **what `self` is**: `@v` in `def a` and `@v` in `def self.b` are two variables, and a `def c`
  inside `class << self` shares the second.
- `null` wherever ya-lsp does not know — a comment, a string, a keyword — which leaves the editor's
  word matching in play for exactly those positions. Swept over every identifier position of a
  large file, the request answers in well under a millisecond.
- **`textDocument/signatureHelp`.** The method's real parameters with the one being written
  underlined. `Person.new(` shows what `Person#initialize` takes, not `Class#new`. Overloads stay
  overloads: RBS declares three arms for `String#gsub` and the editor gets all of them with the
  fitting one marked. A keyword argument is found by **name**, so `create(role: :admin, name: `
  underlines `name:` however the two were ordered. A `*rest` absorbs every positional after it.
  Measured on a real project at typing speed, it keeps up with the keyboard.
- Signature help fires **only when the callee resolved exactly** — another class's parameter list
  under the cursor while you type into it would be a syntactically valid wrong answer.
- **Ruby that changes outside the editor is indexed as it changes.** A `git checkout`, `git pull`,
  rebase or `rails g model` re-indexes exactly what it touched, deletions included — the one place
  ya-lsp was *wrong* rather than absent, since go-to-definition landed in a file that no longer
  existed. Measured on a real checkout moving hundreds of Ruby files between releases, the whole
  re-index and the first correct answer after it both land inside a second; one file is
  instantaneous.
- Four rules decide whether a watched change is acted on: **an open buffer beats the disk**; **a gem
  is not your code** (a `bundle install` is left to the background index); **the walk decides what
  belongs** (`Workspace::indexes` shares its globs and walker with `Workspace::discover`, asserted
  against each other over every path in a fixture tree); and **`index.max_files` still applies**.
- **Every VS Code setting reviewed, regrouped and rewritten**, and five that only `ya-lsp.toml`
  could reach are now in the editor: `index.include`, `index.exclude`, `index.loadPaths`,
  `index.respectGitignore`, `gems.paths`. Twelve settings under one heading became five titled
  groups; every description leads with what you will see change, names its `ya-lsp.toml` twin, and
  keeps a measured cost where there is one. Every drop-down explains its choices.
- **The ten diagnostic rules are declared one by one**, so `ya-lsp.diagnostics.rules` completes
  their *names* and documents each on hover. It was an open object: the values completed, the names
  did not. This is how you find `parse-warning` in order to switch it off when RuboCop or Standard
  reports the same unused variables.

### Fixed

- **`ya-lsp.logLevel: "off"` no longer produces more output than any other value in the list.** It
  was treated as "the user said nothing", falling through to the server's `info` — louder than the
  `warn` or `error` above it in the same drop-down.
- **The same setting documented a default the server does not have** (`warn` against `info`). Every
  documented default is now checked against the server's own by a test that reads the manifest.
- **Settings can be set per folder in a multi-root workspace again.** Ten of the twelve settings
  were `window`-scoped, and VS Code does not read those from a folder's `.vscode/settings.json` —
  while the extension has always read them per folder. Everything the server reads is now
  `scope: "resource"`, asserted by a test over the manifest.
- **ya-lsp no longer dies silently when rubydex's resolver crashes.** rubydex 0.2.5 panics inside
  `Resolver::resolve` after a document is deleted. Uncaught it is the worst failure this server has:
  the thread dies, the editor keeps sending requests, and a server that answers nothing is
  indistinguishable from one that is thinking. The panic is caught, you are told in one sentence,
  and the index is rebuilt; a rebuild that crashes again stops rather than recurring.
- A panic anywhere else on the analysis thread now ends the server instead of leaving it running
  with nothing behind it. Every editor restarts a server that stops and none restarts one that goes
  quiet.
- **Typing `@` no longer offers `$@`.** A sigil is not a letter to fuzzy-match on. A prefix now only
  reaches names carrying the sigil it asked for, with one asymmetry: `@` still offers class
  variables, because the second `@` may be the next thing you type.
- **A `class << self` inside a nested class hovers as `class << Shelf::Book`** rather than
  `class << Book`. It read the attached class out of rubydex's `Shelf::Book::<Book>`, where it is
  written unqualified.

## [0.2.0] — 2026-08-25

### Added

- **The Ruby version is looked for in every directory from the project up to your home directory.**
  `.ruby-version` and `.tool-versions` are how rbenv, chruby, RVM, asdf and mise are told which Ruby
  a *tree* of projects uses, and every one of those tools walks the ancestors. Over the real
  workspaces on one machine, **half went from "no Ruby version" to a resolved one**, and every one
  of those gained Ruby's own library. The nearest directory wins whichever kind of file it holds, and the
  whole chain outranks `RUBY VERSION` in `Gemfile.lock`. The startup log names the file that
  answered, by path.
- **ya-lsp says so when it cannot tell which Ruby a project uses.** Refusing to guess is right —
  guessing once put macOS's vestigial Ruby 2.6 stdlib into the index and answered `"hello".u` with
  `unspace` — but refusing in silence cost the whole standard library with nothing said. There is
  now a message naming what was skipped and what to do, and a second for a known version that is
  not installed. Both name `gems.default_gems = false` as the way to silence them.
- **The first line ya-lsp logs names its version**, before the handshake, so a pasted log says which
  build wrote it. Spelled `ya-lsp <version> starting`, the same way `--version` spells it.

### Changed

- **Completion is ranked by how close a method's owner sits on the receiver's ancestor chain.** With
  nothing typed after the dot, nothing in the ranking key varied and the list came out alphabetical:
  `"hello".` opened on `DelegateClass`, `Digest`, `append_as_bytes`. It now opens on
  `append_as_bytes`, `ascii_only?`, `b`, with `Object`, `Kernel` and `BasicObject` at the bottom.
  The cap keeps the nearest names rather than the alphabetically first.
- **Completion on a receiver whose type cannot be known is ranked by how near the code is to the
  cursor** — the current file first, then outwards by directory. It was alphabetical, so `@foo.` in
  a Rails app opened on `account_type` regardless of where the cursor was.
- **`Foo.new` answers with `Foo#initialize`.** `Foo.new` really is `Class#new`, so the exact answer
  was `core/class.rbs` and `(*args, **kwargs, &block)` — on a Rails app, **every** `.new` call site
  landed there. A class writing its own `def self.new` keeps that answer; a class with no
  constructor keeps `Class#new`.

### Fixed

- **Methods declared inside an RBS `interface` are no longer treated as methods of the surrounding
  class.** They were indexed as though their contents belonged to whatever enclosed them, which for
  most was `Object`: `"hello".` offered `begin`, `exclude_end?`, `read` and `rewind`. Ruby's
  signatures declare dozens of such blocks, and on an ordinary receiver they crowded out nearly
  everything between the class's own methods and `Kernel`'s.
- **Completion no longer offers a method Ruby would refuse to call.** `initialize` was suggested on
  every receiver, because Ruby privatises it at the point of definition and neither RBS nor the index
  records that; same for `initialize_copy`, `initialize_clone`, `initialize_dup` and
  `respond_to_missing?`. A private method was also offered on any receiver of the same class as the
  caller, where Ruby raises `NoMethodError`.
- **The outline no longer disappears while a `def` is being typed.** VS Code threw
  `selectionRange must be contained in fullRange` and discarded the whole response. Prism recovers a
  bare `def` into a node whose span is the three keyword bytes and whose *name* span is the
  whitespace after them. Go-to-definition carried the same broken pair.
- **A half-typed `def` no longer adds a blank row to the outline.**
- **An anonymous rest, keyword-rest or block parameter is written once in a hover.** `def f(*, **, &)`
  came out as `**`, `****`, `&&`.
- **Hover no longer loses the markup Ruby's documentation is written in.** RDoc left HTML in —
  `<code>` spans plus `<em>`, `<strong>`, `<tt>`, `<b>`, `<i>` — and an editor renders a hover as
  markdown, which strips them. Worse, prose that merely *looks* like a tag (`<vowel>`, `<rhs>`,
  `<main>`) vanished with everything up to the next `>`. Ruby in an example is left exactly as
  written.
- **RDoc's cross-references no longer render as dead links.** Ruby's core signatures alone are full
  of them. The words stay, the link goes.
- **Every hover card puts what ya-lsp knows *about* an answer in the same place**: an italic line
  under the answer, one per fact.
- **The messages ya-lsp shows are written to one rule.** Settings named as `ya-lsp.toml` spells them;
  what happened and what it means joined one way; a remedy wherever ya-lsp knows one. Two messages
  were the same sentence written out twice in two files.
- **`ya-lsp.toml` reloads without a restart in every editor, not only VS Code.** Nothing in the
  server ever sent `client/registerCapability`, and the extension covered the gap with a watcher of
  its own. The server now asks the client to watch the file during the handshake, and says so in the
  log when the client cannot be asked. The extension's watcher is gone with it — two watchers meant
  the reload ran twice per save.
- **A change to a watched file that is not `ya-lsp.toml` no longer re-indexes the project.** File
  watchers are shared across every language server an editor runs, and ya-lsp answered every change
  by dropping the whole index.
- **Projects in a directory whose name contains `[`, `]`, `^` or `|` work.** Every answer naming a
  file was silently dropped: the two URL standards involved disagree about those four characters.
- **A multi-root workspace no longer starts a server on its first folder regardless of what is in
  it.** The eager start exists so a single-folder project has a warm server before the first
  keystroke, and now happens only when the workspace has exactly one folder.
- **A folder with no Ruby in it is no longer reported as a misconfiguration.** The warning now fires
  only when `index.include` or `index.exclude` was written by hand and matched nothing.

## [0.1.0] — 2026-08-20

First release.

ya-lsp resolves constants and does not infer types: constants are exact, and methods are matched by
name wherever the receiver cannot be named.

### Added

- **Diagnostics**, reported as the workspace is indexed and only for your own code. Most rules ship
  `off`, because they fire on correct Ruby.
- **Go to definition, hover, and document symbols**, answered from a resolved graph rather than from
  matching text.
- **Workspace symbol search and find-references.** The picker ranks your own code above match
  quality on purpose: a bundle declares orders of magnitude more names than the project does. Find-references looks
  only at your own code and says so when it reaches its cap.
- **Completion.** `Foo::`, `Foo.`, `self.`, a bare word and a call in a class body walk the real
  ancestor chain with real visibility. Literals are typed by reading the parse. Keyword arguments
  are offered only when the method resolved exactly. Nothing fires inside a comment or a string, but
  `#{}` does.
- **Gems, read from disk.** `Gemfile.lock` and the gem directories are parsed directly; ya-lsp never
  executes Ruby and never shells out to `ruby`, `bundle` or `gem`. Indexing runs in the background
  behind `$/progress`.
- **Ruby's own core and standard library**, from the `rbs` gem when the machine has one and from a
  copy embedded in the binary when it does not.
- **`ya-lsp.toml`**, read from the workspace root and overriding whatever the editor sends. Changes
  take effect without a restart.
- **VS Code extension**, bundling a server binary for `darwin-arm64`, `darwin-x64`, `linux-x64`,
  `linux-arm64`, `win32-x64` and `win32-arm64`. One server per workspace folder.
- **Standalone server archives** for any other LSP client, each carrying the binary, `LICENSE.txt`,
  `NOTICE.txt`, `README.md`, `THIRD-PARTY-NOTICES.txt` and this file. `ya-lsp --licenses` prints all
  of it from inside the binary.

### Known limitations

- **Methods on a receiver that cannot be named are matched by name.** For an unusual name that is
  the answer you wanted; for `call`, `name` or `id` it is a scoped text search.
- **Rails' `validates`, `has_many` and `scope` are not offered.** `ActiveSupport::Concern` installs
  them at runtime, so they exist in no source file.
- **`define_method` and friends** define names that only exist while the program runs.
- **A local variable's type comes from the textually last preceding assignment**, the one place
  ya-lsp can be confidently wrong rather than merely absent.
- **A Ruby file outside every workspace folder gets no server**, because there is no root to index.

[Unreleased]: https://github.com/ar2em1s/ya-lsp/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.4.0
[0.3.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.3.0
[0.2.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.2.0
[0.1.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.1.0
