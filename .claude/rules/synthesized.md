---
paths:
  - "src/analysis/synthesized.rs"
  - "src/analysis/locator.rs"
  - "src/workspace/rails/**"
  - "src/workspace/rspec.rs"
  - "src/workspace/factories.rs"
  - "src/workspace/singletons.rs"
  - "src/workspace/defines.rs"
  - "src/workspace/mixins.rs"
  - "src/workspace/i18n.rs"
  - "src/analysis/annotations.rs"
  - "src/analysis/structs.rs"
  - "src/generated.rs"
  - "src/analysis/synthesize.rs"
  - "src/knowledge/**"
---

# Declarations ya-lsp writes itself

Every generator (schema, macros, annotations, structs, routes…) ends at `generated::Facts`, which
renders RBS text. That text is indexed like any other signature and harvested by `Types::harvest`.

## The boundary

1. **Output is only RBS text.** A generator cannot special-case a hover card, reach into
   `completion.rs`, or add a rung to `types.rs`.
2. **`workspace/rails/` is pure text in, text out**: no I/O, no graph, 100% coverage. Orchestration
   lives in `knowledge/rails.rs`, which is handed closures (a document's text, its caption, "is it
   the user's own", "which document declares module X").
3. **`analysis/synthesize.rs` names no body of knowledge** (`grep -c 'rails::'` is 0). A module
   registers `ListId`s and `Wants` rows and declares in three phases: *conjure*, *declare*,
   *derive*. It also has two hooks: `after_the_walk` and `after_the_bundle`. Extra duties are
   `discover` (`db/*structure.sql`), `places_members_on` (after the resolve) and `views`.
4. **`generated::candidates` and `is_constant_path` sit in `generated.rs`**, shared by both sides.

## Places: no mapping means no place

1. **A generated declaration becomes a place only through `locator::site` →
   `analysis/synthesized.rs`.** The table is keyed by `(document, offset)` per *definition*, never
   per declaration, so a user's own `def` stays on the map. The narrowest containing mapping wins.
2. **An unmapped offset gets `Origin::Unknown`, and `site` returns `None`.** The row, link or item
   is dropped. Never guess a place.
3. **The `ya-lsp-generated:` scheme is the backstop.** `DocUri::adopt` admits only `file:` and
   `untitled:`, so a generated URI can never become a `Location`, a symbol row or a diagnostic, even
   with a bug in the table. `hierarchy::mention` is the one `Site` built without `site`; the scheme
   covers it.
4. **A generated `class Foo` is left unmapped on purpose.** `create_table "stories"` does not
   declare `Story`.
5. **The only body that is itself a place is a Zeitwerk-conjured namespace** (`Facts::namespace`
   takes an `at`: the confirming file's segment). Keep that span in the walk's `Contribution`, or it
   goes stale when a line is added above `class`.

- **Query-interface members are placed by lookup, not by reading.** Once the resolve has run,
  `Analysis::place_generated_members` looks each name up through Ruby's own method lookup:
  - `rails::RAILS_RELATION` (`ActiveRecord::Relation`) for the relation side
  - `RAILS_CLASS_SIDE` (five `ClassMethods` modules, the two `Scoping` ones before the relation
    because `all` is on both), then the relation, for the class side
  - It never looks on `ActiveRecord::Base`'s own singleton, which holds our declaration.
  - It applies `locator::ruby_s_own` (skip `Kernel#select`, etc.).
- **`Analysis::regenerate` is synthesize → resolve → place, as one function.** It is asked again on
  every resolve, and memoised only within the pass.
- **Callbacks are never looked up.** A jump into `define_model_callbacks` tells the reader nothing.

## Documents: one per source file and body

1. **`synthesized::generated_uri` is a pure, total function of the source URI and the body name**
   (`…/db/schema.rb#class:Story`). `source_of` reads it backwards. `environment`,
   `completion::Locality` and `hints` read that, not the table.
2. **`Facts::split` cuts on `render`'s key `(is_module, name)`, after every generator and after
   precedence.** No collision crosses a part, and the split is total. A generator may still return
   one `Facts`. **A `Facts::whole` is one part** (`generated::WHOLE`): a spec file's groups, forty
   bodies nobody else writes onto, are one document, not forty.
3. **`record` takes every part of a source at once**, so a dropped body is forgotten atomically.
   It updates the graph and `Types` in one function.
4. **`record` re-indexes only when the *text* changed.** Mappings are assigned either way and never
   compared.
5. **`forget` runs from `Analysis::forget`** on every route out of the index. `forget_stale` prunes
   sources the pass didn't write this time. `Analysis::generated` is the pass's own memory; never
   `retain` on the shared table.

- **`record` refuses RBS that does not parse, for the whole document, and warns.** One unparsable
  `def` costs every declaration in that document. That is why names are validated before writing
  (snake-case columns, identifier prefixes, spellable labels and actions).

## Ranking and rename

- **A generated document is scored beside its source file** (`Locality::at` via `source_of`), and
  `Ranked` has a `generated` term (`completion.md`). `own_documents` stays untouched.
- **A generated document does not vote on rename** (`rename::decide` filters it out). Models,
  mailers, jobs and any class with a `sig` now rename. A table that doesn't follow the model is not
  the server's business. A name *no* file declares gets `rename_refuses_generated` (`renaming.md`).

## Reading the generated text (`workspace/textDocumentContent`)

1. **`Synthesized::content` serves only the generated scheme.** An unknown URI is an *error*, not
   `null`, so a second server's provider gets asked.
2. **The URI travels only as a command argument.** `DocUri::from_graph_uri` and `from_lsp` both
   refuse it (`a_generated_document_is_not_a_file_and_cannot_be_turned_into_one`).
3. **The door is a code action** at a generated member or a macro's `:symbol`, gated by
   `requests::jumpable`. It needs both `window/showDocument` and
   `workspace/textDocumentContent`, and is withheld when the client lacks either (Neovim).
4. **`workspace/textDocumentContent/refresh` is sent only for documents whose text moved *and* that
   someone opened** (`Analysis::refresh_generated`).
5. **VS Code re-encodes the URI.** `workspace::uri::same_uri` compares spellings; `content` tries
   the exact key first. **The command name carries a hash of the root**, because languageclient
   registers commands globally per folder.

## The generator seam (`Facts`)

- **A writer's lone bare parameter is named `value`** (`generated::named_value`, decided
  2026-09-29): RBS lets it go unnamed, and rubydex then calls it `arg0`, which a card prints.
- **RSpec's and FactoryBot's rows name every parameter** (decided 2026-09-30): as the gems' `def`s
  name them (`expect(value)`, `let(name)`, `config.before(scope, *meta)`), and a strategy as a
  reader calls it, `create(factory, *args, **kwargs)`, with a list's `amount` after the factory.
  i18n's rows too (`i18n::lookup_parameters`): `t` and `l` are `alias`es, whose cards print only
  these, `I18n.t(key = ..., **options)` and a view's `t(key, **options)`.
- **A namespace a generator invents says what a reader sees instead** (`Knowledge::shown`,
  `rails::SHOWN`): `ActiveRecordRelation` is shown as `ActiveRecord::Relation`, `HelperProxy` as
  `ActionView::Base`, and a `RouteHelpers` member alone. `render::shown_as` applies it only where
  every definition of the name is generated.
- **One `Declared` per `(owner, name)`, rendered once at the end.** No offset arithmetic outside
  `render`.
- **Rank, less derived wins:** annotation → a struct's or `define_method`'s member → `enum` → column → association → `attribute` → long tail
  → `delegate` → convention → query interface (`Source::Interface`, lowest). At equal rank a typed
  declaration beats `untyped`, then the first to speak wins.
- **A rank across two documents is spent by the loser declining** (`Facts::source`,
  `Source::outranks`). An `enum` or `attribute` re-typing a column goes through
  `Model::retyped_columns`, and the schema withdraws that column.
- **The user's own `def` is not in this table.**
- **Phase two is a query, not a loop** (`Facts::returns` over the union built by `Facts::absorb`,
  once, last, on demand). A `delegate` through a `delegate` is not typed here: it is
  `generated::FORWARDED`, which the types table answers at the call. `Facts::returns`
  answers no sentinel (`ELEMENT`, `COLLECTION`, `WRITTEN`, `DEFINED`, `FORWARDED`, `BLOCK`, `SENT`):
  each is read against the member it was written on.
- **`Facts::mixins`, `Facts::extensions` and `Facts::inherits` are lists, not `Declared`.** A
  `Facts` holding only those is not empty. **An `extension` only on a body this document declares**:
  rubydex never links an `extend` onto somebody else's class (`navigation.md`).
- **`Facts::runs` says which class a block's `self` is**, by where the call it is passed to starts
  in the source: `None` refuses. Not RBS (a signature is per method, and two `describe` calls make
  two classes), so it travels with the mappings (`Declarations::ran` → `Synthesized::ran`), taken on
  every record, text changed or not. `types::rebound_self` asks it before any signature.
- **`Owner::Module` and `Owner::ModuleSingleton`** exist so a module never opens as `class`.

## What the pass may look at

1. **Never use `Graph::get` in the pass.** The declaration map is one settle behind. Read
   definitions instead.
2. **One walk fills every list and every projection.** Bound it with `wanted_namespaces`: filter on
   the last segment first.
3. **`Context::classes` ("may a macro name this") and `Context::namespaces` ("may a name be spelled
   around this") stay separate sets.**
4. **Never read a generated document** (`GENERATED_SCHEME`).
   `two_settles_over_an_unchanged_workspace_generate_the_same_bytes` holds it.
5. **`hosts` and `claims` are `own`-only.** They mean *the application*.

- **A superclass is spelled as written** (`spelled_name`), never with the class's nesting.
- **Order-dependence is a bug.** `claims` is sorted in `settle`. `superclasses` and `defined_in`
  take the lowest URI.
- **Engines shipped as gems:** `is_generator_source` is the second predicate. Never widen
  `is_own_code`.
  - `Wants::engines` opens `Models`, `Annotated`, `Entrypoints` and `Routes`, and closes `Schemas`
    and `Renamed`.
  - `Context::classes` widens by one `app/` per gem.
  - Captions use `gem_relative`.

## Namespaces

- **A joined RBS name introduces every segment above the last.** An undeclared top segment costs
  that module its own singleton members.
  - Safe spellings: every segment above is declared, or the immediate parent is a `module` the
    application writes.
  - `Namespaces::spellable` declines everything else.
  - Never write the namespace out as an invented wrapper. Its kind is unknowable.
  - Never synthesise segments up to the nearest declared ancestor on argument alone.
- **The pinned rubydex no longer shows the damage.** The decline is kept anyway: removing it is a
  behaviour change that needs its own sweep.
- **A directory declares a namespace (Zeitwerk)** (`rails::autoloaded_namespaces`):
  - The anchor is an `app` segment. `NOT_AUTOLOADED` covers `assets`, `javascript` and `views`.
  - `app/*/concerns` are roots.
  - **The path proposes and a file confirms**: a document in the directory must declare the
    directory's name plus one segment. `rails::confirmed_spelling` takes the file's spelling (`REST`).
  - Filtered after the walk against `bundle_namespaces`: **dropped only where a `class` declares
    the name** (`Namespaces::admits_a_module`). A directory conjures a `module`, and rubydex holds
    one declaration per constant. Where only `module` lines declare it, the directory's places join
    them (Zeitwerk defines the module from the directory either way), behind every
    written line. The whole chain is declared.
  - `own` code only.
- **A concern's `included do` and `prepended do` run as each including class**
  (`concerns::evaluated_elsewhere`): each block's call start goes out as
  `Facts::runs(module, call, Runs::Each { of, classes })` with the project's includers
  (`Elsewhere::includers`), or `Runs::Refused` where there are none. Part of `Macros::is_empty`,
  since `included` is `ActiveSupport::Concern`'s own.
- **A name rubydex invented (`<…><anonymous>`) is never an owner** (`is_constant_path`, checked by
  `spellable` and `hosts_routes`).

## Model macros

### Associations (`belongs_to`, `has_one`, `has_many`, HABTM, `scope`)

1. **The host must be a `module` or a model** (`Association::declare`, an admit list that fails
   closed). A model is what `models_of` climbs to `ActiveRecord::Base`, or what a macro made a
   collection element (`modelled`). Serializers are declined without being named.
2. **Resolve the class with `compute_type`'s list**: innermost nesting first, bare name last
   (`generated::candidates`). A leading `::` gives one entry. `composed_of` is absolute, as in Rails.
3. **Read `class_name:`, `source_type:` and `source:`.** `polymorphic:` wins. If the class can't
   be named, declare the members as `untyped`, never nothing. Constructors are dropped;
   the `_ids` pair is kept.
4. **Nine names per association** (from `builder/`). `reload_`, `reset_`, `_changed?` and
   `_previously_changed?` are declined (almost unused).
   - The singular writer is nilable.
   - A collection writer is `untyped`.
   - `_ids` is `Array[untyped]`, and singularizes the association's own name.
   - **A `belongs_to` or `has_one` reader is always `T?`** (2026-09-30, reversing an
     earlier ruling): `optional:`, `required:` and `belongs_to_required_by_default` are a validation
     run on save, and `Comment.new.post` is `nil` whatever they say, as is an orphaned key where no
     foreign key constrains it. Roundhouse makes the same call. A `null: false` column stays `T`:
     a loaded row cannot hold `NULL` there, and a `select` leaving it out raises.
   - **A `has_many` whose class cannot be read** is `ActiveRecord::Associations::CollectionProxy`
     where the bundle declares it (`framework_classes`).
   - **A `scope` is `X::Relation | generated::SCOPED`**: the lambda's value where truthy, else the
     relation (`instance_exec(…) || self`), read by the types table at the member's place
     (`types.md`).
5. **Only statements of a class body are macros**, plus the hosts `included do` (modules) and
   `with_options` (keywords merged innermost-first, so the call's own keyword wins).

- **`Comment::Relation` is monomorphic and nested under the model.** A project's own
  `Comment::Relation` makes this emit nothing for that element. One relation class per model.
- **Every `X::Relation < ActiveRecordRelation`** (`rails::RELATION_BASE`). The base is written
  once, holds `include Enumerable`, and is withdrawn if the project declares that name.
- **The query interface is written once, with `Return::Element` / `Return::Collection`**
  (`types.md`):
  - All of `QUERYING_METHODS` plus `Persistence::ClassMethods`, `with`, and `Scoping`'s `all` and
    `unscoped` (`Side::Both`: a relation answers both too).
  - Each row has a side: `Both`, `Relation` (`size`, `length`, `empty?`, `to_a`, `each`, `new`),
    class only (`instantiate`), or the model's own answer beside a relation's (`Model`: `count`,
    `average`, `sum`).
  - A name may refuse a *type*, never exist on the wrong side.
- **The class side goes on the model's base** (`synthesize::base_of`), or on `ActiveRecord::Base`
  only when the bundle declares it. Abstract classes keep it.
- **Overloads render on one line** (`Declared::overloads`). An overloaded member has no
  `Facts::returns`. Never let a `(*untyped)` arm claim arity 0.
- **A bare `where` is `ActiveRecord::QueryMethods::WhereChain[<relation>]`**, and a
  keyword hash or a positional reaches the relation arm: Rails writes `where(*args)`, so there is
  no `**` arm, which a bare call would also reach.
  - `relations::where_chain` opens Rails' class with a type parameter (`Facts::generic`), and its
    `not`, `missing` and `associated` are `-> R`: the relation it was made from.
  - Only where the bundle declares the class (`framework_classes`); otherwise the bare arm is
    `untyped` and answers nothing.
- **`pick` is the one interface member a relation class holds itself** (`relations::pick`):
  an arm per column the schema types (`Schema::picked`), each `T?` (no row is `nil`), then
  `(*untyped) -> untyped` for a string, `Arel.sql`, a joined column or several columns. The call's
  symbol picks the arm (`types.md`). No arm for a column an `enum`, an `attribute` with any
  written type, `serialize` or `store` re-types (`Model::recast_columns`), and none on any class
  for a name a module's macros re-type: its includers are not all known. The class side is
  `FORWARDED["all", "pick"]`, so a model's own `def self.pick` still answers first.
  An STI subclass's relation gets none.
- **`pluck` and `ids` by column** (`Picked::columns` and `Picked::keys`): `pluck(:col)`
  is `Array[T]` with the column's own `?` (stored rows keep `null: false`), the same withdrawals as
  `pick`, then `(*untyped) -> Array[untyped]`. `ids` is `Array[<primary key>]` where
  `create_table` names one key column (a SQL dump's is not read) and no `primary_key=` call in the
  body (whatever its receiver) or `def self.primary_key` can move it: not on the class, a class above it, or a module it includes
  (`rekeyed`, from `Model::rekeyed`). The class side of both is `all`'s.
- **`group` hands back `X::Grouped < X::Relation`** (`relations::grouped`, `generated::grouped_of`),
  whose `count`, `sum`, `average`, `minimum` and `maximum` are a `Hash` by group. Its
  chain methods keep it grouped (`types` answers `COLLECTION` on a grouped receiver with itself).
  The class side of `group` is `all`'s. A project's own `X::Grouped` withdraws it. The plain
  relation keeps its union: a scope or a parameter may hold a grouped relation nothing can see.
- **What `ActiveRecord::Delegation` hands the loaded records** (`relations::RECORDS`, 24 names:
  `reverse`, `sample`, `index`, `join`, `as_json`, `[]` and the rest, less `length` and `each`)
  are `generated::FORWARDED["to_a", name]` on the relation side: `Story.where(…).reverse`
  is `Array[Story]#reverse`'s answer.
- **`find`, `create`, `create!`, `build`, `new` have an arm per argument class**, and a
  relation's `count`, `average`, `sum` are a union with `Hash` (`Side::Model` keeps the model's
  own exact). `update`/`update!` are `update(id = :all, attributes)`: attributes alone are the
  records, an array of ids too, one id the record.
- **A relation class is also an association's `CollectionProxy`**, so the removals
  are split by side: a model's `delete` is a count, a relation's `Integer | Array[E] | nil`; a
  model's `destroy(id)` is `E | false | nil` (an array of ids `Array[E]`), a relation's the union
  of both; `destroy_all` is `Array[E]` on a model and `Array[E]?` on a relation (an empty
  association's is `nil`).
- **`each` is the records' `Array#each`** (`delegate … :each, to: :records`): `Array[E]` with a
  block, an `Enumerator` without. `unscoped { }` is the block's value, `each_with_object` its memo,
  the `async_*` names an `ActiveRecord::Promise` where the bundle declares it.
- **`minimum` and `maximum` are per column, like `pick`** (`relations::pick`), each arm the
  column's type or a `Hash` (grouped); the class side is `FORWARDED["all", name]`, so a model's own
  `def self.maximum` still answers first. A model with an `interval` column declines `sum` and
  `average` on both sides (an `untyped` row ranked `Source::Column`): Rails casts them through the
  column's type, a `Duration`.
- **A `scope` is two declarations** (singleton + relation, one span). Relation copies are held and
  sorted by owner.
- **A `scope` written again in one body replaces the earlier** (`Association::replaced_by`): Rails
  defines the method once more, so the last lambda is the one that runs and its call is the place.
  `Facts` keeps the first of two equal rows, so the earlier is dropped before declaring.

### Concerns

- **Instance macros go on the module**; the user's `include` carries them.
- **Class-side things (`scope`, `class_methods do`, hand-written `module ClassMethods`, and
  `included do extend M`) are fanned out onto each includer's singleton** (`Context::includers`,
  transitive). They live in the concern's document, with the span on the `def`
  (`syntax::def_span`, the whole `def … end`).
  - **An `attr_accessor`, `attr_reader` or `attr_writer` there is read too**, one member per side,
    placed on the call with the symbol as its name (`concerns::read`):
    `ActiveRecord::Inheritance::ClassMethods`' `attr_accessor :abstract_class` is what
    `self.abstract_class = true` calls. The same list feeds a migration's forwarded members.
  - The block speaks first.
  - Visibility is read from the file.
  - **The return is `generated::DEFINED[::Holder]`**, `Holder` being the module rubydex
    files the `def` under: the concern for `class_methods do`, `Concern::ClassMethods` for the
    hand-written module, `M` for `included do extend M`. The types table reads that `def`'s body
    with the includer's class object as `self`. A row a generator typed (`framework::members`)
    still answers alone: the sentinel is no vote.
- **An `extend` can't be declared.** rubydex never linearizes a late mixin (`navigation.md`).
- **`included do extend M` is finished against the graph** (`Analysis::concern_declarations`), keyed
  by the module's own file.
- **`rails::CONCERNS` is the one list a gem's `lib/` joins** (`Contribution::foreign`).
- **An `enum` in a concern is declined.** A concern nobody includes declares nothing. A concern's
  `attribute` withdraws no column.

### Everything else on a model

| Macro | Rule |
|---|---|
| `enum` | 3 + 4N names. Rank above the column, and the schema withdraws that column. Type `String?`. Both spellings. An unreadable `prefix:`/`suffix:` takes every value; `scopes:`/`instance_methods:` read as `false`. An unspellable label is declined |
| `attribute` | Declares a type only when the 2nd positional is one of `attributes::CAST_TYPES`, Rails' cast-type registries, not the schema's words (serializers can't produce that). `:datetime` is `ActiveSupport::TimeWithZone` only on a model in a project that keeps the time-zone default, else `ActiveSupport::TimeWithZone | Time`. `:time` and `:json` go to the host test, being plausible list-form names. With no cast type it goes to the host test. Always optional. The column wins across documents, unless the call wrote a type at all: `:money` or `Money::Type.new` withdraws the column, as Rails replaces its type |
| `mattr_accessor` / `cattr_accessor`, and their `thread_` spellings | The module object's reader returns `generated::WRITTEN` (what its writer is given) when the host is a `module` no class `include`s (`Host::sealed`) and the call has no `default:` or block (`Tail::stored`); every other accessor stays `untyped`. A plain one keeps a class variable, so the reader refuses where one of its name is spelled anywhere or a `class_variable_set` can name it (`types::class_variable_written`). `extend`/`prepend` are not seen |
| `attribute` on an `ActiveSupport::CurrentAttributes` class | `current::declare`, for each class whose superclass chain reaches it (`knowledge::rails::inheriting`): a reader and a writer per name on the class and on its instance, placed at the name. The reader returns `generated::HELD`, beside a literal `default:`'s type (`String \| HELD`); any other default is `untyped`. A name the body `def`s is left to it |
| `delegate` | Always declares the member. Typed here where both hops are facts; otherwise `generated::FORWARDED["to", "name"]` for a method or constant `to:`, the two calls the types table makes at the call. `to:` an ivar or a non-literal is `untyped`. The prefix must be a plain identifier. In a concern it goes on the module |
| Long tail (`LONG_TAIL`, 29 names / 17 families) | Affix shapes. `Installs::Nothing` and `Installs::Elsewhere` (`helper_method`) are in the table on purpose. `serialize` withdraws the column. Attachments are gated on `framework_classes`. `instance_*: false` only as a literal |
| Callbacks | Rails' 23 names, unmapped, kept on abstract classes. The block is handed the record, and it and the `if:`/`unless:` lambdas run against it (`[self: instance]`) |
| Mailers / jobs | Gate on superclass or include, never on `def perform`. Suffix list plus exact `ActionMailer::Base`. `ActionMailer::MessageDelivery` is written once and unmapped. `perform_later` is `instance \| false` (`enqueue`'s value). A class's own `perform_async` wins. `rails::convention_of` is shared. The types table reads these rows' calls as `perform`'s and an action's callers (`rails::run_from_the_class`; Sidekiq's are not read) |
| Route helpers | See below |

- **`MACROS` and `ASSOCIATIONS` are one list written twice**
  (`every_macro_is_read_by_exactly_one_reader`).

## Tables (`db/*schema.rb`, `db/*structure.sql`)

1. **Class → table, never table → class.** Only top-level own classes claim tables, unless they are
   namespaced models with a readable prefix (`compute_table_name` copied: innermost-parent affixes,
   `isolate_namespace` via `rails::engine_prefix`).
2. **Several claimants survive only when they all demodulize to the same name.** Dedupe claimants
   per class. New claimants must be models (`is_model`, a chain with `seen`).
3. **`self.table_name =`** (string or symbol, `symbol_or_string`) replaces the inflected claim,
   unless the inflected class is a model.
4. **A table two schemas (or schema sources) declare is declared by neither.** Parse every schema
   before writing.
5. **`rails::is_schema`: `schema.rb` or `*_schema.rb`, directly under `db/`.** The file name is a
   parameter.

- **Types:** `COLUMN_TYPES` (25 rows), checked against ActiveRecord's type maps in Rails 7.2, 8.0,
  8.1 and main. Everything else is `untyped` (never `untyped?`). `null: false` drops the `?`.
  `array: true` wraps in `Array[…]`. The primary key is read from `create_table`.
  - **`datetime` → `ActiveSupport::TimeWithZone`**, the railtie's default. A project file writing one
    of `rails::TIME_ZONE_SETTINGS` (the `rails.zones` list, membership only) makes it `untyped`.
  - **A `decimal` with a precision and no scale is `Integer`** (`DecimalWithoutScale`; `whole`):
    `precision:` without `scale:` in `schema.rb`, `numeric(p)`/`decimal(p,0)` in SQL.
  - **`serial` and `bigserial` are `Integer`**: PostgreSQL's dumper spells an `integer`/`bigint`
    with a sequence that way (`id: :serial`).
  - **`time`, `timestamp`, `timestamptz`, and a `datetime` a setting moved, are
    `ActiveSupport::TimeWithZone | Time`** (`EITHER_TIME`): whether each is converted
    rests on a version or setting not read, and either class is right. A nullable union is written
    `… | nil`. PostgreSQL's `timetz` has no ActiveRecord type: `String`. `interval` is a
    `Duration`, an `enum` a `String`, ranges `Range[untyped]`, `point` an `ActiveRecord::Point`,
    the other geometric types `String`, a `virtual` column its `type:`.
  - **Left `untyped` on purpose:** `json`/`jsonb`.
  - **Attribute methods** (`Schema::attribute_methods`): `x?`, `x_changed?` and
    `saved_change_to_x?` are `bool`, `x_was` is the column's type or `nil`, and the writer `x=`
    is `(untyped value) -> untyped` (2026-09-29: `self.user_id = nil` had no card while the reader
    had one). Five of Rails' fifteen, the ones the corpora call, since each is a completion line
    per column. A name the model's own file `def`s is left to it; a re-typed column keeps the
    predicates only.
- **The provenance comment carries file, table, column, type and nullability**, in the generated
  document (`workspace/textDocumentContent`). No card shows it (decided 2026-09-29): the column's
  type is the card's `-> T`.
- **The SQL reader is a scanner, never a parser** (`Scan::hidden` for quotes, dollar-tags and
  comments).
  - It feeds `schema.rs`'s `Table`/`Column` (`Schema::from_tables`).
  - Its 49-row word map ends at `schema.rb` words. `timestamp with time zone` is `timestamptz`,
    what Rails dumps, not `datetime`.
  - `KEY` depends on the dialect. A quoted name is always a column. The three dialects render the
    same RBS.
- **`structure.sql` plumbing:**
  - `capabilities::watched_files` registers it (constant, not indexed).
  - `Analysis::refresh` has a third outcome, "neither".
  - `Context::dumps` is one `read_dir` of `<root>/db`, counted by `is_empty`.
  - `Workspace::admits` is the gate.

## Structs and annotations

- **`Struct.new` / `Data.define`** (`analysis/structs.rs`):
  - Declares for `CONST = …` and `class X < Struct.new(…)`. `Foo::Point = …` is declined.
  - Symbol literals only, and `keyword_init:` is skipped.
  - A `def` in the block wins and is not declared.
  - `members` goes on both sides. `Struct#each` is two arms: the struct with a block, an
    `Enumerator` without.
  - Fixed half `Source::Interface`, per-name half `Source::Struct`.
- **`sig` / YARD:**
  - *Derived*, never a place. Sorbet wins over YARD.
  - **Every `@return` tag above a `def` is one list**: YARD writes one tag per return
    case, so `[String]` beside `[NilClass]` is `String?` and `[TrueClass]` beside `[FalseClass]` is
    `bool`. `NilClass` is `nil`. A tag with no type, or two classes, refuses.
  - Render every parameter shape; `(...)` → `(*untyped)`. Drop positional names; keep keyword
    names.
  - Decline any type it can't spell exactly.

## Route helpers

1. **The reader is checked against the real `RouteSet`.** It must invent nothing.
2. **`name_for_action` is copied exactly**, including scope levels, `namespace` = `nested { super }`,
   and `as:` replacing the resource word.
3. **Descend through `if`/`unless` wrappers** (`end if ActiveStorage.draw_routes`), both arms, a
   literal array's block once, and any unknown call's block.
4. **`draw :admin` files are read at their drawer's prefix into their own document.** Each helper is
   declared by exactly one file, first in URI order, with the workspace before gems.
5. **`RouteHelpers` is a top-level module, one `include` per controller** (hosts are own
   controllers, mailers and helpers). A project's own `RouteHelpers` wins. `mount` and
   `devise_for` are declined.
6. **A mounted engine's helper is a proxy**. The `rails.engines` list reads each own
   engine's `engine_name` or `isolate_namespace` (the last in the class, `read_engines`), and the
   name becomes a `RouteHelpers` method returning `ActionDispatch::Routing::RoutesProxy`, placed
   at that call. `main_app` is always one. `RoutesProxy` includes `RouteHelpers`, since own routes
   are read receiver-blind. All of it only where the bundle declares `RoutesProxy`.

## Blocks that run against something else

`workspace/rails/blocks.rs` writes the `[self: T]` a signature would say for every Rails method
whose block or lambda runs under `instance_exec` (`types.md`, "Blocks a signature rebinds"):

- **Gem `ClassMethods` copies** (`blocks::class_method`, read by `concerns::declare`): `scope`'s
  lambda against the relation (`COLLECTION`); `validate`, `validates`, `validates_with`,
  `rescue_from`, the job and mailer callbacks, `after_discard`, `queue_as`,
  `queue_with_priority`, `content_security_policy`, `permissions_policy`, `rate_limit` and
  `initializer` against the object (`instance`). Every other copied member keeps the parameters its
  `def` implies.
- **The model callbacks** (`relations::callbacks`) run their block and their `if:`/`unless:`
  lambdas against the record.
- **Framework rows** (`framework.rs`' `BLOCKS`, `CALLBACK_HOSTS`): `configure` (instance and class
  side), `Rails::Engine#routes` and `RouteSet#draw`/`prepend`/`append` against a
  `Routing::Mapper`, a mailer's `default` lambdas, and the nine controller callbacks
  `define_method` makes, declared on `ActionController::Base`, `ActionController::API` and
  `ActionMailer::Base`.
- **Not in it:** `retry_on` and `discard_on` (`yield self, error` keeps the class body's `self`),
  `included do` (the includer is not one class), `on_load` (the hook's symbol decides),
  `with_options` (its merger forwards to the class, which the body's `self` already answers).

## The connection adapter

`workspace/rails/adapters.rs`. `ActiveRecord::Base.connection` and its kin are the adapter class
the application connects through, which three things decide:

- **Rails' registry**: its four registrations (`BUILT_IN`, each loadable where its driver gem's
  constant is declared: `PG`, `Mysql2`, `Trilogy`, `SQLite3`), and every
  `ActiveRecord::ConnectionAdapters.register("name", "Class")` in a gem's `*adapter.rb` (the
  `rails.adapters` list, `read_registered`), with the `class X < Y` lines beside it.
- **`config/database.yml`** (`read_database_config`, a scanner: keys by indent, anchors, merges,
  aliases; ERB is unknown; a `url:` wins over an `adapter:`). Discovered and watched like a schema
  dump, and never indexed. Each environment's primary database is the application's connection.
- **A model's own `connects_to` / `establish_connection`** (`read_connection`): the databases it
  names, looked up by name in the file; under an `if`/`unless` the application's joins. Rows on the
  model's class object, placed at the call; a class's own `def self.connection` wins.

`Resolver`: named adapters are their class, or the nearest common ancestor of several; an unknown
one is whatever the bundle loads; a name nothing registers (makara's, through Rails 7.2's legacy
`<name>_connection`) answers nothing; an engine or gem (no `config/application.rb`) is
`AbstractAdapter`. Rows: `connection`, `connection_pool` (`ConnectionPool[A]`, a type parameter
ya-lsp opens Rails' pool with), and from Rails 7.2 (`LEASING`, `ConnectionPool::LeaseRegistry`)
`lease_connection` and `with_connection`, whose block is handed the adapter. **The pool's own
`connection` is declared only before 7.2** (`pool_rows`): 7.2 deprecates it and 8.0 removes it,
while `ActiveRecord::Base.connection` stays in every version.

- **`DatabaseStatements`' ten `delegate …, to: :transaction_manager` members are declared**
  (`transaction_rows`, from 7.2), each returning `generated::FORWARDED["transaction_manager", name]`:
  once the connection was typed, `connection.open_transactions` read as *no member*.
- **A driver is "bundled" where its top-level constant is declared**, which a gem's stub also
  satisfies: rpush writes `module Mysql2` and `module SQLite3` to name their error classes. That
  only widens an adapter the file cannot say (an ERB `url:` is `AbstractAdapter`, not
  `PostgreSQLAdapter`); it never narrows one. Reading `Gemfile.lock` would be exact.

Not read, and knowingly: `DATABASE_URL` and `<NAME>_DATABASE_URL`, which override the file at run
time; an `ActiveRecord::Base.establish_connection` a script or initializer makes; a gem swapping
`ActiveRecord::Base.connection` for a proxy at load (ar-octopus, replica_pools), whose proxy
forwards every call to the adapter named.

## RSpec

`workspace/rspec.rs` reads, `knowledge/rspec.rs` orchestrates, gated on `rspec.enabled` (`auto`:
the lockfile locks `rspec-core`).

- **Each `describe`/`context` is a class** under `RSpec::ExampleGroups::<the file's path>`,
  nested as the groups are (`base_name` is RSpec's spelling; siblings suffixed `_2`), inheriting
  its parent's or `RSpec::Core::ExampleGroup`, and the `self` of its block (`Facts::runs`). Every
  group declares its class-side `described_class`, which is also what makes rubydex hold the class
  object: a class with nothing on its class side has none.
- **Only spec files the editor holds** (`Sources::held`; `Wants::buffers` marks them touched on
  `didOpen`/`didClose`, since the graph already holds the text). A group's classes are that file's
  alone. Eager generation cost the largest corpus's cold open +1.1 s for 25,753 groups. A `hover`,
  `definition` or `completion` right after `didOpen` settles first (`requests::reopens`): the old
  graph answered a `let` with every `def` of its name, which the head-to-head counted as 88 answers
  changing tier after an edit that did not touch them.
- **Opening or closing a spec re-runs RSpec alone** (`Analysis::reopened_only`): on the
  largest corpus 96 ms a spec open, was 294 ms. Sound because:
  - a buffer-reading module declares **apart** (`declare_all`): last, into its own map, reading
    nobody's facts and read by no other phase, so its output is a function of its own inputs;
  - the settle must be only reopenings (`Analysis::reopened`, emptied by any edit to that
    document), with no bulk index, the projection the one held, and every file a generator read
    unchanged on disk;
  - a source both kinds write (a support file's `Struct.new` beside its shared groups) gets the
    other modules' facts from the last whole pass (`Analysis::shared`), merged back in in the whole
    pass's order: nothing that pass read has moved. Only a source both write for the first time (a
    spec with a `Struct.new`, opened) takes the whole pass, which holds it from then on. Refusing
    every overlap instead turned reopening off for a whole project (one support file did it);
  - placing asks only the documents the reopening rewrote: nothing else moved in the graph.
- **`let`/`let!`/`subject`/`subject!` are members returning `generated::BLOCK`**, the last of a
  name in a group kept; a named `subject` also answers `subject`. The implicit `subject` is
  `described_class.new` (the module itself for a module) where a group names a constant and no
  `subject` is written above.
- **`it`, the hooks and `let`'s block** get `[self: instance]` from `dsl`'s rows on
  `RSpec::Core::ExampleGroup`'s class side; `around` is handed a `Procsy`; `before`'s parameter is
  untyped (`:context` hands the group instance). `RSpec.configure` hands a `Configuration`, whose
  hooks run on an `ExampleGroup`.
- **`config.include`/`extend`** (`read_configured`, every non-spec file naming `RSpec`): unfiltered
  includes go on `ExampleGroup`, unfiltered extends on each outermost group, filtered ones on each
  group whose metadata (its own over its parent's, `type:` inferred by rspec-rails' directory table
  where `infer_spec_type_from_file_location!` is called) matches one key. A filter only running Ruby
  can read drops the mixin. rspec-core's `RSpec::Matchers`/`RSpec::Mocks::ExampleMethods` (unless
  `expect_with`/`mock_with` names another), rspec-rails' per-type modules and Capybara's are the
  built-in rows.
- **A shared group is a module of its `let`s**; `include_context`/`include_examples` include it in
  the group, `it_behaves_like` in the group it makes (whose own block's `let`s win), and
  `config.include_context` by filter. Found innermost group first, then the file's top, then a
  support file's top (a name two support files define is neither's). A group's own `let` written
  before an `include_context` defining the name is left out: Ruby redefines it. **Inside a shared
  block `self` is `RSpec::Core::ExampleGroup`**: every includer descends from it, and what only an
  includer defines falls to a guess, never a wrong type.
- **The gems' run-time syntax** (`syntax`): `RSpec::Matchers#expect` (a `ValueExpectationTarget`,
  or a `BlockExpectationTarget` for a block), rspec-mocks' `receive`/`allow`/`*_any_instance_of`
  and friends on `ExampleMethods`, `Receive`'s 18 customizations (each the `Receive`; `to_s`
  included, since it is one), and the mock targets' `to`/`not_to`/`to_not` (`untyped`). All are
  written by `module_exec`/`class_exec`/`define_method`, so rubydex sees none.
- **A jump on one goes to the gem's own `def` where it writes one** (`rspec::written_in`, asked
  through `Knowledge::written_in` by `requests::written_places` when the member has no place): the
  `def expect` inside `Syntax.enable_expect`'s `module_exec`, which rubydex files under
  `RSpec::Expectations::Syntax`; rspec-mocks' `Syntax` the same way; `MessageExpectation`'s
  `def and_return` for a customization; `Hooks`, `MemoizedHelpers::ClassMethods`,
  `SharedExampleGroup` and `TestProf::LetItBe` for the DSL the group extends. A named table, not
  an ancestor walk: past a `define_method` on the class object (`describe`, `it`,
  `it_behaves_like`, the targets' `to`) the ancestors reach minitest's `Kernel#describe`. Those
  jump nowhere, where v0.6.0's name match listed playwright's `describe` and mail's `to`.
- **test-prof's `let_it_be`** (and `_with_reload`/`_with_refind`) is a `let` whose block runs on
  an instance, where the bundle declares `TestProf::LetItBe`. Untyped where it passes a modifier
  test-prof does not ship, or a file naming `TestProf` calls `register_modifier` (every
  `let_it_be` then: a default or metadata may apply it).
- **A `def` in a group's block is the group's** (`generated::OWN_DEF`, `types::own_defs`): rubydex
  files every spec's `def` of one name as one `Object` method, whose body read was the union of
  them all. The margin and the card on the `def` read the member instead
  (`types::own_def_member`). `def`s are declared before `let`s, so a `def` keeps a name both write,
  as Ruby's class method outranks the `let` module's. A shared group's `def self.x` is dropped.
- **A group written in a shared group** (`Group::shared`) is a class under the shared group's
  module that includes it, not under the includer: what only an includer writes answers nothing.
  No directory `type:` is inferred for it.
- **Not read:** `define_derived_metadata` blocks (fewer includes, never extra), `fab!` and other
  projects' own macros, and `expect_with`/`mock_with` other than RSpec's for the syntax rows.

## Factories

`workspace/factories.rs` reads (pure, floored at 100%), `knowledge/factories.rs` orchestrates,
gated on `types.factories`. Checked against every FactoryBot release, 4.8.2 to 6.6: the same
`extend Syntax::Default` → `Syntax::Methods` wiring, the same `define_method`-made strategies, and
the same class rule (6.x tries `safe_constantize` first, which names the same class for every string
read here). `factory_girl`, the name before 4.8.2, is not read. Definition files are the files
naming `FactoryBot`, a gem's `lib/`
included: a gem's factory answers only for a name no project file writes (a working project cannot
load both). **FactoryBot only**: Fabrication was built and removed on purpose (two of six corpora use
it); do not add it back.

- **Each strategy is one method with an arm per factory**,
  `(:user factory, *untyped args, **untyped kwargs) -> ::User`, then an `untyped` catch-all, and
  `types::pick_by_literal` picks the arm by the call's first argument. A list's
  `untyped amount` sits between, which the pick passes over: `untyped` takes any count. FactoryBot's nine strategies and `attributes_for`'s three are rows on
  `FactoryBot::Syntax::Methods` (rubydex sees none: `define_method`). A name that is no bare RBS
  symbol gets no arm.
- **The class is FactoryBot's own rule**: the nearest `class:` up the parent chain (a nested
  factory's or `parent:`), else the topmost factory's name. **A constant is looked up from where it is written; a String or Symbol from the top level**, as
  `constantize` does.
- **Every doubt answers nothing, never a guess:** a name two definitions write, a parent nothing
  defines, a `parent:` only Ruby knows, a loop, a `class:` only Ruby knows, and **any `initialize_with` that is not `new(…)`**
  in the factory, a trait, an ancestor, the global one, or a `FactoryBot.modify` (which may be
  anyone's). A `class:` can name a service whose `initialize_with` returns a model.
- **`FactoryBot.register_strategy` replacing a strategy** leaves it (and its `_list`/`_pair`)
  undeclared; a name not written as a literal is every strategy.
- **Each factory's blocks run where FactoryBot runs them** (`factories::proxies`, written beside
  the definition file, `Facts::whole`): the factory's own block, its `trait`s' and `transient`s'
  on a class made for it, `FactoryBot::Factories::<Name> < FactoryBot::DefinitionProxy`
  (`Runs::Instance`); an `after`/`before`/`callback` block on a `SyntaxRunner`, so `create(:x)`
  there is typed; an attribute's on the `Evaluator` (the proxy undefines all but a dozen
  methods, so any other name is an attribute); `sequence`, `initialize_with`, `to_create` and
  the rest refused. A name two definitions write, or a gem's the project writes too, is left as
  it was.
- **A callback's block is handed what was built** (`callback_row`, a row on the made class): the
  class of the factory and of every factory inheriting it, by nesting or `parent:`, since a
  child runs its parent's callbacks and traits. `after(:build|:create|:stub)` and
  `before(:create)` hand the object, `before(:build|:all)` `nil` (6.6; older releases never run
  them). The evaluator stays `untyped`: it is a subclass made per factory and per call, whose
  methods any attribute or override may replace. **No row** where a class in the union is not
  known, a factory only Ruby names (or a `parent:` only Ruby names) may inherit it, a block is
  handed to `send`, a strategy is registered (it may hand a callback anything), a callback's
  name is not a literal or hands something else (`after(:all)` hands a strategy's result), or
  its block takes more than two plain parameters (FactoryBot reads the arity).
- **The last statement of a callback block was never a used call**: the `after` call is a
  statement of the factory's block, which no one reads (`coverage::used_calls`).
- **Go-to-definition on `create(:user)` goes to the `factory :user` call** (an alias's to its
  factory): a strategy has no place of its own, so `requests::literal_places` asks every module
  `Knowledge::literal_place` for the call's first Symbol, and `Factories` answers from
  `Classes::places`.
- **Seeds:** `FactoryBot.create(:user)` is typed and jumps (`FactoryBot` extends
  `Syntax::Default`, which includes `Syntax::Methods`). A bare `create` after a top-level
  `include FactoryBot::Syntax::Methods` is only a name match: ya-lsp does not read a top-level
  `include` as mixing into `main`.

## Translations

`workspace/i18n.rs` reads (pure, floored at 100%) and holds i18n's words; `knowledge/i18n.rs` finds
the files and answers, gated on `i18n.enabled` (`auto`: the lockfile locks `i18n`). Its own table,
for `[rspec]`'s reason: i18n runs outside Rails.

- **The key table is the second output that is not RBS**, beside the view conventions: key → what
  it holds, its file and its span. The analysis asks it through `Knowledge::keyed_type`,
  `keyed_entry` and `keyed_under`. The only RBS is which members look a key up
  (`generated::KEYED`: `I18n::Base`'s `t`/`translate`/`t!`/`translate!`, and `t`/`translate` on
  `AbstractController::Translation` and `ActionView::Helpers::TranslationHelper`) and i18n's own
  returns: `I18n.locale` `Symbol`, `localize`/`l` `String` on all three (an arm with a required
  `default:` is `untyped`: a `nil` object answers its default), `with_locale` its block's value,
  `ActiveModel::Name#human` `String`.
- **One locale, the main one** (`[i18n] locale`, `en`), ruled 2026-09-28: what other locales hold
  is the project's business. A key the main locale lacks answers nothing on every surface.
- **i18n's load order, later wins, trees merge key by key** (`Translations::merge`): Rails' own
  `lib/<lib>/locale/*.yml` (`RAILS_OWN`, flat), every gem's `config/locales/**`, then the
  project's: every `config/locales` within three directories of the root (dot directories, test
  trees, `vendor`, `node_modules` skipped), the application's own last, or what `[i18n] paths`
  lists instead. A `.rb` file must be one `index.include` admits.
- **A file's locale is its top-level key**, read without parsing (`locales_in`; a `.rb`'s hash keys,
  `ruby_locales_in`), and only the main locale's files are parsed. The walk is memoised on the root,
  the gems, the locale and the paths, each file's locales on its stamp. A watched change under a
  `config/locales`, or to a file discovery listed, walks again (`Knowledge::touched`). The files
  ride the projection's `also_reads`, so the pass stamps them and a changed one is parsed again.
- **A scanner, not a YAML parser**, like `database.yml`'s: Psych's scalar rule (`scalar`: `yes`,
  `12`, `:sym`, `2024-01-01` are not strings), anchors, aliases, `<<:` merges, flow collections,
  block and quoted scalars. Checked against PyYAML over the six corpora's locale files (40,585 key
  kinds, 32,227 strings, no difference). What it cannot read is `Held::Other`, which every surface
  declines.
