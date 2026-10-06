# Changelog

The server, the VS Code extension and the Claude Code plugin ship as one version. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0] — 2026-10-06

### Added

- **A key read off a controller's `params` is what a request can carry.** `params[:id]`,
  `params.fetch(:page)` (or `fetch(:page, {})`) and `params.dig(:a, :b)` are `String | Integer |
  Float | bool | Array | ActionController::Parameters | ActionDispatch::Http::UploadedFile`, or
  `nil`, and `params.require(:post)` the same without `nil`, so `params[:q].strip` is a `String` and
  `params.require(:post).permit(…)` a `Parameters`. A key the application writes joins what it
  writes there (`params[:locale] = :en` adds `Symbol`), and so does a route's default
  (`defaults: { format: :json }`). A write it cannot read, a parser of its own, or `params` handed
  to a method that writes into what it is given leaves the key, or every key, unanswered, as before;
  a hash or an array written under a key leaves every read below a key so.
- **A key read off what `permit` or `expect` hands back is what its filter lets through.** With
  `def post_params = params.require(:post).permit(:title, tags: [], meta: {})`,
  `post_params[:title]` is `String | Integer | Float | bool | ActionDispatch::Http::UploadedFile`
  or `nil`, `post_params[:tags]` an `Array` or `nil`, `post_params[:meta]` a `Parameters` or
  `nil`; `fetch` drops the `nil` a missing key gives, and `require` every `nil`.
  `params.expect(post: [:title])` is the `Parameters` itself, read the same way, and
  `params.expect(tags: [])` an `Array`. Only off the controller's own `params`, by literal keys
  (`require`, `fetch`, `[]`, `dig`), with a filter written as literals or held in a constant
  assigned once a frozen list of them (`FIELDS = %i[title body].freeze`, never written again with
  `+=`), and only while nothing can have written into the value: through the call itself, a `def`
  that hands it back, and a local never written into, passed on or given a second name. A
  memoized instance variable, a local passed to a method, a key the filter does not name, a
  splatted filter (`permit(*FIELDS)`), and a write below the key in the request answer as before.
- **A route segment every route to the action requires is a `String`.** With `resources :posts`,
  `params[:id]` in `PostsController#show`, in a `before_action :set_post, only: :show`, and in a
  private helper only those actions call, is a `String`, so `Post.find(params[:id])` is a `Post`,
  not a `Post | Array`. A key a route's default gives is that default's class. Any route ya-lsp
  cannot read that may reach the action, a concern's method, or a method named by a symbol leaves
  it the request union.
