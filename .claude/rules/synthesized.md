---
paths:
  - "src/analysis/synthesized.rs"
  - "src/analysis/locator.rs"
  - "src/workspace/rails/**"
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
  - `RAILS_CLASS_SIDE` (three `ClassMethods` modules), then the relation, for the class side
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
   one `Facts`.
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

- **One `Declared` per `(owner, name)`, rendered once at the end.** No offset arithmetic outside
  `render`.
- **Rank, less derived wins:** annotation → `enum` → column → association → `attribute` → long tail
  → `delegate` → convention → query interface (`Source::Interface`, lowest). At equal rank a typed
  declaration beats `untyped`, then the first to speak wins.
- **A rank across two documents is spent by the loser declining** (`Facts::source`,
  `Source::outranks`). An `enum` or `attribute` re-typing a column goes through
  `Model::retyped_columns`, and the schema withdraws that column.
- **The user's own `def` is not in this table.**
- **Phase two is a query, not a loop** (`Facts::returns` over the union built by `Facts::absorb`,
  once, last, on demand). A `delegate` through a `delegate` is `untyped`.
- **`Facts::mixins` and `Facts::inherits` are lists, not `Declared`.** A `Facts` holding only those
  is not empty.
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
  - Filtered after the walk against `bundle_namespaces`. The whole chain is declared.
  - `own` code only.
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
5. **Only statements of a class body are macros**, plus the hosts `included do` (modules) and
   `with_options` (keywords merged innermost-first, so the call's own keyword wins).

- **`Comment::Relation` is monomorphic and nested under the model.** A project's own
  `Comment::Relation` makes this emit nothing for that element. One relation class per model.
- **Every `X::Relation < ActiveRecordRelation`** (`rails::RELATION_BASE`). The base is written
  once, holds `include Enumerable`, and is withdrawn if the project declares that name.
- **The query interface is written once, with `Return::Element` / `Return::Collection`**
  (`types.md`):
  - All of `QUERYING_METHODS` plus `Persistence::ClassMethods` and `with`.
  - Each row has a side: `Both`, `Relation` (`size`, `length`, `empty?`, `to_a`, `each`, `new`), or
    class only (`instantiate`).
  - A name may refuse a *type*, never exist on the wrong side.
- **The class side goes on the model's base** (`synthesize::base_of`), or on `ActiveRecord::Base`
  only when the bundle declares it. Abstract classes keep it.
- **Overloads render on one line** (`Declared::overloads`). An overloaded member has no
  `Facts::returns`. Never let a `(*untyped)` arm claim arity 0.
- **`where` has no arm for `WhereChain`.** Keyword hashes don't count as arity.
- **A `scope` is two declarations** (singleton + relation, one span). Relation copies are held and
  sorted by owner.

### Concerns

- **Instance macros go on the module**; the user's `include` carries them.
- **Class-side things (`scope`, `class_methods do`, hand-written `module ClassMethods`, and
  `included do extend M`) are fanned out onto each includer's singleton** (`Context::includers`,
  transitive). They live in the concern's document, with the span on the `def`
  (`syntax::def_span`, the whole `def … end`).
  - The block speaks first.
  - Visibility is read from the file.
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
| `attribute` | Declares a type only when the 2nd positional is one of `COLUMN_TYPES`' ten (serializers can't produce that). With no cast type it goes to the host test. Always optional. The column wins across documents |
| `delegate` | Always declares the member (`untyped` if a hop fails). The prefix must be a plain identifier. A setter is `(T) -> T`. In a concern it goes on the module. `to:` an ivar declines the type |
| Long tail (`LONG_TAIL`, 29 names / 17 families) | Affix shapes. `Installs::Nothing` and `Installs::Elsewhere` (`helper_method`) are in the table on purpose. `serialize` withdraws the column. Attachments are gated on `framework_classes`. `instance_*: false` only as a literal |
| Callbacks | Rails' 23 names, unmapped, kept on abstract classes. The block is handed the record |
| Mailers / jobs | Gate on superclass or include, never on `def perform`. Suffix list plus exact `ActionMailer::Base`. `ActionMailer::MessageDelivery` is written once and unmapped. A class's own `perform_async` wins. `rails::convention_of` is shared |
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

- **Types:** `COLUMN_TYPES` has ten, and everything else is `untyped` (never `untyped?`).
  `datetime` → `Time`. `null: false` drops the `?`. `array: true` wraps in `Array[…]`. The primary
  key is read from `create_table`.
- **The provenance comment carries file, table, column, type and nullability.** That's how the card
  shows it.
- **The SQL reader is a scanner, never a parser** (`Scan::hidden` for quotes, dollar-tags and
  comments).
  - It feeds `schema.rs`'s `Table`/`Column` (`Schema::from_tables`).
  - Its 48-row word map ends at `schema.rb` words.
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
  - `members` goes on both sides. `Struct#each` is `untyped`.
  - Fixed half `Source::Interface`, per-name half `Source::Struct`.
- **`sig` / YARD:**
  - *Derived*, never a place; hover shows them as footnotes. Sorbet wins over YARD.
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

## Framework singletons

| Chain | Return | Status |
|---|---|---|
| `Time.zone` | `ActiveSupport::TimeZone` | shipped |
| `Rails.root` | `Pathname` | shipped |
| `Rails.cache` | `ActiveSupport::Cache::Store` | shipped |
| `Rails.application` | the project's own `< Rails::Application` class | shipped |
| `Rails.logger`, `Rails.env`, `Rails.configuration`, `Time.current` | — | **declined**: the real class answers through `method_missing` / `define_method` |

- **The bar is "does the named class hold the members the next call asks for"**, not "is the return
  knowable".
- **Declare only the return, unmapped.** Check both ends against the graph (`singleton_classes`).
  Read the owner's keyword from `Namespaces::opens`.
- **The host is `config/application.rb`**, gated by `rails.enabled` alone.

## Keeping the pass cheap

`settle` runs the pass before every resolve. Gates, in order:

1. **Per-document gate** (`context_would_be_the_same`): one document's `Contribution` against its
   held one. It runs before the walk. The held map is also the walk's memo.
   - It is keyed on "rubydex re-indexed this document", never on a file stamp.
   - Only `index_buffer` names a document. **Every bulk route sets `touched_all`**, including
     `index_workspace`.
   - Absent means *not visited*, never *contributed nothing*.
2. **Whole-`Context` comparison**, which is strictly wider. When it fires, keep the fresh
   contributions.
3. **`generators_would_repeat_themselves`**: the files, checked by `stat` (never trust a
   notification).
4. **`record`'s text test**: re-index only documents whose RBS changed.

- **The parse memo is keyed on text and on which readers ran.** `rails::MODELS` gains members after
  the walk. `Fresh::Disk` is `stamp_of`. Buffers are hashed. `structs::read` and
  `rails::read_routes` are not memoised.
- **Two gates, two questions. Never share a clause.** Instruments: `Analysis::walks`, `passes`,
  `Synthesized::indexed`.
- **Gem discovery drops the held map** (`engine_prefixes` changes).
- **Cost model:** re-indexing generated RBS into a settled graph is ~32 ms per document plus
  ~0.7 ms per declaration. That cost is rubydex's `consume_document_changes`. There is no byte
  term: shrinking comments or concatenating documents buys nothing. That is why a document is one
  body.