- **A call's type** (`Translations::returns`): a `String` is `String`; a plural subtree with
  `count:` is `String`; a subtree is `Hash[Symbol, untyped]`; a list `Array[String]` or
  `Array[untyped]`; an `_html` key through a view's or controller's `t` is
  `ActiveSupport::SafeBuffer`.
- **Declined:** a key that is no literal or starts with `.` (relative keys are not built: a view's
  `@virtual_path` is the layout's inside a `render layout:` block, and a controller's `action_name`
  is the running action, not the enclosing `def`); a `scope:` that is no literal; a `default:` that
  is not a `String`; a `locale:` other than the main one; `count:` on a subtree that is no plural;
  an `_html` subtree through the html-safe `t` (it becomes an `Array`); the view's `t` with a block
  (the block's value).
- **Not read:** `I18n.backend =`, `store_translations`, `I18n.load_path <<` in Ruby (rails-i18n),
  `config.i18n.load_path`, ViewComponent's sidecar files. A new file under a custom `paths` glob
  waits for the next walk: watchers are registered once, from constants.

## `config`

`framework.rs`' `CONFIG` and `NAMESPACES`: `config` is `Rails::Application::Configuration` (and the
engine's and railtie's, on both sides, the class side made by `delegate`), a framework's namespace
is the `ActiveSupport::OrderedOptions` its railtie assigns (22 names, each gated on its module being
in the bundle; lograge's is its own `Lograge::OrderedOptions`), and a few containers Rails fills
(`hosts`, `public_file_server`, `session_options`, `filter_parameters`, `x`, `paths`, `root`, the
load paths, `middleware`) are typed. A namespace's own options stay untyped.

**A setting an application assigns outright** (`config.dispatcher = Dispatcher.new`) is
kept by `Rails::Railtie::Configuration`'s `method_missing` in one class variable, which the
application's, every engine's and every railtie's configuration share. `framework::read_config_writes`
reads each `name =` on a configuration spelled as Rails spells it (the `rails.configured` list, own
code and engines; not a namespace's name): `Rails.application.config`, `Shop::Application.config`
and `Rails.configuration` anywhere, and a bare `config` only in a class `< Rails::Application`,
`Engine` or `Railtie` or a `configure` block on `Rails.application` or an `…Application` constant,
never in a `def`. A gem's own `ShopPromotions.config.x = y` and a spec's `let(:config)` are other
objects. `config_facts` declares `name` and `name=`
on `Rails::Railtie::Configuration`, placed at the write, only where the bundle declares that class.
The reader returns `generated::SHARED` (`types.md`). A name Rails' own configuration declares is
found on that class first, so the row only answers where Rails has none. Not read: a `send` of the
writer, a gem's own railtie writing the name.

## What a migration sends to its connection

`workspace/rails/migrations.rs`. `ActiveRecord::Migration#method_missing` sends a migration's own
calls to the connection, which rubydex cannot follow.

- **The public `def`s of `SchemaStatements`, `DatabaseStatements` and `Quoting`** are declared on
  `ActiveRecord::Migration`, with the `def`'s own parameters, placed at that `def`. They are
  **read out of the bundle's files** (`concerns::installed`), never listed here, because the list
  moves with the Rails version. `initialize` is skipped (Ruby makes it private).
- **`create_table`, `create_join_table`, `drop_table` hand their block a `TableDefinition`,
  `change_table` a `Table`** (`YIELDS`), only where the bundle declares that class.
- **Each symbol of a `define_column_methods` call** in the three `ColumnMethods` modules is a
  member of the module, placed at the symbol, returning `Array[untyped]` (the names it was
  given). Both shapes: the module body (8.1) and `included do`
  (8.0 and before, which writes them on each includer; the same receivers).
- **The modules are found by the framework rows' `declares` call**, one walk of the definitions for
  both, and the rows are hosted on the file each module is read from. Gated like the framework
  rows, on `rails.enabled`.
- **Not forwarded:** `AbstractAdapter`'s own methods (it overrides `Object#inspect`, which a
  migration really calls), an adapter's own modules (which adapter runs is in
  `config/database.yml`), the class side (`def self.up`), and gems that `prepend` onto
  `ActiveRecord::Migration` at load time (`strong_migrations`' `safety_assured`).

## Ruby's `Singleton`

`workspace/singletons.rs` reads (pure, floored at 100%), `knowledge/singletons.rs` orchestrates,
ungated: it is Ruby's library, like the inflector. `include Singleton` runs `Singleton.included`,
which extends the class with `SingletonClassMethods`; rubydex sees the `include` and no `instance`,
and Ruby's RBS leaves the module empty.

- **`def self.instance: () -> ::Class` on each class whose body writes `include Singleton`** (or
  `::Singleton`), placed at the `include`. A class's statement only: not in a `def`, and not in a
  `module` (its includers are not known).
- **Not where the project declares a `Singleton` nearer the class** (`candidates` against
  `Context::classes`): `include Singleton` inside `module Jobs` with a `Jobs::Singleton` of its own
  is that module.
- Own code and engines (`Wants::engines`), where a file names the constant.

## Ruby's `define_method`

`workspace/defines.rs` reads (pure, floored at 100%), `knowledge/defines.rs` orchestrates, ungated
like `Singleton`: it is Ruby itself. rubydex reads `def` and `attr_*`, not a call that makes a
method, so a method `define_method` made had no declaration, type or place.

- **A statement of a class, module or `class << self` body, with no receiver**, is read. In a `def`
  or a block (`included do`, a loop over names) it declares nothing: only running Ruby knows when it
  runs or what the names are. `private define_method(…)` is the statement's own.
- **The name is a symbol literal RBS can spell**, else the call declines alone.
- **Which side:** `define_method` in a class body is an instance method, in a module body the
  module's (its includers'), in `class << self` the class object's; `define_singleton_method` in a
  body is the class object's. One in `class << self` names no class and declines.