- **A parameter no signature types is what every call passes it.** `def greet(name)` called with
  `"x"` in one place and `nil` in another has `name` as a `String?`, so its margin, its card and
  every read of its body say what it returns, where before only a call's own arguments could.
  Calls through an `alias` count too, and so do `send(:greet, x)`, `public_send` and `try` with a
  literal name, and a callback (`before_action :greet` calls it with nothing). Only the
  application's own methods: a name built for `send`, an override or module that may `super` into
  it, a Sidekiq worker's `perform` or a channel's action leave it untyped. A call whose argument
  ya-lsp cannot type, and a name handed to something that may call it with anything
  (`method(:greet)`, a gem's DSL), are left out, and the parameter's own card says so:
  `name: String | untyped`. A call on a receiver ya-lsp cannot type is not read, unless no other
  method has that name. Only the files that name one of the method's classes, or write the body of
  a class or module above one, are read: a call in a file that names none of them is left out,
  even where its receiver is one.
- **More calls count as a parameter's callers.** A method named `call` is what its written calls
  pass (`service.call(account)`), and its card always says more may come (`| untyped`): Ruby, Rack
  and `&callable` call it where nothing is written. A job's `perform` is what `perform_later` and
  `perform_now` pass on its class, also after `set(wait: …)`, and always says more may come, since
  a scheduler can enqueue it. A mailer's action is what `UserMailer.welcome(user)` and
  `UserMailer.with(…).welcome(user)` pass. A `scope` lambda's parameters are what each call of the
  scope passes, on the model or on a relation, and a class method called on a relation counts as
  called on its model.
- **An object holds what its own class was built with.** Where `Base#initialize(thing)` writes
  `@thing = thing` (or `self.thing = thing`), `thing` in `class PostView < Base` is what
  `PostView.new(…)` and its subclasses' `new` passed, not what every class built on `Base` is
  given. A class with its own `initialize` passes what its `super` passes (a bare `super`, its own
  parameters). A gem's `initialize` is read the same way: a serializer's `object` is what the
  application builds it with (`AccountSerializer.new(account)`). A construction ya-lsp cannot see
  (a gem building the object itself, a custom `self.new`) is left out, so the type may be narrower
  than what runs. **A class the application hands to other code has no answer**
  (`mount_uploader :cover, CoverUploader`, `serializer: AccountSerializer`): that code builds it
  with what nothing shows, so a `CoverUploader`'s `mounted_as` is no longer `nil`. A class with no
  `initialize` of its own that the application never builds (a policy only Pundit builds) holds
  what every class running that `initialize` is built with.
- **A concern or module calling a method its includers have is typed.** In a module's instance
  method, `params`, `errors.add(…)` or an association the including classes declare is each
  including class's answer, joined (`String | Integer` where two includers differ), the jump goes
  to each class's method, and completion offers their members. Subclasses count, and so does a class object a hook extends
  (`def self.included(base) = base.extend(ClassMethods)` makes `ClassMethods`' methods run on each
  includer's class). A name the module has itself is still its own. An object ya-lsp does not see
  the module mixed into (`obj.extend(M)` at run time) is left out, so the union may be narrower
  than Ruby's; a `def` inside `class_methods do` and a gem's module are not read this way.
- **`UserMailer.with(user: u).welcome` is an `ActionMailer::MessageDelivery`**, and
  `Job.set(wait: 1).perform_later(x)` the job or `false`. In a mailer, `params[:user]` is what each
  `with(user: …)` passed, or `nil`; a call that hands `with` a hash it does not write out leaves
  every key unanswered.
- **A `begin … end` that rescues nothing is its last statement**, so a memo written
  `@range ||= begin … end` has the type of what the block ends with. A method whose `rescue` ends
  in `retry` is typed by the body it runs again, and `$1` or `$&` is a `String?`.
- **A FactoryBot callback's block is handed what the factory builds.** In `factory :user`,
  `after(:create) { |user| … }` has `user` as a `User`, or a union where a child factory builds
  another class, and `create(:post)` inside the block is a `Post`. `before(:build)` is handed
  `nil`. The evaluator stays untyped, and so does a callback ya-lsp cannot read whole: `after(:all)`,
  a name it builds, a factory only Ruby names below this one, or a block with more than two
  parameters.
- **A block parameter a signature types as a union (`Integer | Float`) is that union**, as a
  union return already was.
- **An API controller's `params` is the request's, as a `Base` controller's is.**
  `ActionController::API` includes its modules in a loop ya-lsp did not read, so `params` under it
  was `Metal`'s and answered nothing; every rule above now applies, and `render`, `redirect_to` and
  the callbacks are found there too.
- **`SecureRandom.hex`, `uuid`, `alphanumeric` and the rest of `Random::Formatter` are `String`s.**
  Ruby's signatures write them on a stand-in module that `Random::Formatter` includes, and the `def`
  in Ruby's own library was read instead.
- **ActiveSupport's `String#blank?` is a `bool`, `parameterize` a `String` and `Array.wrap` an
  `Array`.** A record's `changes` is a `HashWithIndifferentAccess`, and `previous_changes` and
  `saved_changes` are that or, before any save, an empty `Hash`.
- **A local, a block parameter and a method parameter have a hover card**: `name: String`, at its
  assignment, a read, or the `def` line.
- **A method's card writes each parameter's type before it**:
  `Greeter#greet(String | untyped name, Integer times = 2) -> String`.

### Changed

- **Completion no longer waits for inlay hints while you type.** VS Code asks for hints again after
  nearly every keystroke, and each ask first brought the index up to date with the edit, so the
  next keystroke's completion queued behind it. A hint request now waits until typing pauses for
  half a second and is answered right after the update that pause brings anyway; the hints already
  on screen stay there meanwhile. On four mid-size reference apps the median completion while
  typing went from 30–120 ms back to 4–6 ms, what it is with hints off. New hints show about half a
  second after typing stops.
- **Hints after an edit come back sooner on a large app.** Each edit re-checked which routes prove
  a request's keys by parsing again every file a routes file's helper methods are written in, and
  every project file's methods; both are now read again only where a file changed. And the index
  of every method call is built when the project finishes loading, not by the first hint or hover
  that reads a method's callers.
- **A long session on a large app holds about 160 MB less**, for 5–10% more time answering:
  1.17 GB after 29,000 requests on the largest reference app, was 1.33. What files are read into
  between requests is kept for at most 8 MB of their text, was 64, and closed files' text for at
  most 16 MB, was 64. Hints over the busiest files of the five smaller reference apps take the same
  time as before.

### Fixed

- **Editing only a routes file's helper method, or a module a controller includes, updates what a
  request's keys are typed as.** The routes are read again at the next check after the edit, not
  after some later edit to another file.
- **A `scope` written twice in one class is the second one**, as in Rails: its return was read
  from the first lambda, and a jump went to the first line.
- **A factory whose `parent:` is not written as a literal no longer builds its own name's class.**
  `factory :admin, parent: base` made `create(:admin)` an `Admin`, where FactoryBot builds the
  parent's class; ya-lsp now says nothing there.
- **`x&.to_s` on a value that is only `nil` is `nil`**, not `String`: `&.` skips the call there.
- **A value that can be `true`, `false` or something else answers its calls.** `String | bool`
  answered nothing for `to_s`, `to_i` or `blank?`; each call now runs on every class of the value,
  so `to_i` is an `Integer` (only `String` has it) and `blank?` a `bool`.
- **A method written inside a block in some gem no longer hides `Kernel`'s.** One `def freeze`
  inside a `Struct.new do … end` anywhere in the bundle made `Foo.new.freeze` a guess, and a value
  that could be a `Foo` or a `String` answer `String` alone. The lookup now goes on to `Kernel`, as
  Ruby's does.
- **`f(1, **opts)` no longer says the next parameter is a `Hash`.** An empty `opts` passes nothing,
  so that parameter may hold its default; it is now left untyped at that call. The parameters
  before a `*rest` or `...` are now typed by what the call passes, as Ruby places them by position.
- **A gem added or installed while the server runs is indexed without a restart.** The bundle was
  found once, at startup, so after `bundle add` or `bundle install` the new gems had no hover,
  jumps, completion or inlay hints until a restart. A change to `Gemfile.lock` (or `gems.locked`,
  or the lockfile beside `BUNDLE_GEMFILE`) now re-indexes the project when it changed what the
  bundle is: a gem locked, removed, installed or moved, another Ruby, or RSpec, i18n or Rails
  support turned on or off by `auto`. A lockfile written again with the same bundle, as every
  `bundle install` does, re-indexes nothing.

## [1.0.0] — 2026-09-30

### Added

- **`ya-lsp coverage [DIR]` says how much of a project ya-lsp can type.** It indexes the project
  and its gems, samples 2,000 of the calls its own code makes outside test and migration folders
  (as `[trees]` says), and prints the share it types for certain with the sample's 95% error:
  `Type coverage: 47.3% ± 2.1% (2,000 of 37,909 calls sampled)`. Any Ruby project, Rails or not.
- **`ActiveSupport::CurrentAttributes` attributes are typed.** `attribute :account` declares
  `Current.account` and its writer (and the instance's), and the reader is what the application
  assigns (`Current.account = …`, `Current.set(account: …)`), with `nil`, or beside a literal
  `default:`. A value nothing types, `set(**options)`, a `send` of the writer, or a hand-written
  writer leaves it unanswered.
- **An empty array or hash a method fills holds what it was given.** `r = []`, then
  `items.each { |i| r << Foo.new(i) }`, then `r` is an `Array[Foo]` (`h[k] = v` gives a
  `Hash[K, V]`). Passing the local anywhere, storing it, or any other method on it leaves it as
  before, and so does a value only a name guesses.
- **An instance variable just written is that value.** `@label = "x"` followed by `@label` (or
  `@label.upcase`, or `touch(@label)`) in the same method is a `String`, whatever other methods
  write, as long as nothing that could run code sits between them.
- **A `before_action` that always writes a variable keeps `nil` out of the actions it runs
  before.** With `before_action :set_post, only: %i[show edit]` and `def set_post; @post =
  Post.find(params[:id]); end`, `@post` in `show` is a `Post`, not a `Post?`. An `if:`, a skip on
  the class, an ancestor or a subclass, an early `return` in the callback, or an action the
  controller also calls by name keeps the `?`.
- **An `attr_writer`'s variable is what its calls pass.** `box.label = 1` and `self.label ||= "y"`
  type `@label` in `Box`, where the receiver can be a `Box`. A receiver or value nothing types, a
  hand-written `label=`, and a `send`/`public_send` that can call `label=` on the object leave it
  unanswered, as before. So does `new(attributes)` on an Active Record or `ActiveModel` class.
- **`pluck`, `ids` and `group` know their columns.** `Comment.where(…).pluck(:depth)` is an
  `Array[Integer]` (a nullable column's element is left open), `Comment.ids` an array of the
  primary key's type unless a `self.primary_key =` can move it, and `Comment.group(:x).count` a
  `Hash` by group, through any chain after `group`. `Comment.pick(:depth)` and `Comment.pluck` on
  the class answer as the relation does.
- **A jbuilder view is read as a template.** `@story` in `show.json.jbuilder` is what the
  controller assigns, the view's helpers answer bare calls, `json` is a `JbuilderTemplate`, and a
  jbuilder partial's locals come from `json.partial!`, `json.array!` and `json.key …, partial:`
  calls. A JSON render finds jbuilder partials only.
- **A partial's locals are typed from the calls that render it.** In `_story.html.erb`, `story`
  is what every `render "stories/story", story: …`, `locals: { … }`, `object:`, `collection:`
  (with `as:`, `_counter` and `_iteration`) and `render @stories` passes, joined; hover shows it
  as a variable and go-to-definition lists each value passed. A strict-locals comment decides
  which names are locals. A call whose locals cannot be read, a component's `render`, or a helper
  of the same name leaves the name unanswered rather than guessed.
- **A check narrows the local it reads.** Below `return unless user`, `raise if user.nil?` or
  `user = User.create unless user`, `user` is a `User`, not a `User?`; inside `if user`, `unless
  user.nil?`, `x.is_a?(Hash)` or `case x when Hash`, and on the right of `user && …`, it is what
  the check says. An untyped value checked with `is_a?` becomes that class. A write after the
  check, one inside a block or lambda that may run later, and instance variables are left as they
  were.
- **`self` in a concern's `included do` is each class that includes it.** A macro or a call there
  resolves on every including model, a callback block inside it runs against their records, and a
  hover counts each model's own method under *N definitions*.
  Only where the project's own `include`s name the classes; a concern nobody includes stays
  unanswered. A scope the concern writes, called on one model, is that model's relation. The margin
  skips a union of more than four including classes.
- **A method whose own code writes `raise` or `fail` is marked `!`** in the margin and on its card:
  `-> String!`, `-> String?!`. Every `raise` in the method counts, in blocks and `rescue` clauses
  too, as a warning. A variable holding what the method returned is not marked.
- **A call whose arguments pick no signature is every signature it could reach**:
  `Story.find(params[:id])` is `Story | Array[Story]`, since a request can send a list. A call on
  such a union runs on each class that has the method, so `.title` on it is `Story#title`'s answer,
  and go-to-definition goes there.
- **Completion on a union lists every class's methods**, each row naming its class: after
  `@invite = Invite.find(params[:id])`, `@invite.` offers `Invite`'s methods beside `Array`'s. A
  name two classes define is one row naming both.
- **Completion at a bare word offers what the block runs against**, as hover already answers it:
  `let` and `subject` in a `describe` block, `eq` and FactoryBot's `create` in an example, a
  model's class methods in a concern's `included do`, and a block a signature rebinds.
- **A call on a union answers each class's own method**: `URI.parse(link).host` is
  `URI::Generic#host` on hover and go-to, and `to_i` on a `Float | Integer` is its two definitions,
  not a guess among every method of that name.
- **A conditional used as a value is whichever branch ran**: `x = ok ? "a" : 1` is
  `String | Integer`, and so for `if`, `unless`, `case`, `case … in`, `begin … rescue … end` and
  `a rescue b`. A missing branch is `nil`; one that raises or returns adds nothing.
- **`rescue Foo => e` makes `e` a `Foo`**, and a bare `rescue => e` a `StandardError`.
- **A block runs against what its method's signature says** (RBS `[self: T]`), for types and for
  go-to-definition. In Rails: a model's, controller's, job's and mailer's callbacks and their `if:`
  and `unless:` lambdas, `validate`, `validates`, `rescue_from`, `after_discard`, `initializer`
  and `content_security_policy` run against the object; `scope` lambdas against the relation;
  `Rails.application.configure` against the application; `routes.draw` against the route mapper.
- **`config` is typed**: `Rails::Application::Configuration`, a framework's namespace
  (`config.action_mailer`) is `ActiveSupport::OrderedOptions`, and a few settings Rails fills
  (`hosts`, `paths`, `middleware`) are typed.
- **A migration's own calls go where Rails sends them.** `add_column`, `create_table`, `execute`
  and the rest reach the connection through `method_missing`, so go-to-definition, hover and
  completion now answer each with the `def` in activerecord's `SchemaStatements`,
  `DatabaseStatements` or `Quoting`, read from the installed version. The `t` of
  `create_table … do |t|` is a `TableDefinition` (`change_table`'s a `Table`), and `t.string`,
  `t.integer` and the other column methods `define_column_methods` writes are found.
- **`to_s` is a `String` whatever it is called on**: `params[:id].to_s.strip` is typed, and
  `x&.to_s` is a `String?`. Every object answers `to_s`, and Ruby raises where it converts with one
  that returns another class. A method's own body or signature still answers first.
- **What Ruby's syntax fixes is typed**: a `*rest` parameter is an `Array`, `**rest` a `Hash` and
  `&block` a `Proc?`, in a method, a block or a lambda; `a, *rest = x` makes `rest` an `Array`;
  `defined?(x)` is a `String?`; and `obj&.x = v` is `v`'s type or `nil`.
- **Blocks, procs and lambdas are typed at each call.** A method returning `yield` (or its
  `&block.call`) returns the block's value; a block's `next` values count, and a `break` value is
  one more value of the call; `map(&:to_s)` knows its element. A block parameter takes what the
  method's own `yield`s hand it, `nil` where they hand fewer. A local, instance variable or constant
  holding only proc or lambda literals is read at each `.call`, `.()` or `[]`, and where passed as
  `&fmt`; a class's own `proc` or `lambda` method is not taken for Ruby's.
- **`block_given?` is read for each call.** A method that writes
  `return to_enum(:each) unless block_given?`, `if block_given? … else … end` or `unless block`
  answers a call with a block from the block's side, and a call without one from the other, so
  `CSV.open(path) { … }` is the block's value and no longer a union with `CSV`.
- **A signature that returns one of several classes is typed as that union**:
  `relation.count` is `Integer | Hash`, `Float#round(1)` is `Integer | Float`. A union whose
  classes all inherit one of them is drawn as that one: `URI.parse` is `URI::Generic`.
- **More of Rails is typed:** `Rails.env.test?` and its siblings; a `TimeWithZone`'s `year`,
  `to_date`, `strftime`, `beginning_of_day` and the rest; `Time.zone.now`, `parse` and `today`;
  `Time.current`; a model's `arel_table`, `model_name`, `sanitize_sql_array` and `transaction`
  (the block's value); a relation's `to_sql`, `arel` and `load`; `find_each` and its siblings
  without a block; `errors.empty?`; and an attachment's `blob` and `filename`.
- **A mounted engine's route helpers are typed**: `spree.admin_orders_path` and
  `main_app.root_path` are `String`. The proxy's name is read from the engine's `engine_name` or
  `isolate_namespace`, and `spree` jumps there.
- **A module's `thread_mattr_accessor` or `mattr_accessor` is what the application writes to
  it**: with `Current.account = @account` in a controller, `Current.account` is `Account?`. Every
  write the application makes counts, templates included and specs not, and one ya-lsp cannot
  type leaves the accessor untyped. A plain `mattr_accessor` whose `@@` variable something writes
  directly stays untyped.
- **A concern's class methods answer from their `def`**: `Account.find_local(name)` is what the
  `def` in `class_methods do` returns, read with `self` as the class, so its calls reach the
  class's own class methods. The same holds for Rails' own: `ActiveStorage::Blob.find_signed` is a
  `Blob?`, a mailer's `with` an `ActionMailer::Parameterized::Mailer`.
- **A `delegate` answers what its target's method does**, where the target is a method (a private
  one too) or a constant: with `delegate :total, to: :order`, `updater.total` is what
  `order.total` is. `allow_nil: true` adds `nil`.
- **More Rails returns:** `perform_later` is the job or `false`; `insert_all`, `upsert_all` and
  their siblings are an `ActiveRecord::Result`; a model's `table_name` is a `String?`,
  `column_names` an `Array[String]`, `table_exists?` a `bool`; and an `id: :serial` or
  `:bigserial` primary key is an `Integer`.
- **RSpec is read**, in spec files the editor holds: each `describe` and `context` is a class, so
  inside an `it`, a hook or a `let` block `self` is the example and its members complete and hover.
  `let(:story) { Story.new }` and `subject` are what their blocks return, a nested group inherits
  and overrides them, and `described_class` and the implicit `subject` come from what a group
  describes. `config.include` and `config.extend` reach the groups their `type:` or tag names
  (rspec-rails' directory types included), and a shared context's `let`s reach the groups that
  include it. `[rspec] enabled` turns it off; `auto` asks the lockfile for `rspec-core`.
  - `expect(x)`, `expect { }`, `allow(x)`, `receive(:name)` and its `and_return`, `with`, `once`
    and the rest are what rspec-expectations and rspec-mocks make, though both gems define them
    at run time.
  - Go-to-definition on `expect`, `allow`, `receive`, `and_return`, `with`, `let`, `before` and
    `shared_examples` goes to the `def` the gem writes it in. `describe`, `it` and
    `allow(x).to`, which the gems make with `define_method`, go nowhere, instead of to any
    method of that name in the bundle.
  - test-prof's `let_it_be` is a `let`, unless the project registers a modifier of its own.
  - A `def` written in a group is that group's method: its margin and card read its own body, not
    every spec's `def` of the same name.
  - A `context` written inside a shared group is a group of its own, whose examples see its `let`s
    and the shared group's.
- **A FactoryBot factory builds its class**: `create(:user)`, `build`, `build_stubbed` and their
  `_list` and `_pair` forms hover and complete as the `User` the factory says, read as FactoryBot
  reads it: `class:`, a nested factory's or `parent:`'s class, `aliases:`. A factory whose
  `initialize_with` returns something other than the class (in a trait, a parent, or the global
  one) stays untyped, and so does a strategy `FactoryBot.register_strategy` replaces.
  Go-to-definition on `create(:user)` opens the `factory :user` line. A gem's factories are read
  too, where the project defines none of that name. `[types] factories` turns it off.
- **A connection is its adapter**: `ActiveRecord::Base.connection`, `lease_connection` and the
  block of `with_connection` are the adapter `config/database.yml` names
  (`ActiveRecord::ConnectionAdapters::PostgreSQLAdapter`), or the class every adapter the bundle
  can load shares. A model's own `connects_to database: { writing: :animals }` is the `animals`
  database's adapter, and its `connection_pool.with_connection` hands the block the same class. An
  adapter a gem registers (`postgis`) is read from the gem; an engine's connection is any adapter.
  The transaction methods Rails makes with `delegate` (`open_transactions`, `current_transaction`
  and eight more) are found on it.
- **`Rails.logger` is typed**: the `ActiveSupport::BroadcastLogger` Rails wraps every logger in,
  so `Rails.logger.info` goes to its method. Where the application assigns `Rails.logger`
  itself, what it assigns joins: `Rails.logger = Logger.new(STDOUT)` makes it
  `ActiveSupport::BroadcastLogger | Logger`.
- **A call returning a tuple is an `Array`**: `Array(x)` is an `Array` whatever `x` is, and so
  is `IO.pipe` or `divmod` taken whole.
- **A method's own type variable is the argument passed for it**, where that argument is typed:
  `ENV.fetch("PORT", 3000)` is `String | Integer`, `ENV.fetch("HOST", "x")` a `String`, and
  `each_with_object({})` a `Hash`.
- **A controller's `helpers` reaches the application's helpers**: `helpers.cover_url(image)` in a
  controller and `ApplicationController.helpers.cover_url(image)` anywhere are what the helper
  returns, and `ActionController::Base.helpers.strip_tags` is Rails' own. A helper made with
  `helper_method` or added by a gem is not reached.
- **`pick(:column)` on a relation is that column's type or `nil`**:
  `Comment.where(id: id).pick(:depth)` is `Integer?`. A column an `enum`, `attribute`,
  `serialize` or `store` re-types, a string or several columns, and `pick` on the model itself
  stay untyped.
- **A column's `x?`, `x_changed?`, `saved_change_to_x?` and `x_was` are declared**, so
  `user.admin?` on a boolean column is `bool`, unless the model writes its own `def admin?`.
- **`where.not`, `where.missing` and `where.associated` are typed.** A bare `where` is
  ActiveRecord's `WhereChain`, holding the relation it was made from, so `Story.where.not(…)` is
  a `Story::Relation` and the chain goes on: `Story.where.not(…).first` is a `Story?`.
- **A model's class method answers on a relation**, as Rails forwards it: `Story.where(…).digest`
  and `digest` inside a `scope` lambda.
- **What a relation hands its records is typed**: `Story.where(…).reverse`, `sample`, `index`,
  `join`, `[]` and the rest Rails forwards to the loaded array.
- **`include Singleton` gives the class `instance`**: `TagManager.instance.url_for(x)` is typed
  and jumps. Ruby adds `instance` when the module is included, which no file writes down.
- **A setting the application assigns is what it was assigned**: after
  `config.dispatcher = Dispatcher.new`, `Rails.configuration.dispatcher` is a `Dispatcher`, and
  go-to-definition opens each assignment. Every engine's and railtie's `config` shares the store,
  so their writes join; one on a receiver ya-lsp cannot type leaves it untyped.
- **A mailbox's `mail` is a `Mail::Message`** and its `inbound_email` an
  `ActionMailbox::InboundEmail`: Rails makes both with `delegate` and `attr_reader`.
- **`send(:title)` is the call `title`**: `send`, `__send__`, `public_send` and ActiveSupport's
  `try`/`try!` with a symbol answer what the method they name answers (`try` on a value that may be
  `nil` may be `nil` too). `public_send` and `try` reach no private method.
- **The `:name` in `send(:name)`, `method(:name)`, `try(:name)`, `respond_to?(:name)` or
  `instance_method(:name)` goes to that method**, found on the object the call is sent to, in a
  method body as well as a class body. Hover and highlight answer there too.
- **A method `define_method(:name) { … }` makes exists**: calls of it are typed by what its block
  returns, go-to-definition lands on the `:name`, and completion offers it. The same for
  `define_singleton_method`, and for `define_method` in `class << self`. A `private` section or
  `private define_method(…)` makes it private. Names built in a loop are not read.
- **An `alias` or `alias_method` of a method with no signature answers as that method**, including
  one the class inherits.
- **`method(:shout)` is a `Method[Widget#shout]`**: calling it (`.call`, `.()`, `[]`), straight or
  from a local, answers what `shout` does, and so does passing it as a block
  (`list.map(&method(:shout))`).
- **References to a method list where its name is handed as a symbol**: `send(:shout)`,
  `try(:shout)`, `method(:shout)`, `respond_to?(:shout)`. Highlight lights them too, and asking for
  references on the symbol itself lists the method's uses.
- **More of Rails is typed, from its own source** (checked in 7.2, 8.0 and 8.1):
  - a record's `save` (`bool?`), `save!`, `update!` (`true?`), `update`, `update_column(s)`,
    `new_record?`, `persisted?`, `destroyed?`, `destroy!` and `attributes`;
  - `errors.full_messages` (`Array[String]`); a controller's `redirect_to` (the status,
    `Integer`), `params.expect(user: [...])` (`ActionController::Parameters | Array`, Rails 8),
    `flash.now`, `request.env`, `request.host` and `request.format`;
  - `Rails.application.credentials`; `2.days.ago` and its siblings (`TimeWithZone | Time`, or the
    class of the time handed to them); `duration.to_i`;
  - `Rails.cache.fetch(key) { … }` is the block's value (not with `raw:`), and `write`, `delete`,
    `exist?` and `fetch_multi` are typed;
  - a mailer's `mail`, a delivery's `deliver_now` and `deliver_later`; `strip_tags`; `Arel.sql`;
    a connection's `exec_query` and `select_all`; `Rails.application.configure { … }` and
    `RSpec.configure { … }` are their block's value;
  - `relation.minimum(:col)` and `maximum` are the column's type (or a `Hash` after `group`), and
    `unscoped { … }` is the block's value; `each_with_object` is its memo; `async_count` and its
    siblings are an `ActiveRecord::Promise`;
  - `has_secure_password`'s `authenticate` is the record or `false`; a `has_many` whose class
    cannot be read is an `ActiveRecord::Associations::CollectionProxy`;
  - more column types: `time`, `timestamp` and `timestamptz` (`TimeWithZone | Time`), `interval`
    (`ActiveSupport::Duration`), a PostgreSQL `enum`, the range types, `point` and the geometric
    types, and a `virtual` column's `type:`. A `datetime` in a project that moves Rails' time-zone
    default is `TimeWithZone | Time` instead of untyped.
- **`-> void` and `-> bot` on the card** of a method whose signature says it hands back nothing,
  and **`-> bot` in the margin** of a `def` whose every path raises.
- **Translation keys.** Inside `t("…")`, `translate`, `t!` and `I18n.t`, completion offers the next
  segment of the key, go-to-definition opens the YAML line that writes it, and hover shows its text.
  A call is typed by what the key holds: `String`, `ActiveSupport::SafeBuffer` for an `_html` key in
  a view or controller, `Hash` for a subtree (a `String` for a plural given `count:`), `Array` for a
  list. Keys come from Rails' own locale files, every gem's `config/locales` and the project's, in
  i18n's load order, read as Ruby's YAML reads them (`yes` is not a string). Only the main locale is
  read: `[i18n] locale` (`en` by default); `[i18n] paths` replaces where the project's own files
  are. `I18n.locale` is a `Symbol`, `I18n.l` a `String`, `I18n.with_locale { … }` the block's value,
  and `model_name.human` a `String`.

### Changed

- **A hover card says what the answer is, not how ya-lsp found it.** ya-lsp adds two lines, above
  the documentation where a long comment cannot hide them: *Guessed from name alone.* on any
  answer that rests on a name, and *Defined in N places.* on a method or a variable written in more
  than one place. Gone: the lines naming the signatures, assignments, renderers and bodies a type
  was followed through, a generated member's provenance, and the note that a type is this call's.
  A derived answer now reads like one the code states; the audit scores the same two tiers.
- **A method's card shows its return type whenever ya-lsp has one**: declared, read from its body,
  or this call's. A column's card is `Story#title -> String?`.
- **An instance variable's card is the variable and its type**: `Story#@title: Title`, not the card
  of `class Title`. A constant's card shows what it holds: `Keystore::MAX_KEY_LENGTH: Integer`.
- **Parameters are what the `def` wrote**: real names wherever a Ruby `def` exists, a generated
  method's included (`ActiveRecord::Base.sanitize_sql_for_order(condition)`, not `(arg0)`), and
  defaults as written where they fit on the line (`limit = 10`, not `limit = ...`). A generated
  writer's parameter is `value`. RSpec's words are named as the gems name them (`expect(value)`,
  `let(name, &block)`), and FactoryBot's strategies as they are called:
  `create_list(factory, amount, *args, **kwargs, &block)`.
- **An alias's card has the parameters of the method it renames**: `alias send __send__` is
  `send(name, *args, **kwargs, &block)`, where it read as taking nothing, and i18n's `t` is
  `t(key = nil, **options)`.
- **Class names ya-lsp invents are shown as Rails' own**: `ActiveRecord::Relation#where`, not
  `ActiveRecordRelation#where`; a route helper as `story_path`; a controller's `helpers` as
  `ActionView::Base`. A project's own class of that name keeps its name.
- **A list card counts and no longer lists**: *N possible definitions* with the guess line, or *N
  definitions* for each class a concern runs on. Go-to-definition lists them with their files.
- **A translation key's card is the YAML the main locale holds**: a plural or a subtree nested and
  cut after ten lines, where it said *a subtree of 4 keys*, and no *Written at* line.
- **Inlay hints have no tooltip**, and the server no longer offers `inlayHint/resolve`.

### Fixed

- **An index call on a constant that holds an object is typed**: `ENV["HOME"]` is `String?`, as
  `ENV.fetch("HOME")` was already typed.
- **A constant alias is the module it names**: `YAML.` lists `Psych`'s methods, and a call on it
  is typed like one on `Psych`.
- **Inside a module's method, completion offers `Object`'s methods too**, after the module's own:
  `self.class`, `format`, `raise`, as hover resolves them.
- **Hovering `new` no longer shows what `initialize` returns.** `Sponge.new` was carded
  `Sponge#initialize -> Integer` (its last statement's type), though `new` hands back the object.
- **`initialize` stops counting writes at an early `return`.** `return if skip` above `@seed = 1`
  used to drop `nil` from `@seed` everywhere.

- **Ruby's own class docs no longer open with `<!-- rdoc-file=string.rb -->`** on the hover card.
- **A module only the test suite mixes in is not on the application's path.** One application's spec
  helper reopens `MessageBus` and `extend`s a wrapper around `publish`, so a plugin's
  `MessageBus.publish` jumped into `spec/support/` with a *Resolved* card. It now goes to
  `MessageBus::Implementation#publish`, the method the application runs. A call written inside the
  suite still reaches the wrapper.
- **A layout knows its controllers.** `@title` in `layouts/application.html.erb` has a type, a
  card and a jump to where it is set, and `current_user` there reaches the controllers'
  `helper_method`. Which controllers and mailers render in a layout is Rails' own lookup: the
  nearest `layout "admin"`, else a layout named after the controller, else the parent's, so an
  admin controller's variables never reach the application layout. A `layout :method` or a lambda
  counts for every layout. Layouts written in HAML or Slim are still not read.
- **A mailer's `default template_path:` is where its views are read from.** A
  `->(mailer) { "mailers/#{mailer.class.name.underscore}" }` puts `NotifyMailer`'s views under
  `app/views/mailers/notify_mailer/`, whose `@ivar`s had no type and no jump. A written directory
  and a string around the mailer's own name are read; any other value only stops the default
  directory from counting.
- **A module included or prepended from outside a class is one of its ancestors.**
  `Paperclip::Attachment.prepend(Extensions)`, or a plugin's `Post.include(Extension)` in an
  `after_initialize` block, now makes the module's methods the class's own (they were name
  guesses) and its instance variables read the class's writes. Only calls that run when the file
  loads count, never one in a method, a condition or a spec file.
- **A column's writer has a card and a place.** `self.user_id = nil` in a model jumps to the
  schema's line and says Rails defines it, as `user_id` did; it used to answer nothing. Completion
  lists `user_id=` beside the reader.
- **A concern's `attr_accessor` in `class_methods` is a class method of every includer.**
  `self.abstract_class = true`, Rails' own `attr_accessor` in `ActiveRecord::Inheritance`, used to
  answer nothing; only a `def` there was read.
- **A call on a class answers nothing rather than another class's instance method.** Where a
  class has no such method (a gem's macro made it, or a YAML file declares it), the name match
  used to fall back to any instance method of that name, which that class can never reach: a
  `Settings::General.app_domain` went to the application's `config.app_domain =`, a
  `SiteSetting.uncategorized_category_id` to a serializer, `I18n.locale =` to five unrelated
  classes. A module's method is still offered, since an `extend` may reach it, and so is a call in
  a block written into a class body, which may run on something else.
- **Hover and go-to-definition answer on two calls rubydex misfiles.** The member of
  `record.name ||= value` (and `&&=`, `+=` and the other operator writes) is answered on its name,
  where it used to answer nothing or only by a name match. A call inside a constant path's parent,
  `record.class::LIMITS`, has no reference at all upstream and is now read from the code.
- **An instance variable read with no type gets the card of its write**, where it showed nothing.
  Where the file itself never writes it (a subclass reading what its parent's `before_action`
  sets), go-to-definition now lists the writes in the parent, the included modules and the
  subclasses: the same writes its type is read from.
- **An instance variable in a partial, or in a lambda a Rails macro runs on the object, is
  answered.** A partial's variables come from every controller and mailer that renders a view, even
  where its folder names no class (`user_notifications/digest/_stats`), and go-to-definition lists
  each write instead of none. A read in `after_action …, if: -> { @payload }`, in a mailer's
  `default to: -> { @user.email }`, or in `included do` of a concern with one includer is that
  object's variable, and highlight lights it with the method that writes it.
- **An instance variable named by a symbol is answered like a read of it**:
  `delegate :render, to: :@template`, `def_delegators :@items, :size` and
  `record.instance_variable_get(:@x)` get the card and the go-to-definition a read gets.
- **A helper's instance variables are its views' controllers'**: a helper reading `@home_page`
  jumps to the controller that sets it. A controller that renders another folder's view by name
  (`render template: "articles/index"`) now counts as a renderer for partials and helpers.
- **Go-to-definition lands on the `instance_variable_set` or `attr_writer` that writes a
  variable** where no line spells it, and a partial's jump lists every controller's writes it can
  read, even when one controller's ancestors cannot be read.
- **A variable an application's `instance_variable_set("@#{name}", …)` can write is no longer
  typed from the other writes alone.** The file was skipped for not spelling the name, so the
  type ignored a write it cannot know.
- **A namespace a directory declares lists the files that open it, even where a `module` line
  declares it too.** Zeitwerk defines the module from the directory either way, so each file that
  opens it is one of its places. The written lines come first. Not where a `class` declares it.
- **A call on `self` in a concern's `included do` no longer claims the module is the receiver**,
  which Rails never makes it.
- **A `has_many`'s generated signature no longer calls the collection one record**:
  `has_many :comments`, *a collection of `Comment`*, where it said *which is a `Comment`*.
- **Rails types**
  - **A `belongs_to` reader is always `nil`-able**, whatever `optional:`, `required:` or
    `belongs_to_required_by_default` says: those are a validation run on save, and
    `Comment.new.post` is `nil` all the same, as is a key whose row was deleted where no foreign
    key constrains it. A call on the reader keeps its type where `nil` has no such method
    (`comment.post.title` is still a `String`). Columns declared `null: false` stay non-`nil`.
  - **`relation.each { }` is the loaded `Array`**, not the relation: Rails hands `each` to the
    records.
  - **An association's `delete` and `destroy` hand back the removed records, or `nil`**, and its
    `destroy_all` is `nil` for an empty one; they read as a count and a record.
    `Story.destroy(id)` is the record or `false`.
  - **`relation.new([{…}, {…}])` is an `Array`**, as `build` already was.
  - **A `scope` whose lambda returns something else answers that**: Rails hands back the lambda's
    value unless it is `nil` or `false`, so `scope :latest, -> { order(:id).first }` is the record
    or the relation, not the relation.
  - **A model with an `interval` column no longer says its `sum` and `average` are `Numeric`**:
    they are a `Duration` there, and both now answer nothing on that model.
- **Types**
  - **`Foo.new`, where `Foo = Bar`, builds a `Bar`**: arel's `table[:name]` is an
    `Arel::Attributes::Attribute`, so `matches`, `lt` and the other predicates answer.
  - **A top-level constant used in a `SimpleDelegator` subclass keeps its answer after an edit**:
    `Current.account` in a presenter lost its type and jump once the file was re-indexed, since
    `Delegator` inherits from `BasicObject`. Ruby finds it through `Delegator.const_missing`, and so
    does ya-lsp now.
  - **The first hover or jump in a spec file just opened waits for its groups** (one short settle)
    instead of answering from before them: a `let` answered with every method of its name.
  - **Inside a `define_method` block, `self` is the instance**, not the class: a call there reached
    the class object's method of the same name and could show its type. A block whose signature
    says `self` is something ya-lsp cannot read now answers nothing there, instead of the code
    around it.
  - **Inside a module's method, `Object`'s and `Kernel`'s methods are found**: `send`, `format`
    or `instance_variable_get` called on `self` there answer, since whatever includes the module
    is an object.
  - **A call with keywords reaches the signatures its keywords can run.** A signature that
    requires a keyword is no longer one a call without it can reach, and a call writing a keyword
    no signature takes reaches none. `CSV.read(path)` is `Array`, and `CSV.read(path, headers: true)`
    is `CSV::Table | Array`: before, the method's own code answered, and it said `Array`, which is
    wrong. `foo(**opts)` also reaches the signatures of `foo()`, since `opts` may be empty.
  - **A `datetime` column is an `ActiveSupport::TimeWithZone`**, not a `Time`, as Rails returns
    it. A project that changes Rails' time-zone setting (`time_zone_aware_attributes`,
    `time_zone_aware_types`, `skip_time_zone_conversion_for_attributes` or PostgreSQL's
    `datetime_type`) gets no type for these columns. `attribute :at, :datetime` is a
    `TimeWithZone` on a model and untyped elsewhere, where Rails makes it a plain `Time`.
  - **A `decimal` column with no digits after the point is an `Integer`**, as Rails reads it:
    `t.decimal "x", precision: 10` in `schema.rb`, `numeric(10)` in `structure.sql`.
  - **More PostgreSQL column types are typed:** `uuid`, `citext`, `ltree`, `tsvector`, `xml`,
    `macaddr` and `bit` are `String`, `inet` and `cidr` are `IPAddr`, `hstore` a `Hash`, `money` a
    `BigDecimal` and `oid` an `Integer`. `timestamptz` is no longer typed: Rails 7.0 reads it as a
    `Time`, and 7.1 and later as a `TimeWithZone`.
  - **`attribute`'s cast type is looked up in Rails' own list**, which is not the schema's: `:bigint`
    is not a cast type (Rails raises), and `:big_integer` and `:immutable_string` are.
  - **Several YARD `@return` tags are read together**, as YARD's one-tag-per-case convention
    means: `@return [String]` beside `@return [NilClass]` is `String?`, and `[TrueClass]` beside
    `[FalseClass]` is `bool`. Only the last tag was read, which answered `nil` or `false` for such
    a method.
  - **An `attribute` with a type ya-lsp cannot name replaces the column's type**, as Rails does:
    `attribute :price, :money` or `attribute :price, Money::Type.new` on a `decimal` column is
    untyped, no longer the column's `BigDecimal`.
  - A call on a value that may be `nil` also asks what `nil` answers. `user.nil?` is `bool`, not
    `false`. `record.present?` is `bool`, not `true`. `record.dup` is `Record?`. Where `nil` has
    no such method, the answer is unchanged.
  - `a&.b` can be `nil`: `user&.id` is `Integer?`, not `Integer`. Only that one call is skipped,
    so `user&.name.nil?` is `bool`.
  - `!x` is `bool` where `x` is typed only by its name (`!admin`). It used to draw no label at all.
  - **A variable's type is every value that can reach it**, not its last assignment.
    - A write in a branch, a loop or a block adds to the one before it:
      `x = "a"; x = 1 if c; x` is `String | Integer`, where it used to be `Integer`.
    - A variable that no write sets on every path can be `nil`: `x = 1 if c; x` is `Integer?`.
    - `x ||= v`, `x &&= v` and `x += v` count as writes.
  - **A variable with a write ya-lsp cannot type gets no label**, even when another write could
    be typed. Labels that were right only by luck are gone with it. One wrong label this removes
    is `-> nil` on every Rails controller action that ends in `render`.
  - **An instance variable is every write any class of its object makes**, because methods run in
    any order and any of them can run on it: a superclass, an included module, a subclass, and
    another file reopening the class. It is `nil`-able unless every class the object can be sets
    it in `initialize` (directly or through `super`), or the reading method sets it first. A
    template's instance variable is every write its controller's classes make.
    - `attr_writer` and `attr_accessor` store whatever a caller passes, so a variable they write
      gets no label.
    - A write in an included gem module counts: a class's own `@errors = []` beside
      `ActiveModel::Validations` is `ActiveModel::Errors? | Array`.
    - A method read for a known receiver hears only that receiver's classes: `record.errors` on a
      `Story` is not widened by another class that includes the same module.
    - A class whose superclass Ruby itself could not load is refused, because its real superclass
      is unknown.
    - `instance_variable_set` and `remove_instance_variable` count as writes. One on another
      object (`record.instance_variable_set(:@tags, v)`) refuses every variable of that name,
      since the receiver is rarely known; an interpolated or passed-in name refuses the names it
      can spell.
    - A class that inherits from a library's class keeps its `?` even when `initialize` sets the
      variable: a library can build it without running `initialize`, as Active Record does for a
      record it loads.
  - **ActiveRecord's query interface no longer answers one shape for a call that can return two.**
    - `Story.find(x)`, `destroy(x)`, `create(x)` and `build(x)` are a record for one thing and an
      `Array` for an array. The argument's class decides, and an argument nothing types (like
      `params[:id]`, which a request can send as an array) gets no label.
    - A relation's `count`, `sum` and `average` can be a `Hash`, after `group`. `Story.count` is
      still an `Integer`.
    - A bare `where` (`Story.where.not(…)`) is no longer called a relation.
  - **A method read for a known receiver calls that receiver's methods**, as Ruby does. Inside an
    inherited method, a call like `parse` reaches the receiver's class first:
    `CsvImporter.new.run` uses `CsvImporter#parse`, not the `parse` beside `Importer#run`. A
    service's `self.call` builds and calls the service it was called on, and a module's method
    read for a class that includes it calls that class's methods. Labels that took the ancestor's
    step (a `nil` from a base class's empty hook) are corrected.
  - `x.class` is `x`'s own class, not any `Class`: `self.class.new(…)` is another object of the
    same class, and `self.class.default_scope_name` is that class's method. A class that defines
    its own `class` keeps its answer.
  - A class object is labelled `Foo:class`: `def model_class; User; end` is `-> User:class`, and
    so is `self.class` inside `User`. A module object is still not labelled.
  - **A call is typed by its method's body with the arguments it passed**, where the method's
    parameters have no type: `Foo.bar(1)` is `Integer` and `Foo.bar("x")` a `String` for
    `def self.bar(baz) = baz`, whose own label stays empty. Positionals bind by position and
    keywords by name; an argument left out holds its default at that call. The call's hover card
    shows its type and says it is the call's.
  - **An `attr_reader` returns its instance variable**, typed by every write to it: `attr_reader
    :topic` with `@topic = Topic.new` is `-> Topic`. A reader in `class << self` returns the class
    object's variable. An `attr_accessor` answers nothing, since its setter can write anything.
  - A signature's `attr_reader name: T` (and `attr_accessor`) types the reader, as a `def` would:
    `uri.host` is `String?` and `response.code` a `String`.
  - **An object holds what `new` passed it**: `Service.new(story).call` reads `@story = story` in
    `initialize` as a `Story`, for that object only. A class with its own `self.new` binds
    nothing.
  - A receiverless call in a helper or a template is typed through the view context, as its hover
    card already was: `def headline; shout; end` in one helper is what another helper's `shout`
    returns, and `link_to …` in a helper is an `ActiveSupport::SafeBuffer`.
  - A method only the test suite mixes into a class (a spec file's `extend`) no longer answers for
    that class in application code: `MessageBus.publish` is the gem's, not a spec helper's.
  - A hover card whose return was read out of a method body built on a guessed name now says it
    was guessed, as the margin already treated it.
  - A class declared inside `class << self` is never drawn under rubydex's name for it
    (`Orchestrator::<Orchestrator>::Params`), which no Ruby can write.
  - A call written with keywords reaches the arms that take them, and one taking an options
    `Hash`: `update_all(status: "x")` is an `Integer`, and `transform_keys(a: :b)` is a `Hash`, not
    an `Enumerator`.
  - **A template's instance variable is every write of every class that renders it**: the
    controller its path names, and any class that renders it by name (`render "stories/show"`).
    A partial is every class a view is rendered by, since its path names none. A template
    rendered with `ApplicationController.render(…)` gets no label: its variables come from
    `assigns:`.
  - A method's parameter is no longer typed by a same-named local in another method.
  - A type read off a name is never drawn inside another type. For example, `[1].map { |v| prep(v) }`
    was labelled `Array[Ledger]` because a local inside `prep` was spelled `ledger`.
  - A block on a value that may be `nil` is handed `nil` too, where `nil` has the method:
    `maybe.then { |v| v }` makes `v` a `String?`, and the call a `String?`. `&.then` does not.
  - **A parameter is no longer typed by its default.** `def f(limit = 10)` says nothing about
    `limit`, because a caller may pass anything. A declared type (RBS, a Sorbet `sig`, a YARD
    `@param`) still types it.
  - An element that may be `nil` is no longer drawn as if it never is: `Array[String?]` and a
    `map` whose block can return `nil` are drawn `Array`, not `Array[String]`.
  - A method ending in a long `elsif` chain, a conditional inside an `else`, or a `begin`/`rescue`
    holding one is typed again. Every `elsif` and `else` counted as a level of nesting of its own,
    so a fourth branch was given up on even when every branch is a `String`. Conditionals nested
    up to nine deep are read; four was the limit before.
  - Limits on how far a type is followed no longer cut real code. A call with more than 20
    arguments or keywords binds them all, a chain of more than 20 calls is followed, a method
    whose answer is more than ten methods deep is read, and a constant built from constants four
    deep (addressable's `QUERY`) is a `String`.
  - **A controller's `params`, `request`, `response`, `session`, `flash` and `cookies` have their
    Rails classes**, and so do a template's and a helper's `request`, `response`, `session`,
    `flash` and `cookies`. `params.permit(…)` is an `ActionController::Parameters`, and
    `request.original_url` a `String`. `session` is the application's session, not the one a
    controller test stores. A template's `request` can be `nil`, as it is in a mailer's template.
    A controller's `cookies` stays private.
  - **`Rails.root`, `Time.zone` and the controller types no longer need a
    `config/application.rb`**: an engine or a gem monorepo whose bundle holds the framework gets
    them too.
  - **A class whose superclass is spelled like itself inherits from the class Ruby names**:
    `class ApplicationController < ApplicationController` inside `module Admin` is the top-level
    `ApplicationController`'s subclass, and `class Scope < Scope` inside a policy is its parent
    policy's `Scope`'s. The class and every class below it reach the parent's methods, completions,
    instance variables, supertypes and subtypes, and so do their templates. Hover and go-to
    definition on that superclass name go to the parent; where Ruby would raise, they answer
    nothing.
  - **A `raise` no longer erases a method's type.** `return name if name; raise "missing"` is a
    `String`: a `raise` or `fail` hands nothing back, so it is left out of what a method or a block
    returns. `x || raise` is `x` without its `nil`.
  - **Inlay hints read a file once per request**, not four times on first open and twice after.
  - **A block's type reads every branch.** `map { c ? "x" : maybe }` was `Array[String]` where
    `maybe` can be `nil`, and `then { c ? true : flag }` was `true`; they are `Array` and `bool`.
    The answer no longer depends on which branch is written first.
  - **`new` in a subclass is the subclass**, not the parent an inherited signature names
    (`Tempfile.new` reached from `Paperclip::Tempfile`).
  - **A method two Ruby bodies define beside a signature shows the same union in the margin as in
    a chain**, instead of the signature alone.
  - **A method written inside a block (`class_eval`, `Struct.new do`) no longer types calls on
    every object**, and a private method hands a block nothing on another object. Navigation
    already refused both.
  - **Hover, completion and go-to answer from the same per-request memory as inlay hints**, so an
    answer no longer depends on which request asked. A union lists its classes before `true` and
    `false` everywhere, as variables already did.
  - **A private method no longer types a call written on another object**, where Ruby raises.
    `Oj.load(…)` was `bool` and `Process.spawn(…)` was `Integer`, both read from `Kernel`'s
    private copies.
  - **`Story.all` and `Story.unscoped` are a `Story::Relation`**, on a model and on a relation, so
    the calls after them are typed too. `Story.unscoped { … }` returns what its block returns, and
    gets no type. Go-to definition on `Story.all` goes to Rails' `Scoping::Named#all`, as before.
  - **A `def` written in an RSpec group no longer takes its margin from every spec's `def` of that
    name.** rubydex files them all as one `Object` method, so two spec files' `def helper` read
    `-> String | Integer` in both.
  - **A hover no longer says a method's body was read where its signatures answered.** A call whose
    arguments pick one of several signatures, or whose receiver fills in a generated return, now
    reads "what the method declares, read with this call's receiver and arguments".
- **Go to definition**
  - **`count`, `sum`, `average`, `first`, `last`, `pluck`, `merge`, `empty?` and `create` on a
    model or relation go to the `def` Rails calls**: `Calculations#count`, `SpawnMethods#merge`.
    They went to a class inside `ActiveRecord::Relation` with a method of that name
    (`explain.count`'s proxy, the `Merger`). `+`, `-` and `|`, which Rails hands to the records,
    now go nowhere instead of to `WhereClause`.
- **Speed**
  - **Every re-index after an edit takes a third less time** on a large app: 217 ms,
    was 336 ms. Finding ActiveRecord's classes scanned every name in the bundle, seven times; four
    readers each walked every class in it; every spec file was checked on disk, though only open
    ones are read; and each column's generated methods were filed twice.
  - **Opening or closing a spec file costs a third of what it did**: 96 ms on a large app, was
    294 ms. Only the RSpec reading runs again, not every model, schema and struct in the
    project, and only its own documents are looked up afterwards.
  - **A hover inside a gem whose base class has hundreds of subclasses takes 30 ms, was 190 ms**
    (`shopify_api`'s REST resources). Reading an instance variable asks every file of every
    class the object can be; those files are now kept between requests, parsed and unparsed,
    instead of read from disk and parsed again each time.
  - **Go-to-definition on `describe` or `it` in a spec being edited no longer re-indexes first**
    to find it still has nowhere to go.
  - **Hover and go-to-definition spend a quarter to two thirds less time**, with the same answers:
    on the largest app a third of what they did, on smaller ones 18–37% less. The classes a
    view or partial can be rendered by are kept until the project changes, not found again for
    every request. Finding which constant a call's receiver names searches a sorted index of a
    large file's spans, not the whole file. An instance variable's writers are checked for
    `instance_variable_set` in the index, not by scanning each file's text twice. And the file and
    line a hover's note names are worked out once, not for every value a method can return.
  - **A hover on an instance variable in a view or a helper takes under 2 ms, was 5–7 ms**, with
    the same answers. The classes an object can be, and the files that can
    write its variables, are kept until the project changes, not worked out again for every
    request by reading each subclass's path. Hover's slowest twentieth takes 29% less time.
  - **Hover takes another fifth less time on Apple silicon**, with the same answers (slowest
    twentieth 1.6 ms, was 2.0). Reading an instance variable searches the text
    of every file that could write it for its name, and that search is now vectorised on ARM too.
  - **Hover parses the file under the cursor once**, not up to six times, with the same answers:
    its typical answer takes an eighth to a third less time, and its slowest twentieth 1.1–1.3 ms,
    was 1.7. Go-to-definition, highlight and completion share
    one parse between their own questions too.
  - **Hover's slowest twentieth takes about 1.0 ms**, was 1.2–1.3,
    with the same answers. Checking whether a `private` inside a block reaches the methods below
    it no longer re-reads the file on every request, only when its text changes. Whether a file
    is a test, a migration or a generator template is worked out once, not on every request.
  - **A long session peaks 35–100 MB lower**, with the same answers and the same speed: 543 MB
    on a small app, was 578; 921 MB on a mid-size one, was 986; 1.1 GB on the largest, was 1.2. What a file
    is kept as between requests takes 29% less room: a method parameter is stored once, not once
    per read of it. A file's text is shared with each request instead of copied into it. And at
    most 32 MB of text is kept, not 64.
  - **The check after an edit to a model takes a fifth less of ya-lsp's own time**: 101 ms on
    a large app, was 128. Files with a `Struct.new` or a `define_method` are read again only when
    they change, not after every edit, and gathering what every file declares no longer copies
    each one's list first.

## [0.6.0] — 2026-09-23

### Added

- **Claude Code plugin.** Install it with `/plugin marketplace add ar2em1s/ya-lsp`, then
  `/plugin install ya-lsp@ya-lsp`. The agent's `LSP` tool then answers about Ruby.
  - `/ya-lsp:ya-lsp-setup` installs or updates the binary and proves it works on your project.
    It asks before it downloads or writes anything.
  - Known limits: `Gemfile` and `Rakefile` are not routed, and jumps into git-ignored
    directories are hidden.
- **Go to implementation.** Returns the `def` a call reaches, then every override below the
  receiver's own class. The definition itself is always listed first.
- **Go to type definition.** Returns the class of the value under the cursor. At `story.author`
  that is `class Author`, not `def author`. It also works on locals.
- **Go to declaration.** Returns the RBS signature of a method, such as `string.rbs` for
  `String#split`. If there is no signature it returns nothing.
- **Show the RBS ya-lsp generated.** A code action on a Rails-generated member (a column, an
  association, an enum or a route) opens the RBS behind it, read-only, and refreshes it when the
  source changes. It needs a client that can both show and read a document, which VS Code can.
- **Renaming a Rails file renames its class.** Moving `order.rb` to `purchase.rb` renames `Order`
  to `Purchase` everywhere, in the same undo step. Any move the rule does not cover stays a plain
  file move with no message. Needs `rails.enabled` and a client that asks before moving files.
- **Unsaved buffers are answered.** An `Untitled-1` set to Ruby gets an outline, completion,
  hover, jumps into your project, and parse errors. Nothing in it leaks back into your project.
- **`.jbuilder`, `.builder` and `.ruby` views are indexed** as plain Ruby. **If you set your own
  `[index] include`, add these three globs to it yourself.**
- **ya-lsp watches files itself when the editor cannot**, as in Claude Code, Helix, eglot and
  Neovim on Linux. A `git checkout`, a rebase, or an agent editing through a shell is picked up
  with no restart.
- **Open files follow the disk** where ya-lsp is the only watcher, unless the buffer has
  unsaved changes.

### Fixed

- **Types**
  - `1 + 2` is an `Integer`, and `1.5 + 1` is a `Float` (it used to say `BigDecimal`). When one
    method has several overloads, the arguments you pass now choose between them.
  - A method returning `self` returns the receiver's type. For example, `params[:q].presence` is a
    `String?`.
  - `!x` is `bool`. This gives `blank?`, `present?` and most predicates a type.
  - `.nil?` and about 70 other core methods that return a literal (`() -> false`) now have a type.
  - `alias` and `alias_method` carry the renamed method's type, so `[1].blank?` gets one.
  - `true`/`false` receivers ask both classes, so `x.empty?.blank?` is `bool`, not `false`.
  - `:x.as_json` is no longer a `Hash`. A core signature that two gems' bodies disagree with is
    now shown alongside those bodies.
- **Jumps**
  - A `def` written inside a block, such as `class_eval do` or an RSpec `describe`, is no longer
    offered on every object. `Foo.new` no longer jumps into a gem's monkey patch.
  - A `def` two generators both declared is listed once, not twice.
  - Jumping to a generated member now selects the whole method, not just its first line.
- **Scope**
  - A file opened *outside* the project can still read the project, but the project no longer
    sees it: no symbols, jumps or renames into it.
- **Startup and indexing**
  - The first request on a cold server waits for the index instead of answering empty.
  - Every `*.ru` file is indexed, not only `config.ru`.

## [0.5.1] — 2026-09-17

### Fixed

- **A workspace folder inside another workspace folder no longer answers everything twice.** VS
  Code allows the nesting — a monorepo listing the repository for the code beside its applications,
  and each application for its own `Gemfile.lock` — and the editor resolves a file in the inner
  folder to the innermost one. A document selector cannot say that: an LSP glob has no way to
  subtract a path, so the outer folder's client claimed the inner folder's files too, both servers
  answered, and every hover card, completion item and diagnostic arrived twice. The nesting cannot
  simply be refused, because each folder may hold the only lockfile for its own code and a server
  resolves exactly one bundle. The outer client now stops at the boundary instead. A root that lies
  inside another folder is likewise refused at registration, where the same duplicate used to arrive
  from the opposite direction.
- **`[index] load_paths` indexes the roots it names.** It documented itself as "extra roots to index"
  and indexed nothing — it reached `require` resolution and stopped — so a project that named a tree
  outside its root got a `require` that resolved to a document the graph did not hold. Both spellings
  of such a path failed on top of that, and silently: `../shared` carried its `..` into comparisons
  against URIs that never contain one, and a directory reached through a **symlink** was never walked
  at all. Both resolve now, the tree is indexed, it counts as the user's own code — diagnostics,
  rename and search all treat it as the project's rather than as somebody's gem — and the server asks
  the editor to claim it, since no client's selector reaches outside its own folder.

## [0.5.0] — 2026-09-16

### Added

**Three more requests**

- **Inlay hints, and a tier that is never drawn.** Three families, each with its own switch: what a
  block parameter receives (`stories.each do |story|`), what a call hands back
  (`author = story.author`), and what a `def` returns where the source does not say. **A guessed
  type is never painted into a margin** — a type matched on a name alone is exactly what a margin
  must not state as fact, and the refusal is a test on the tier rather than a list of shapes, so a
  rung added below the graph next release is refused by the same line rather than appearing in
  everybody's margin the day it ships. The tier that would need no footnote turns out to be
  unreachable: a hint exists exactly where the code does *not* name the type, and where it does the
  label would only repeat the line. So every hint drawn is *Derived* and carries its footnote in the
  tooltip.
  <br>`[hints] block_parameters`, `locals` and `returns`. There is deliberately no fourth switch
  beside them: a hint is pulled rather than pushed, and every client that asks for one already has a
  master toggle of its own.

- **Call hierarchy.** `prepareCallHierarchy`, `incomingCalls` and `outgoingCalls`. The two
  directions are not each other's mirror and the answers say so. An incoming caller is a **name
  match** — nothing links a call to a declaration — so every row ends in `by name` beside the file
  it is written in. An outgoing call is a claim that an edge exists, which is a stronger thing to
  say, so it is drawn only where the call resolves precisely; a name-based fallback would answer
  `person.name` with forty edges, and forty edges is not a hierarchy.

- **Document links.** Every `require "..."` path is a link, from the same walk that answers
  go-to-definition on one — a bare `Kernel#require`, never `Foo.require`, never an interpolated
  path, never one inside a comment or a string. A require the index cannot place produces **no link
  at all**, because an underline that opens nothing is worse than a path left plain.

**The two things the graph does not model**

- **An `@ivar` read answers.** Hover and go-to-definition at `@title` give every assignment sharing
  its `self`, which is what occurrence highlighting has answered since it shipped and the other two
  requests were not asking. The card on `@story` and the card on `@story.title` are now one answer
  and cannot drift apart.

- **A macro's symbol argument resolves.** `before_action :authenticate` is a `def` in this
  controller or above it; `validates :title` is the column `db/schema.rb` declared; `belongs_to
  :user` is the reader the macro wrote. One rule and no table — **a macro's symbol argument is a
  member of the class the macro is written in** — asked through Ruby's own method lookup, so it
  holds for the twenty-three macros six real applications write and for a DSL nobody has taught
  ya-lsp about.

**Rails**

- **A template's `@story` goes to the line that assigns it.** A template has no enclosing class, so
  the search for a variable's writes found nothing by construction: the card answered and the jump
  did not. It continues into the one file those writes can be in — the controller or mailer the
  path names, taken from the same convention the card reads, so a card citing `StoriesController`
  beside a jump landing elsewhere is not a state this can reach. **Every** write, not only the typed
  ones: `@stories = Story.where(...)` is exactly the line a reader asked for. Over 1,301 template
  reads drawn from six applications, the jump went from **4 to 870**, and the card from 759 to 788.

- **A mailer's views hang off the mailer.** `app/views/user_mailer/welcome.html.erb` is `UserMailer`
  and no `UserMailerController` exists anywhere — **326 of 4,407** template instance-variable reads
  across six applications are in a mailer's views, and one of the six writes no other kind of
  template at all. Which class a path renders through is now answered in one place for the view
  context, the card and the jump alike: the controller first, the mailer only where no controller is
  declared, which is Rails' own order. The footnote names which convention answered, because *the
  controller Rails renders this template from* is a false sentence about a mailer.

- **A concern's class methods are declared, in all three spellings** — `class_methods do`, a
  hand-written `module ClassMethods`, and an `included do ... extend M` whose `def`s are in a file
  the concern only names. Each `def` is written onto the class side of every class that includes the
  concern, transitively, and the jump lands on the `def` somebody typed however many classes it was
  written onto. **Including the ones in your gems**: `validates`, `scope`, `belongs_to` and
  `has_many` are all a Rails concern's `ClassMethods`, and `ActiveModel::API` installs `model_name`
  on every model in every application. Over one application's `model_name` and
  `human_attribute_name` cursors, 20 of 20 left the guessed tier and every one lands in
  `active_model/translation.rb` or `naming.rb`. A per-position tier comparison over 1,200 cursors in
  each of six applications: **0 worse, 1 better**.

- **`Story.where` has somewhere to jump to.** No file in your project declares it, so a *Resolved*
  card over it used to send a reader nowhere. The place is now **found rather than read** — looked
  up in Ruby's own ancestry after the index settles, since a generator cannot ask a graph it is
  still writing — and answers `activerecord/lib/active_record/relation/query_methods.rb:1033`.
  Checked against `Method#source_location` under activerecord 7.2.3.1, 8.0.5, 8.0.5.1 and 8.1.3.1:
  of the 127 names, **125 resolve and all 125 land on the line Ruby names**. One `def` that several
  declarations name is still one place, so a model no longer offers the same line five times.

- **A namespace a directory declares.** `class Shop::Utils` where no file declares `Shop`
  raises `NameError` in Ruby and runs under Rails, because a directory with no matching `.rb` *is*
  the declaration. ya-lsp reads it, with Rails' own three bounds — the `app` anchor an engine keeps,
  the `assets`/`javascript`/`views` exclusion, and `app/{*,*/concerns}`. Over six applications, jump
  targets described as nothing in particular fell from **126 to 18**, and on the largest from 39 to
  1.

- **`source_type:` is read**, and the keyword beside it no longer guesses. `has_many :records,
  through: :references, source: :record` names a *member* of the joined model, so camelizing it
  produces a class the application does not have — on every line in six corpora that writes the
  pair. `source_type:` is what names the class, and it outranks its neighbour.

- **A `scope` answers one call later.** `Story.recent.visible` jumps to the `scope :visible` line
  exactly as `Story.visible` does, because one macro line is two declarations now — the class side
  and the relation — carrying one span. `enum`'s class-side pair is a `scope` Rails writes itself
  and gets the same two.

- **A constant is not the class it holds.** `ENV.fetch` types from Ruby's own signature; where
  nothing will ever ship a signature — an application's own `Spree::Config =
  Spree::AppConfiguration.new` — the Ruby that assigns it says the type as plainly as RBS would, and
  is resolved in the nesting of the file that wrote it.

- **The framework's own singletons.** `Rails.root` is a `Pathname`, `Rails.cache` an
  `ActiveSupport::Cache::Store`, `Time.zone` an `ActiveSupport::TimeZone`. railties and activesupport
  ship no `sig/`, so every chain written on one used to die at the first `.`.

- **A decline is a declaration rather than a silence.** `belongs_to :owner, polymorphic: true` and a
  `class_name:` naming a class that is not there declare `owner` as untyped, spanned onto the macro
  line: go-to-definition lands on the `belongs_to` you wrote, hover says the type is not known, and
  the name-based guess is never reached rather than reached and refused.

**Configuration**

- **Eleven keys for what a project can turn off, and four for the log.** `[rails] enabled` is
  `auto`, `true` or `false` — `auto` looks for `config/application.rb`, then railties in
  `Gemfile.lock`, and says in the log which way it went — with `schema`, `models`, `routes`,
  `entrypoints` and `views` under it; `[types] guess_from_names`, `structs` and `annotations`;
  `[hints]`' three families; and `[trees] test`, `test_support` and `migration`, which say where
  *this* project keeps the trees the fences are about. A project that is not Rails should not be
  told about Rails, and one with an `app/views/` should not get a card citing a controller it does
  not have.
  <br>`[log] file` writes a second copy to disk at its own level, for a bug report. VS Code exposes
  every one of these, with the TOML key in each description, and a committed `ya-lsp.toml` still
  wins over all of them.

### Changed

- **An untyped receiver no longer answers with the universe.** Where nothing types the receiver,
  completion built a query that every method name in the graph matched — 26,073 candidates on the
  smallest application tested to 45,953 on the largest — sliced to 512 by a cap, with the word the
  file actually wrote sitting at median rank 4,070. There are two ceilings now: over 512 candidates
  nothing is offered at all, and under it the best 128. That is one rule seen at two prefix lengths,
  so **nothing is offered for the first three keystrokes and the list reappears as the word narrows
  it**: 0% of untyped cursors fit under the bound at two characters, 45% at three, 74% at four, 95%
  at five. The list stays marked incomplete on the decline, so your editor asks again inside the
  word rather than filtering an empty list locally.
  <br>**Cost, measured:** 159 cursors lost a list that held the word, and **17 of them — 0.8% of
  the 2,143 sampled — would have shown it in the top ten**.

- **What your application can actually load decides every list that answers *what can I call*.** A
  deny-list of four directory names lived in one module and one rung read it; every other surface
  was free to send a reader into a spec, and most of them did. It is one rule now, with an
  exhaustive table of who reads it: completion, the name and root rungs of hover and
  go-to-definition, the place list those two answer from, signature help and the outgoing callee
  **drop** what the application cannot load; workspace symbol search and the subtype list **rank**
  it down, because a drop makes a real declaration unfindable by the only means of looking for it;
  and find-references, rename, occurrence highlighting, incoming callers and supertypes **never
  ask** — a use under `spec/` is a use, and three tests exist to fail if anyone wires the fence into
  them.
  <br>Measured over six applications: of 4,251 answered bare calls outside the test trees, **31
  resolved onto a spec-only member of `Object`, `Module` or `Class` and all 31 were wrong** — now 0,
  falling through to the name rung rather than to silence. Completion's test-only rows per
  application went 28, 455, 947, 1,780, 3,267 and 4,390 to five zeroes and a 20, with no list
  emptied. 24,143 places under a test tree left the jump lists of 413 cursors, and **no list was
  emptied and none grew**.

- **A migration is real Ruby that nothing autoloads, and it is no longer offered.** One application
  had **581 of its 2,387 generated `def`s coming out of two files in `db/old_migrations/`** — 24% of
  what the Rails readers write, in a directory a rake task loads one file of, in a process of its
  own, which is exactly why people write a private copy of a model inside one. `db/migrate`,
  `db/post_migrate` and `db/old_migrations` are all matched, and the fence is on the **cursor's**
  side only: inside a migration the constant really does resolve to the copy at the top of that
  file.
  <br>Measured: 261 places left 20 jump lists, **every one of them a migration and none emptied**;
  1,265 rows left the symbol picker and **not one of them was real**; 232 completion labels went,
  over 41 names, all declared only inside a migration — and **not one list on any application
  changes its first row**.

- **A gem's library directory called `test` is a library.** `rack/test/`, railties'
  `rails/commands/test/`, `rbs`'s `sig/test/` and the vendored minitest signatures every project
  has were all being read as somebody's test suite, so the members in them answered nothing. The
  fence carries the workspace layout beside the cursor now, because those two halves coming apart
  *was* the defect. Over 6,836 drawn call cursors: **1,939 lists grew, 0 shrank, and 2,906 places
  came back across 95 files.**

- **A generator's template is in neither environment.** Ruby a gem ships in order to *copy* it, one
  day, into a project that does not exist yet — it loads nowhere, ever, so no load-path rule could
  settle it. Of 5,859 drawn cursors, 731 lists shrank and **767 places were dropped, all 767 of them
  a template**: no list lost a real place.

- **A Ruby file you jump into answers like any other file.** Go-to-definition into a gem, Ruby's own
  library or the signatures beside them landed you in a file where every further request was dead,
  with nothing on screen to say so — a document selector naming the workspace folder is the only
  gate there is. The server now registers the roots it has answers about, over the channel the file
  watcher already uses, because it is the only side that knows where a bundle is; a client that
  declines dynamic registration keeps what it had and is told once. In a multi-root workspace each
  folder's server registers its own and one of them claims each shared root, so two folders on one
  Ruby produce **one hover card, not two**.

- **Typing costs less on a large application, and the bigger it is the more it saves.** Two
  measurements, on the largest application tested. The walk that decides what the Rails readers see
  was one function called 25,900 times every time the index settles, whose answer for 25,899 of them
  was the answer it gave last time; it remembers them now, keyed on what the index actually re-read, and goes
  **275 ms to 70** with the pass around it 340 to 132. And a generated document is now one per
  *body* rather than one per file, so **a keystroke that re-types a column goes 2.46 s to 0.31** —
  it re-indexes one table rather than all 3,180 columns.
  <br>**Cost:** a cold open pays 1.4% (8.82 s to 8.94) for having more documents, and resident
  memory does not move. The objection — a migration that moves every table at once — was checked
  against 1,007 real migrations: **a median of 1 table, 92.8% exactly one, 98.4% one or two.**

- **Which of a wide constant's places the jump opens.** Where a name is written in dozens of files,
  the file named after the constant now wins — squashed to letters and digits, an exact match first
  — applied to namespaces only, because a file is conventionally named after the class in it while a
  method's file is named after its class rather than after the method.

- **A span that begins at the cursor wins.** Where several spans cover one offset, one that *starts*
  there is preferred and the rest are dropped before narrowest-wins runs. The obvious rule —
  anything containing the cursor — was written first and regressed 29 cursors, because a method
  reference is recorded over the whole call expression as well as over the name.

### Fixed

- **A template's hover no longer cites an assignment that is not there.** Hovering `@messages` in a
  view printed *Type taken from the assignment on line 7*, naming a line of that template that holds
  no assignment — and, in one real application, holds markup. The offset was the controller's: the
  chain is resolved over there, and every consumer turns that offset into a line against the file
  the cursor is in. The view rung drops it; the footnote below it already names the controller *and*
  the line, which is the line that assignment is really written on.

- **`Story.select` is not `IO.select`.** Looking a generated member up in Ruby's ancestry can hit
  the object model, where `Kernel` alone declares `select`, `format`, `open` and `test` — four
  positions answered the query interface with `IO`'s method. A hit on Ruby's own object model is not
  an answer.

- **A jump no longer lands on a private method the code cannot call.** Ruby permits a private method
  with no receiver written, or with one spelled `self`, and raises otherwise — a rule completion
  applied and navigation did not. The first line of nearly every spec file was the cost: `RSpec.describe`
  resolved to minitest's private `Kernel#describe` and said *Resolved*, the tier that claims the
  code names the type, while completion at the same cursor correctly declined to offer it. Measured
  over six real applications, 90 such cards; now 1, and that one is the completion cap rather than
  this. Where the refusal leaves nothing — `RSpec.describe` is defined dynamically, so no file
  declares it — the answer is no card rather than a worse one, and where something public remains
  the card says which receiver kept the member private instead of claiming the receiver had no type.
  Signature help went with it, so the popup and the jump cannot disagree about one cursor.

- **A `private` written inside a block no longer hides the public methods below it.** rubydex reads
  a bare `private` as a statement that governs the body it is written in until that body ends, and a
  block is not a body to it — so `class_methods do … private … end` in an ordinary Rails concern set
  the *module's* default visibility, and every method declared below the block was recorded private.
  Nothing acted on that record until the refusal above shipped; then it took them all. One
  application's `HasCustomFields#upsert_custom_fields` is the measured one: a public method the application calls
  on explicit receivers, answered with no card, no jump and no place in any completion list. The
  declaring file is reread at the point the refusal would be made, and the record is overturned
  where a bare modifier that escaped a block is the whole reason for it. **The card and the outline
  read it too**, so the word *private* no longer appears beside such a method in a hover card or in
  the document's symbol tree. A modifier written in a class or module body still governs every
  method below it, and one written inside a block still governs the methods inside that block.

- **A card and the list beside it agree about a block.** A bare name in a block written straight
  into a class body — `rule(:colon) { str(':') }`, `scope :recent, -> { where(...) }` — may be on
  the class object or on an instance, because re-binding `self` is what every DSL taking a block
  does. Hover had answered from the instance side since the rung shipped; completion at the same
  byte never left the class object, so **hover named a member that the list beside it did not hold**.

- **`self` inside a block in a method body.** The answer was inverted rather than absent: the
  innermost namespace and the innermost method were both asked, and the method won whenever there
  was one. The innermost body decides now, and a namespace can be the inner one.

- **`self` inside `Class.new(base) do ... end`.** Ruby's `self` there really is the new class, which
  is what the index records — but a `self` written *outside* the block and read inside it was being
  resolved against the anonymous class rather than against where it was written.

- **A concern's writers.** `self.primary_key = :id` went from answered to guessed, because the
  check on whether a name can be spelled stripped a trailing `?` or `!` and nothing else, so every
  **writer** a `ClassMethods` declares was dropped. `==` and `[]=` are still refused.

- **A deferred answer is never less than an eager one.** Hover, completion and go-to-definition
  answer against the last settled graph and translate the cursor into it; where the receiver itself
  is what is being typed there are no settled offsets for it, and the request was quietly falling
  through to the name-matched list instead of declining. It declines now, which makes the server
  settle and answer again — the same move completion already made.

- **`[types] guess_from_names = false` reaches a completion list.** The switch was read by every
  rung that draws a *card* and by nothing that builds a *list*, so it silenced the bottom tier in
  hover and left the same guess answering completion.

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

[Unreleased]: https://github.com/ar2em1s/ya-lsp/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v1.1.0
[1.0.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v1.0.0
[0.6.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.6.0
[0.5.1]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.5.1
[0.5.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.5.0
[0.4.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.4.0
[0.3.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.3.0
[0.2.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.2.0
[0.1.0]: https://github.com/ar2em1s/ya-lsp/releases/tag/v0.1.0