- **The block is the body:** the member returns `generated::BLOCK`, placed at the call (the whole
  call and the name in its symbol), so the types table reads that block. Its parameters are the
  member's, every one `untyped`; `_1` and `it` are required positionals. A proc or method handed
  instead is `(*untyped, **untyped) -> untyped`: declared, not typed.
- **Visibility is Ruby's**, which `define_method` follows (checked on Ruby 4.0): a `private` or
  `protected` section, `private define_method(…)`, or `private :x` anywhere in the body writes
  `private def`. `protected` is written private.
- **A `def` of the name in the same body keeps it**, on its own side: the `def` has a body rubydex
  reads, and which of the two Ruby keeps depends on an order this does not follow.
- **`Source::Defined`, ranked beside a struct's member:** Ruby keeps the class's own method over one
  a column or a macro installs in an included module.

## Ruby's `include` and `prepend` from outside the class

`workspace/mixins.rs` reads (pure, floored at 100%), `knowledge/mixins.rs` orchestrates, ungated:
it is Ruby itself. rubydex reads an `include` written in a body; `Paperclip::Attachment.prepend(M)`
or a plugin's `Post.include(M)` is a method call to it, so the module never joined the class's
ancestors (its methods were name guesses, its instance variables had no writes).

- **Written as an `include` or `prepend` line on the class** (`Facts::mixin`, `Facts::prepend`),
  which rubydex links however late the generated document is indexed. Arguments last to first:
  `include(A, B)` puts `A` in front, as `include B; include A` does.
- **Only where the call runs when the file loads**: top level, a class or module body, or any
  block (`after_initialize do`, `config.to_prepare do`). Not in a `def`, a lambda, a loop or any
  conditional, modifier included. Every argument a constant, or `self` straight in a module body;
  one other argument declines the call.
- **Names are Ruby's lexical lookup** (`mixins::meanings`, `generated::candidates` over the
  nesting), answered by `Declaring::kinds`: the nearest declared meaning; a mixed-in name whose
  nearest meaning is a class declines (Ruby raises); the target's `module`/`class` keyword is the
  graph's; a namespace above the target declared nowhere declines.
- **Only files the application loads** (`Declaring::loaded`, `Fence::unloadable`'s reading): a
  spec's `Post.include(Helpers)` would put the suite's helpers in the application's ancestors,
  which `resolve` walks unfenced.
- **Listed by text, not by reference** (`Wants::spells`: `.include(`, `.include `, `.prepend(`,
  `.prepend `): rubydex records only the two constant references. The indexer's workers look for
  the texts while they hold each file (`indexer::spells`), and `Analysis::spelled` keeps the bits
  per document, rewritten wherever a text enters the graph. Scanning in the walk instead read every
  application file again: a third of a second on the largest corpus's cold open.
- **Not read:** `send(:include, M)`, `class_eval { include M }`, `ActiveSupport.on_load` blocks.

## A read off the request's `params`

`workspace/rails/request.rs` reads (pure, floored at 100%), `knowledge/rails.rs` orchestrates
(`request_gates`, `read_type`), gated on `rails.enabled`; with `rails.routes` off nothing answers,
since what a route gives a key is then not read.

- **The one RBS is which members read a key** (`framework::READS`, `generated::READ_OFF`): `[]`,
  `dig`, `fetch` and `require` on `ActionController::Parameters`, and `expect`'s `(Symbol)` arm.
  Every other receiver than the controller's own `params` (`StrongParameters#params`, written
  bare) answers as before: a `Parameters` the code built holds whatever it was given.
- **The union** (`request::VALUE`): `String | Integer | Float | bool | Array[untyped] |
  ActionController::Parameters | ActionDispatch::Http::UploadedFile`, what Rack's, multipart's and
  the JSON parser hand a `Parameters`; `nil` for `[]`, one-key `fetch` and `dig`, none for one-key
  `require`. `expect(:id)` is the scalars (`SCALAR`). `fetch(key, {})` is `fetch(key)`: the
  default comes back an empty `Parameters`, which the union holds (`without_default`). Any other
  default, a block, `require` of a list and a dynamic `expect` are left to Rails' body.
- **What can change it, each read from files the application loads** (`Declaring::loaded`):
  - the code's writes (`read_request_writes`, list `rails.requests`, spelled `params` or
    `parameters`, own and engines): `params[:k] = v`, `||=`, `store`, a merge's pairs, a local
    or ivar holding `params`, `request.parameters` and its parts, `params.tap`/`each`'s block
    parameters. A value the text names (a literal, a branch of each, a value off the params, a
    conversion like `to_s`) joins its class to the key; any other refuses the key; a key it cannot
    name joins every key, or refuses them all. **A write counts for reads under the class or module
    it is written in** (`self`'s ancestors, the body's name resolved as Ruby's lookup does,
    `resolved`); one outside any, for every read.
  - a callee handed the params that writes into its parameter, matched by method name (`new` is
    `initialize`), project-wide;
  - what a route gives a key (`read_route_values`): every `key: value` in a routes file, each file
    it draws, and every `routes.draw`/`append`/`prepend` block elsewhere (`rails.route_blocks`); a
    `Regexp` is a constraint, a non-literal refuses the key, a `**` or a non-hash `defaults` refuses
    them all;
  - a parser of the project's or a gem's own, or `parse_json_times` (`read_request_settings`, list
    `rails.parsers`, gems too): refuses every key, unless the parser is a lambda that only decodes
    JSON (`decodes_json`).
- **A key every route reaching the read gives is what those routes give it** (route proofs,
  `request::Asked::proven`): `String` for a required segment, a default's class otherwise (and
  `String` too where an optional segment writes the key, `(.:format)` always does), never `nil`
  unless a default is. The routes reaching the read come from two pure readers:
  - `targets.rs` (`read_targets`): every route's controller, action, path and defaults, from the
    project's own routes files and route blocks (`ROUTE_BLOCKS`) and the files a `draw` names,
    with Rails' scope rules (`namespace`, `scope`, `controller`, `with_options`, `defaults`,
    `concern`/`concerns`, resources with `only`/`except`/`param`/`path`/`module`/`to`/`shallow`,
    `member`/`collection`/`new`/`on:`, a loop over a literal array, engines under their mount
    prefix and `isolate_namespace` module). Anything else is `Unreadable`: bounded to the
    controller a `"c#a"` in its text names, a project macro's resource (`read_route_macros`, its
    `def` found through `Declaring::methods`), a mounted gem engine's namespace, or any controller.
    Checked route for route against the real routers of the six corpora (through the spike's
    prototype): none missed silently.
  - `actions.rs` (`Controllers::proven`): which `(class, action)` pairs a `def` runs under. A public
    `def` is an action of its class and every subclass not writing its own; a callback's
    `only:`/`except:`, or every routed action; a private helper its bare callers', to a fixpoint.
    It reads every controller file the application loads (`rails.controllers`) and every own
    module one includes, to a fixpoint. It refuses a module's `def`, a singleton `def`, a callback
    a module names, a non-literal `only:`/`except:`, a name a symbol hands `send`, `try`, `method`,
    `layout`, `respond_to?`, `with:`, `if:` or `unless:`, a `helper_method`, a caller it cannot
    place, and no route at all. A route that may reach any controller refuses every proof, unless
    it is a call no project `def` writes (a gem's routing macro, `devise_for`'s kin), which refuses
    only the controllers descending from a class the project does not write.
  - The read's class and `def` come from the types table (`Read::ancestors`, `Read::def`). What
    `read_route_values` reads is left out of a proven read: its proof read every reaching route's
    defaults.
- **A key read off what `permit` or `expect` hands back is what its filter lets through**
  (`framework::SHAPES`, `generated::SHAPED` on `permit` and `expect`'s `(**untyped)` arm;
  `knowledge::rails::shaped_type`; the pure half in `request.rs`). Checked against 7.2's and
  8.1's `strong_parameters.rb`.
  - **The call's own type, any receiver, from the filter alone** (`request::filters`,
    `expected`): `permit` is a `Parameters`; `expect` one key's value, required (`[]` an
    `Array`, `{}` a `Parameters`, `[[…]]` an `Array` or a `Parameters` of hashes by index, a
    filtered hash the `Parameters`), several keys' values an `Array`. A filter it cannot read
    (`permit(*FIELDS)`, `expect(post: fields)`, `expect(**opts)`) still answers: `permit` a
    `Parameters`, `expect` with keywords either; without keywords, and for one bare name, the
    arms (`READ_OFF`). Answering nothing there lost 92 used calls: the row hides Rails' body.
  - **A filter held in a constant is the list its one assignment writes** (`types::written_at`,
    `constant_names`), as an argument or a keyword's value: a literal of names frozen as written
    (`cursor::frozen_constants`), and no reference resolved to the constant writing it again with
    an operator (`KEYS += [...]`, which rubydex files as a reference). An unfrozen list changes
    under any `KEYS << :x`; what `.freeze` leaves open, a literal nested inside, only code reaching
    into the constant by index changes. A `Todo` rubydex invented over that one assignment is
    still the constant: rubydex takes a value written as a `.` call (`%i[a].freeze`) for a class
    it may build, and promotes the constant once a method is called on it anywhere.
  - **The reads, only off the controller's own `params` in own code**, read by literal keys
    (`read_path`: `[]`, `require`, one-key `fetch` with or without `{}` for a default, `dig`):
    each key's union is the request's
    there (`request_value`, `[]` at the top, `dig` below a key, with the writes `Requests` reads
    for `self`'s ancestors), kept by the filter (`let_through`): a scalar filter every class but an
    array and a hash, `[]` the array, `{}` the hash, `[[…]]` either, a filtered hash the hash, or
    under `permit` an array of them too. `[]` and `dig` add `nil`; `fetch` only for a scalar (JSON
    can send `null`); `require` none. A key named twice is each. A key whose writes leave it open
    is left out.
  - **A hash or an array literal written under a key holds values the text writes** (`holds_values`,
    any branch), so it is a nested write: a read through several keys, `dig`'s and a permitted
    value's below that key, refuses. Before, `params[:post] = { title: date }` read as the union.
  - Not read: a splatted filter (`permit(*FIELDS)`: `cursor` empties a splatted call's
    arguments), a filter a method builds, a receiver held in a local or memoized in an instance
    variable (one object every reader of both shares, a `before_action` and a view included), a
    `slice` or any other link, nested hashes' own keys (`permit` reads `address: [:street]` under a
    hash of hashes by index too), and permits off anything but the request's `params`: a
    `Parameters` the code built lets through whatever answers `is_a?` for a permitted scalar class,
    and `TimeWithZone` and `Duration` answer it for `Time` and `Numeric` without being either.
- **Not read:** a middleware writing the Rack env; a gem that is not an engine (actionpack's own
  `ParamsWrapper` nests the request's own values under a key, inside the union); a callee that
  writes into what it was handed under another method's name.

## Framework singletons and the controller context

| Chain | Return | Status |
|---|---|---|
| `Time.zone` | `ActiveSupport::TimeZone` | shipped |
| `Rails.root` | `Pathname` | shipped |
| `Rails.cache` | `ActiveSupport::Cache::Store` | shipped |
| `Rails.application` | the project's own `< Rails::Application` class | shipped |
| `Rails.logger` | `ActiveSupport::BroadcastLogger \| WrittenByItsWriter` | shipped: Rails' `initialize_logger` wraps every logger in one; what the application assigns after joins (`framework::ASSIGNED`, `types::written_beside`). Its `class_eval`'d `info`, `warn`, `error` and the rest are `framework::LOGGER_METHODS` |
| `Rails.env`'s predicates, `TimeWithZone`, `Time.zone`'s builders, `Time.current`, a model's concern class methods | `framework::members` | shipped: `class_eval`, `method_missing` and `delegate`-made members are declared with the macro named |
| controller `helpers`, both sides | `HelperProxy` | shipped: a generated `class HelperProxy < ActionView::Base` including every helper module the application writes (`framework::HELPER_PROXY`), hosted with `ActionView::Base`. Return-only rows on `ActionController::Helpers#helpers` and `ActionController::Base.helpers`, so the jump stays Rails'. `ActionView::Base` where the application writes no helper or declares the name. Over-inclusive for `ActionController::Base.helpers` itself and `include_all_helpers = false`: those calls raise in Ruby. `helper_method` and a gem's `helper` are not in it |
| mailer `with` | — | `with` itself answers `ActionMailer::Parameterized::Mailer` from its concern's body. The call after it is the mailer's class's (`rails::passes_to_the_class`, `types.md`), and a mailer's `params[:key]` is what each `with(key: …)` passed (`rails::keyed_by_a_class_call`). A job's `set` hands on the same way |
| `Rails.configuration` | `Rails::Application::Configuration` | read out of its body (`application.config`) since `config` is typed |
| ActiveSupport's `try`, `try!` | `generated::SENT` | shipped: return-only rows on `ActiveSupport::Tryable`, which `Object` and `Delegator` include, so the types table makes the call the first argument names (`types.md`). `NilClass`'s own `try` answers `nil` from its body. The same in 7.2, 8.0 and 8.1 |
| a mailbox's `mail`, `inbound_email` | `Mail::Message`, `ActionMailbox::InboundEmail` | shipped: `ActionMailbox::Base`'s `delegate` and `attr_reader` (`framework::FORWARDED`), the same in 7.2 to 8.1; only `receive(inbound_email)` builds a mailbox |

**Methods whose `def` the bundle holds and whose body cannot be read to a type**
(`framework::RETURNS`, `SAVES`, `ARMS`), each checked in 7.2, 8.0 and 8.1: a record's persistence
(`save` `bool?` and `save!` `true?` on each of the four modules a model's lookup may reach first,
`update`, `update_column(s)`, the predicates, `destroy!`, `attributes`), `errors.full_messages`,
`redirect_to` (`Integer`, on both modules), `credentials`, a mailer's `mail`, `MessageDelivery`'s
deliveries, the cache store's `write`/`delete`/`exist?`/`fetch_multi`, `request.env`/`host`/
`format`, `flash.now`, `strip_tags`, `Duration#to_i`, `exec_query`, a record's `changes`
(`HashWithIndifferentAccess`) and `previous_changes`/`saved_changes` (that, or before any save an
empty `Hash`). **Where the call decides**, an
overload set: `Duration#since`/`ago` and aliases (by the time's class), `cache.fetch` (the block's
`[T]`; a `raw:` arm joins `untyped`), `select_all` (an `async:` arm), `Arel.sql`, and
`params.expect` (keywords: `Parameters | Array[untyped]`), which only exists where the bundle
declares `ActionController::ExpectedParameterMissing` (8.0). `configure` is its block's value,
`draw` is `nil`.

**ActiveSupport's methods on Ruby's own classes** (`framework::CORE_EXTENSIONS`): `String#blank?`
`bool`, `String#parameterize` `String`, `Array.wrap` `Array[untyped]`. Written only where the bundle
declares `ActiveSupport`, since Ruby's classes are declared everywhere. **Never a row on `Object`,
`BasicObject` or `Kernel`** (`Object#blank?` is left out): once such a generated document is edited
or leaves the graph, rubydex's next resolve records `Kernel`'s reference a second time. A debug
build, the test suite included, panics there and the seam re-indexes everything; a release build
keeps the duplicate.

**`ActionController::API`'s seventeen modules** (`framework::API_MODULES`) are written as `include`
lines on it: Rails includes them in a loop over `MODULES`, which rubydex does not read, so an API
controller's `params` was `Metal`'s. In Rails' order, the same in 7.2, 8.0 and 8.1, and only where
the bundle declares every one. A table, not a reader of the loop: only actionpack writes that loop
over a constant, and listing the gem files that might would read thousands for one class.

`CONTEXT` is the same table for instance methods: what a controller calls on itself, and a template
on its view context (`ActionView::Helpers::ControllerHelper`).

| Method | Return | Made by |
|---|---|---|
| `params` (controller) | `ActionController::Parameters` | `def` |
| `request` | `ActionDispatch::Request`; in a template `ActionDispatch::Request?` (a mailer has none) | `attr_internal` |
| `response` | `ActionDispatch::Response` | `attr_internal_reader` / `delegate` |
| `session` | `ActionDispatch::Request::Session` | `delegate` |
| `flash` | `ActionDispatch::Flash::FlashHash` | `delegate` |
| `cookies` | `ActionDispatch::Cookies::CookieJar`; a controller's is `private def` | a private `def` / `delegate` |

Declined: a template's `params` (a mailer's is a `Hash`), and a controller's `logger`: the
railtie sets it to `Rails.logger`, but an application's `config.action_controller.logger` replaces
it through configuration, which `Rails.logger`'s rule (a call of the writer) does not read.

- **A private `def` gets a `private def` row** (`Declared::private`). rubydex reads a method's
  visibility from one of its definitions, and which one depends on resolution order; a public
  signature could open the method to every receiver in some order.

- **The bar is "does the named class hold the members the next call asks for"**, not "is the return
  knowable". Measured for `CONTEXT` over every call on the six in six corpora: 5 of 2,316 became
  *no member* (`request.origin`, `ips`, `cookie_jar`).
- **Declare only the return, unmapped**, where rubydex holds the method. **The exception is a
  method Rails makes with a macro in a gem** (`attr_internal`, `delegate`): rubydex holds nothing,
  so the row declares the member, with no place (`at: None`). Its comment says which macro.
- **`session` is the application's class**, not a controller test's (decided 2026-09-24). Its
  comment says the harness stores another.
- **Check both ends and every namespace above an owner against the graph** (`framework_constants`,
  `Namespaces::spellable`). Read the owner's keyword from `Namespaces::opens`.
- **Each owner's rows are hosted on the file declaring that owner**, the component's own
  (`Synthesize::declaring_documents`, classes and modules). So the rows depend on the component
  being indexed, never on the project being an application: an engine or a gem monorepo gets them.
  `config/application.rb` is only read for `Rails.application`'s class. Gated by `rails.enabled`
  alone, asked in `framework_declarations` itself.

## Keeping the pass cheap

`settle` runs the pass before every resolve. Gates, in order:

1. **Per-document gate** (`context_would_be_the_same`): one document's `Contribution` against its
   held one. It runs before the walk. The held map is also the walk's memo.
   - It is keyed on "rubydex re-indexed this document", never on a file stamp.
   - Only `index_buffer` names a document. **Every bulk route sets `touched_all`**, including
     `index_workspace`.
   - Absent means *not visited*, never *contributed nothing*.
2. **Whole-`Context` comparison**, which is strictly wider. When it fires, keep the fresh
   contributions. The walk absorbs each held contribution **by reference**
   (`Context::absorb`), copying a name only where the set lacks it.
3. **`generators_would_repeat_themselves`**: the files, checked by `stat` (never trust a
   notification), and a touched document a generator read through `Declaring::text` in the last
   pass though no list names it (`Analysis::declared_from`: a routes file's helper, a file a `draw`
   names, a module a controller or model includes). Without it an edit to one of those alone ran
   no pass, and route proofs stayed stale until some other edit did.
4. **`record`'s text test**: re-index only documents whose RBS changed.

- **The parse memo is keyed on text and on which readers ran.** `rails::MODELS` gains members after
  the walk. `Fresh::Disk` is `stamp_of`. Buffers are hashed. `rails::read_routes` is not memoised.
  - **Route proofs hold what they read from files a routes file reaches:** a project macro's
    `read_route_macros` by its file's text hash (`Rails::macros`), and which own documents write
    each bare call a routes file makes by each document's content hash (`Analysis::defining_documents`,
    `synthesize::Defining`; other names or another `is_own` layout start over). Re-reading both on
    every pass was 50 of the 66 ms the largest corpus spent proving routes after an edit.
  - `defines` holds its parse (`read_defines`) and filters it by `spellable` at declare.
  - `structs` holds **facts**, because its reader asks `spellable` mid-parse: each file's facts are
    kept with every name it asked and the answer (`structs::read_asking`), and read again when
    the text or any answer moved. Re-reading both lists each settle was a fifth of the largest
    corpus's pass.
- **One `stat` per file per settle.** `remember` keeps the stamp the memo took (`freshness`), and
  stamps only `Context::read_from_disk`: a `buffers` row's documents are read from the buffer or
  not at all, so a closed spec file is never stamped. A file on two lists is stamped once.
- **`declares` is one walk of the definitions per `declaring`** (`Analysis::declarers`, by last
  segment), built on the first question, however many modules ask.
- **Two gates, two questions. Never share a clause.** Instruments: `Analysis::walks`, `passes`,
  `Synthesized::indexed`.
- **Gem discovery drops the held map** (`engine_prefixes` changes).
- **Cost model:** re-indexing generated RBS into a settled graph is ~32 ms per document plus
  ~0.7 ms per declaration. That cost is rubydex's `consume_document_changes`. There is no byte
  term: shrinking comments or concatenating documents buys nothing. That is why a document is one
  body.
