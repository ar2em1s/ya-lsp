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

# Declarations ya-lsp wrote itself

## The boundary

- **Every generator reduces to producing RBS text, and the output format is the boundary.**
  `indexing::index_source` and `Types::harvest` both take a `&str` and neither knows where it came
  from, so "read `db/schema.rb`", "read `belongs_to :user`" and "read a `sig` block" are one
  feature. That also bounds it: a module that may only emit RBS cannot special-case a hover card,
  reach into `completion.rs`, or grow a rung in `types.rs`. `workspace/rails/` is the only Rails
  knowledge in the crate and its only output is text.
- **A generated declaration has nowhere to jump to, and that is the cost of the whole section.**
  Generated text is indexed under a URI with no file behind it, at offsets into bytes nobody can
  open. `analysis/synthesized.rs` is the translation back, consulted at exactly one place:
  `locator::site`, the single point a `Definition` becomes a `Site`. Everything navigational
  reaches a file through it, so one lookup covers all of them.
- **The table is keyed by definition, not by declaration.** A declaration is the *merge* of every
  definition of a name, so `Story#title()` generated from `t.string "title"` and `Story#title()`
  written as a `def` are one declaration with two definitions — the ordinary reason a schema
  answer is *derived*. Rewriting the declaration's location would take the user's own `def` off
  the map. The key is `(document, offset)`.
- **No mapping means no place, never a guess.** A generated definition whose offset no mapping
  covers answers `Origin::Unknown`, and `locator::site` returns `None`: the row is dropped from
  the picker, the link is not offered, the hierarchy has no item. A card that is right above a
  jump into a file the user does not have is worse than not typing the receiver at all — the card
  is checked once and the jump is trusted forever.
- **Mappings may nest and the narrowest containing the offset wins**, the same rule as the spans
  under the cursor in `locator::locate`.
- **One kind of body is a place, and it is the only one.** Every other span is a member's — a
  `def` the generator wrote because a line of the user's code implied it. A namespace Zeitwerk
  conjures from a directory holds no member at all, so the body *is* the declaration, and a
  directory is not a line anybody can be sent to. `Facts::namespace` therefore takes an `at`, and
  the place is the `Api` of a `class Api::V1::Foo` — **the file that confirmed the directory**,
  which is the same rule every member span obeys and not an exception to it. The generated
  document is keyed by that file rather than by the directory, so a name has as many places as
  there are files declaring into it: none of them is *the* declaration and all of them are one,
  which is Ruby's own operational test. Measured over six corpora at 210 such names behind 1,932
  openers: 93 answered no place and 4 do. A body reached any other way — a wrapper `open`
  introduced, a route-helper host, a relation class — carries no span still, and those are lines
  no file wrote.
- **The span travels in the walk's projection, not in the generator.** A confirming file is
  typically a plain controller on no generator's list, so the *sources changed* gate never sees
  an edit to it and `Context`'s own gate is the only one that can. A span left out of
  `Contribution` would be a mapping that goes stale the first time somebody presses return above
  the `class` line — the one failure `Synthesized::record` cannot catch, because it compares the
  text and the text is byte-identical.
- **The path proposes and the file confirms — the spelling as well as the existence.**
  `autoloaded_namespaces` camelizes, which is Zeitwerk's default inflector and not Zeitwerk: an
  application registering an acronym autoloads `app/serializers/rest/` as `REST`, and a chain
  proposed as `Rest` agrees with nothing that file writes. `rails::confirmed_spelling` matches
  the directory with its underscores dropped against the segment, case-insensitively, and takes
  **the file's** spelling — so `not_already_silenced` confirms `NotAlreadySilenced` exactly as
  before and `rest` now confirms `REST` too. The acronym table is
  `config/initializers/inflections.rb`, which is Ruby that only runs; what is on disk is the
  answer rather than the rule. It cannot conjure a name the directory does not spell, which is
  the bound that matters.
- **A mapping may point into a file this generator never read, and one kind of member does.**
  Every other place is a line in the document the generator just read — a column's `t.string
  "title"`, an association's `has_many :comments`. ActiveRecord's query interface has no such
  line, because **no file in the project declares `Story.where`**: it was written with no span at
  all, which made a *Resolved* card over it send a reader nowhere. It does have a definition, and
  the bundle is already indexed, so the place is **found rather than read** — `Story.where`
  answers `activerecord/lib/active_record/relation/query_methods.rb:1033`.
- **The lookup is Ruby's own method lookup, asked of the graph.** `rails::RAILS_RELATION` is
  **one name** — `ActiveRecord::Relation` — because rubydex has already walked `relation.rb`'s
  `include FinderMethods, Calculations, SpawnMethods, QueryMethods, Batches, Explain, Delegation`,
  so the ancestor walk *is* `Method#owner`. Checked against `Method#source_location` under
  activerecord 7.2.3.1, 8.0.5, 8.0.5.1 and 8.1.3.1: of the 127 names the generator declares,
  **125 resolve and all 125 land on the line Ruby names**. The two that do not are `instantiate`,
  which is the class side's alone, and `default_order`, which is in Rails' main branch and no
  released version. That include line is **byte-identical across all four versions**.
- **The class side is ten names and one delegation.** `Story.where` is
  `delegate(*QUERYING_METHODS, to: :all)` at `querying.rb:24`, and no reader can expand a splatted
  constant into ninety method names, so `ActiveRecord::Querying` holds no `def` to find.
  `RAILS_CLASS_SIDE` names the three modules Rails does write a class-side `def` in —
  `Persistence::ClassMethods`, `Core::ClassMethods`, `Inheritance::ClassMethods`, **10 of 10
  matching `source_location` across the four versions** — and then falls through to the relation,
  which is exactly what the `delegate` line says those names are. `ActiveRecord::Base`'s own
  singleton is deliberately **not** on the list although Ruby searches it first: the generator
  writes the class side onto that very singleton, so looking there finds this crate's own
  declaration, which has no place, and the ten that do have one would lose it to the fall-through.
- **It runs after the resolve and cannot run inside the pass.** Every generator writes the text
  rubydex is about to link, so while one is running the graph holds **four** declarations —
  `Object`, `BasicObject`, `Module`, `Class` — and every question about a gem's classes answers
  nothing. So `Facts::render` states *names* (`Declarations::named`, tagged `Source::Query`) and
  `Analysis::place_generated_members` answers them one step later. `Analysis::regenerate` is the
  three steps as one function, because two of the three is not a valid state.
- **Asked again on every resolve, never cached across one.** A bundle can change under a
  workspace and a jump into the version that went away is silent — the same sentence the rest of
  this file is built on. What bounds the cost is a memo *within* one pass: the class side is
  written onto every base in the project and states the same names on each.
- **The callbacks are deliberately not looked up, and that is the line.** `before_save` is built
  by `define_model_callbacks`; a jump into the machinery that defines a *family* of methods tells
  a reader nothing about the one they asked about. The rule is **the file that defines this
  method, not the file that defines methods** — which is why `relations::callbacks` keeps
  `Source::Interface` and only `relation_base` and `class_side` carry `Source::Query`.
- **Two things the corpora caught here and the suite could not have.** A hit on the object model
  is not an answer: `Kernel` alone declares `select`, `format`, `open` and `test`, and four
  positions answered `Story.select` with `IO.select` in `core/kernel.rbs` — so the walk applies
  `locator::ruby_s_own`, the same rule the name rung has for the same reason. And one `def`
  borrowed by several declarations is still **one place**: the interface is declared once per
  relation class and once per base, so a name rung offering all of them offered mastodon's
  `find_each` five times over, four of them the same line. `locator::all_places` deduplicates
  across declarations, which `places` cannot — it is asked about one declaration at a time.
  Over the audit's own draw that is **32,140 repeated URIs down to 29,488**, most of which
  predate this row.
- **What it was worth.** Over 5,523 drawn positions: **297 cards named a tier and no place, and
  231 of them were this** — 157 *Resolved* on the class side, 74 *Derived* on the relation side.
  After: **246 lists gained a place, 0 emptied, 0 distinct places lost, 0 tiers moved**, and every
  new place is in activerecord. What is left placeless is 39 namespaces a directory declares,
  where no line declares them and nothing is wrong, and 27 ivar reads, which are another row's.
- **A generated `class Foo` is deliberately left unmapped.** The schema reader maps its columns and
  not the `class Story` it hangs them off, because `create_table "stories"` does not declare
  `Story` — the model file does. So goto-definition on `Story` offers one place rather than two,
  and the picker grows no second `Story` row opening `db/schema.rb`. A generator wanting the
  reopening navigable has to map it on purpose.
- **One generated document per source file *and body*, by naming rather than bookkeeping.**
  `synthesized::generated_uri` is a pure function of the source's URI and the body's own name —
  `ya-lsp-generated:file:///…/db/schema.rb#class:Story` — so regenerating cannot leave two answers:
  rubydex replaces a document indexed under a URI it holds, and the table is keyed by that URI.
  `generated_uri` is deliberately **total**: a `None` arm would be a silent "generated nothing".
  The *source* stays recoverable from the name alone, by the prefix `generated_prefix` writes and
  `source_of` cuts, which is what `environment`, `completion::Locality` and `hints` read rather
  than asking the table.
- **A body is `Facts::render`'s own key, `(is_module, name)`, and `Facts::split` cuts on it.**
  `Instance` and `Singleton` of one class are one body because they open one `class X`; `Module` of
  the same name is a different one because `class X` and `module X` are two declarations of one
  constant RBS refuses to hold. Three things make the split safe rather than delicate, and each is
  a property something else already had: it happens **after** every generator has spoken and every
  collision is settled, so no rank is re-decided and `declared`, `returns` and `source` are still
  asked of the whole file's facts; **a collision cannot cross a part**, because two generators that
  name one member collide on `(owner, name)` and the owner picks the part; and **every field of
  `Facts` is keyed by an `Owner`**, so the partition is total.
- **The one thing that is bookkeeping: `record` takes every part of a source at once.** A source
  that wrote three bodies and now writes two has to *forget* the third, or a dropped table answers
  forever. Taking the whole list is what makes the set of documents a source owns move atomically;
  a per-part `record` would need a second list somebody kept pruned, and that failure is silent.
- **`record` makes both calls in one function, so they cannot drift.** The graph learns the
  declarations and `Types` learns what they return. It does not mark the graph dirty (the caller
  does) and does not blank `interface` blocks (that rule is for signature files somebody else
  wrote).
- **`record` keeps the text it wrote and hands over nothing when nothing changed.** The generators
  run before *every* resolve, because what they read is not only their source file but which
  classes exist, and nothing watches that. Re-indexing identical RBS would make rubydex invalidate
  and re-link the generated document's dependents for no change. Measured on a real application,
  **finding out there is nothing to do costs two orders of magnitude less than doing it.** Both
  halves are compared, text and
  mappings: a table that moves down `db/schema.rb` generates identical text and different spans.
- **`forget` runs from `Analysis::forget`, every route a file leaves the index by.** Nothing
  re-reads a file that is gone, so a generated declaration left by a deleted `db/schema.rb` would
  outlive every chance to correct it.
- **The generated URI is deliberately not a `file:` URI — the backstop under all of the above.**
  `ya-lsp-generated:file:///…` is a URI rubydex indexes happily and `DocUri::from_uri_str`
  refuses, because `Url::to_file_path` refuses it. So even with an empty table and a bug here, a
  generated document cannot become a `Location`, a symbol row or a squiggle. It is not under the
  workspace prefix either, so `is_own_code` says no. The table decides which generated definitions
  become a *useful* place; the scheme decides that none can become a wrong one.
- **`hierarchy::mention` is the one place that builds a `Site` without `locator::site`**, covered
  by the scheme rather than the table. It points at the `include` or `< Superclass` line naming an
  ancestor the resolver never found, so it reads a document URI directly. Generated RBS should
  never name a class the graph does not hold, and if it did, `DocUri::from_uri_str` drops the row.
  The failure direction is a missing row, never a wrong one.
- **A generated declaration ranks as the user's code, one directory out — and it took a
  measurement to get there.** `Story.new.` offers `summary`, `description`, `id`, `title`, then
  `tap`: what the class was written to do, then what its table says it holds, then what every
  object can do. That order is pinned by a test and has never moved. What moved is the reason for
  it. The first reading was that a generated document is absent from `own_documents`, so
  `completion::Locality` never scores it and `group` reads the miss as *somebody else's code* —
  the same treatment a `sig/` signature gets. **That is wrong the moment the project patches a
  class at the far end of the chain**, which every Rails application does: `group` is the first
  term of the ranking key, so a `def` in `config/initializers` on `Object` outranked every column,
  association and enum the receiver's own table implies, and `distance` — which had the right
  answer — was never reached. Measured on a real application: two `Object` patches took ranks 1
  and 2 of `story.`, above `body`, `score` and `title`.
- **So `Locality::at` files a generated document beside the file that implied it, at the same
  step.** One source writes one generated document per body, so the set cannot be computed from the
  source's URI any more: the generated documents are picked out of the pass over the graph that
  `Locality::at` already makes, and each is mapped back to its source by `synthesized::source_of`,
  which is the naming read backwards. Still no `Synthesized` in `completion.rs` and no second scan. `own_documents` itself is untouched,
  which is what keeps the `rename` rule below saying what it says: a generated definition is still
  not somewhere ya-lsp will edit. A generated document whose source is *not* the user's code stays
  unscored, exactly as before.
- **And the entry carries *which kind of document scored*, because the pinned order above is not a
  distance question.** A column is declared in `db/schema.rb` and the method beside it in
  `app/models/story.rb`. Score them by locality alone and the order holds from a cursor in `app/`
  and inverts from one in `db/`, which is the pinned order becoming an accident of layout. So
  `Ranked` has a `generated` term between `distance` and `locality`: a near owner still wins, and at
  the same owner the user's own `def` leads what the generator wrote for it, from anywhere in the
  tree. **The corpora argue against it and it is kept anyway**: it moves 18 words out of rank 1 into
  the top ten while tightening every band below, so a developer does reach a table's members slightly
  more often than the `def`s beside them — and an order that changes with the cursor's directory is
  one nobody can learn. `completion.md` has the key and the numbers.
- **A generated definition makes `rename` refuse, and that is the right answer.** `rename` reads
  *every* definition and refuses unless all are somewhere ya-lsp will edit; a generated document is
  not the user's own code by the test that keeps a vendored bundle out. So a `class Story`
  generated from the schema makes renaming the model declined out loud. The alternative carries the
  rename into `db/schema.rb`, a generated file whose column is not renamed by rewriting it.

## What may host an association macro

**The gain is a subtraction.** `active_model_serializers` and `jsonapi-serializer` spell
`has_many`, `has_one` and `belongs_to`, store what they are handed and define **no method**.
`attribute` can be gated on a *shape* (the cast type); `has_many :comments` is byte-identical in a
model and in a serializer. So the question is asked of the **host**.

- **The gate is an admit list at the top of `Association::declare`**: a macro declares only where
  the body is a `module`, or where the class is one the project treats as an ActiveRecord model.
  Everything else declines to nothing.
- **A `module` passes unconditionally, and that clause is load-bearing.** A concern inherits
  nothing, so any rule phrased over superclasses deletes every macro in a concern body. Over six
  corpora **almost every** association call written outside a model is in a module.
- **What counts as a model is the union `relations` is filtered out of, not `Context::models`.** A
  class is one if `models_of` climbs its spelled superclass chain to `ActiveRecord::Base`, *or* if
  some macro made it a collection element. `relations`' two extra filters are about **emitting a
  relation class**, not about believing the class is a model, so `relations` is derived from
  `modelled`. Asking the narrower question loses declarations over six corpora; asking this one
  loses **none** — a `Tag < ActsAsTaggableOn::Tag` or an `EmailMessage < Ahoy::Message` has its base
  in a gem's `lib/` and is a collection element because other models say `has_many :tags`.
- **The element half is deliberately *not* gated.** A serializer's `has_many :statuses` still says
  `Status` is a collection somewhere. What the gate removes is the **declaration**, never the
  relation class; gating the input of the set the gate reads would make it circular.
- **Provably a no-op for `Kind::Scope`.** `relations` is a subset of `modelled`, and a `scope`
  already declines unless `relations` holds its own name.
- **`delegate` is out of scope and stays out.** `Module#delegate` really defines the method on any
  class, so a serializer's own `delegate` calls are correct.

**An admit list rather than a blocklist, though a blocklist measures the same.** Declining every
body whose name, superclass or `include` ends in `Serializer` removes exactly the same declarations
and costs nothing. It is the wrong rule by direction: a blocklist fails **open** — the next
serializer gem, or an application whose base class is spelled otherwise, keeps declaring, and
nothing says so. An admit list fails **closed**, this file's standing rule. Concretely: an admin
framework's `ResourceController` defines its own class-side `belongs_to` for nested-resource
routing, defines no method, and is declined without anybody naming it.

**No table in `rails/mod.rs` for this.** Every other convention there is a list of names; this rule
consults `Context::models`, computed per workspace. What matters is that it is stated in one place.

**Two defects currently cancel, and touching the inflector uncovers one.** One corpus writes its
controller calls as `belongs_to "shop/product"` and they declare nothing — but not for this gate's
reason. Rails' `camelize` maps `/` onto `::`, giving `Shop::Product`, which that corpus defines;
`inflect::camelize` splits on `_` only and every candidate misses. Teaching `camelize` Rails' rule
would, without this gate, create a wrong declaration per call. The unit test therefore uses a
**symbol**, because a fixture written the slash way passes with the gate removed.

**What it measures, and the instrument had to be built.** The tier sweep reads **no change at all**
over every swept corpus — a limit of the instrument: `sweep.tier` calls every `**N possible
definitions**` card `list` whatever N is, so a list getting *shorter* is invisible, and the phantom
members were never on a typed chain. Two things say what changed. The pass logs how many members it
declares per settle, and that drops in the two corpora that use serializers heavily while staying
flat in the four that do not. And at the positions whose member name is one this stops declaring,
**candidate lists get shorter, none gets longer, none collapses, none appears**. Re-measure that
pair; the tier sweep will read nothing whatever you do.

## A class nobody can name, and a member that still exists

Some association calls say outright that no single class can be named: `polymorphic: true`, whose
companion `*_type` column decides one row at a time, and either of the two keywords that name a
class outright — `class_name:` and `source_type:` — written as anything but a string literal. One
corpus lets the host application supply its own user class through a runtime object, and no reading
of the text turns one into a name. All of them used to emit **nothing**.

- **A deliberate refusal and a lookup that came back empty are both silence, and one rung down
  silence is the name rung's cue to fire.** With no `Follow#follower()` in the graph, the cursor on
  a bare `follower` fell through to matching the word against every `def follower` in the workspace
  — which on two corpora is an unrelated service object and on one is a **spec** file. The decline
  was right and the thing after it was not, so the decline is now written down: `def follower: ()
  -> untyped`, spanned onto the `belongs_to` line like every other generated member.
- **`untyped` is the honest type, and the alternative is not a better type — it is no member.**
  The member exists at run time whatever the column holds. What nobody can write down is its
  class, and that is exactly what `untyped` says.
- **The line between these two and an ordinary miss is evidence, not degree.** `belongs_to
  :parent_comment` naming no `ParentComment` is a *lookup* that failed — the reader may have
  camelized the wrong word, or the class may be somewhere this pass cannot see — and it still
  declines whole. `polymorphic:`, `class_name:` and `source_type:` are ActiveRecord's **own
  keywords**, so their presence is also positive evidence that the call is an association at all,
  which is the same argument `attribute` makes from a cast type.
- **What is lost is the type and never the member.** Every name the macro installs that does not
  have to name a class is installed exactly as it would be — the writer, and a collection's
  `_ids` pair, whose element type was never this reader's to say. The three constructors are the
  one group that goes: Rails writes them `unless reflection.polymorphic?` because there is
  nothing to instantiate, and a `(*untyped) -> untyped` constructor for the `class_name:` half
  states nothing the association's own name did not. Dropping the `_ids` pair as well was tried
  and **cost a real answer** — one corpus writes `self.user_ids = …` in the very model whose
  `has_many :users` carries the runtime `class_name:`, and lane 2's check 1 reported it as a
  highlight with no definition.
- **`polymorphic:` is read first and wins.** That is Rails' order rather than a preference —
  `compute_type` is never reached for a polymorphic reflection — and the corpus writes the pair
  together inside a `with_options class_name: "User"`, so the order is not hypothetical.
- **Below it, one list decides the class and the first of it that is written answers.**
  `class_name:`, then `source_type:`. The list is stated once and both halves read it — the one
  that resolves the class and the one that records the refusal — so a readable `class_name:`
  leaves nothing undecided even where the `source_type:` beside it is a method call, because that
  `source_type:` was never going to be asked.
- **`source_type:` names a class and `source:` names a member, which is why the order of those two
  is the whole of what reading it buys.** `source_type:` is ActiveRecord's disambiguator for a
  `through:` whose source association is polymorphic, and it has to be read *before* the `source:`
  on the same line: eight association lines across four corpora write both, and camelizing the
  `source:` names a class the application does not have on every one of them. Rails rejects
  `source_type:` as an unknown key without a `through:`, so it is read wherever it is written
  rather than gated on one — a line that could confuse the two does not boot.

## The class side of a concern

A concern's instance-side macros are declared on the module and reach every includer through the
`include` the user wrote — most of the macros written in concerns over six corpora. The rest are
**class-side**, `scope` almost entirely, and a module cannot own one: `scope :expired` in an
`Expireable` concern is `Poll.expired` *and* `Invite.expired`, one relation type per includer for
one line.
Declaring it on the module's own singleton answers `Expireable.expired`, which raises, and still
leaves `Poll.expired` unanswered — wrong in both directions rather than absent.

- **The declaration goes on the includer and lives in the concern's document.** Two `Declared` per
  `(concern, includer)` — `Owner::Singleton(includer)` and the relation copy every scope gets — both
  returning `relation_of(includer)`, span still on the `scope` line, so `Poll.expired`,
  `Invite.expired` and `Poll.expired.expired` all jump to the same line in `expireable.rb`.
  `Synthesized` is keyed by *source*, and the source is the concern.
- **What bounds it is `relations`, a gate not written for this.** Every model owns a relation class,
  so a fanned `scope` has a type to return; a PORO that includes a concern declines exactly as a
  `has_many` naming it would. Inventing a `Plain::Relation` is the one way this could answer worse
  rather than not at all. Over six corpora **every includer is a model**.
- **`Context::includers` is resolved after the walk, not during it.** An `include` is recorded on a
  definition at index time, so which classes include a concern is a question about the *graph* —
  and it needs no resolve, because rubydex records the `Mixin` while indexing. Spellings resolve
  with `generated::candidates`, the same `compute_type` list `class_name:` uses, so
  `include Sidekiq::Worker` adds no row.
- **The closure is transitive and moves nothing in practice.** `ActiveSupport::Concern` hands an
  inner concern's `included` block to whatever includes the outer one. Over six corpora it changes
  **no pair at all** — a correctness property with one fixture behind it. Only **classes** are values; a module
  is walked through and never recorded. The cycle guard is about *source*: a module that includes
  itself is a `NoMethodError` at run time and an infinite loop in a closure.
- **A `scope` of one name written in both a concern and an includer produces two places and one
  type, and both stand.** Two lines of Ruby both really install `Poll.recent`, and Ruby keeps
  whichever ran last. They *may* both stand because they agree about the type **by construction** —
  a `scope` returns a relation of the class it is installed on. That is narrower than "two
  generators collided": where two documents disagree about a type, the rank is spent by the loser
  declining (`Source::outranks`, the column-versus-`enum` rule). Never fires over six corpora.
- **The `enum` is declined in a concern, and the includers do not repair it.** The label is a
  `String` and its column an `Integer`; that pair is resolved by having the schema decline the
  column, which it can only do for a `(class, attribute)` pair, and a concern claims no table. With
  the includers in hand it is *worse*: a concern included by two models re-types a column in each,
  and the class-side half cannot fan out without the instance half following. **One call in the
  whole corpus** writes this shape.
- **A concern nobody includes declares nothing and does not fall back to itself.** Most corpora hold
  at least one.
- **What it costs is a longer name rung, inherent rather than a defect.** One line of Ruby really
  defines 24 methods: `Paginable#paginate_by_max_id` is declared on all 24 including classes, so a
  *typed* receiver resolves and an untyped one gets a candidate list as long as the includer count
  where it had a single name match. Every lateral move over five applications is that one macro.
  Swept: **better in bulk, worse nowhere, laterally only there** — and nothing at all in the corpora
  that write no class-side macro in a concern.

## The class side every model has

**A class side is bounded by the superclass chain, not by which classes own a relation.** Putting
the query interface only on classes that own a relation leaves out a model with no `has_many` and no
`scope` — and worse, **a class side is inherited**: a corpus writing `scope :select_fix` in its
`ApplicationRecord` made it a collection element, wrote an `ApplicationRecord::Relation`, and put
the names on a singleton *every model inherits*. `Category.order(...)` then answered a relation of a
class no row is an instance of, at dozens of positions.

- **The relation set is a union, deliberately not a replacement.** A class gets a relation class for
  either of two reasons and neither implies the other: some macro made it a **collection**, or it
  **is a model**. Replacing the first with the second loses classes over six corpora — a
  `Tag < ActsAsTaggableOn::Tag` or an `EmailMessage < Ahoy::Message` is a real model whose base is
  in a gem, so the superclass walk stops at a name the application does not define; the rest are
  plain classes a `has_many` camelizes onto.
- **`models_of` walks the chain, and the chain is `compute_type`'s.** One hop would call a
  namespaced `Shop::Order` no model: an engine monorepo writes most of its models two hops from
  `ActiveRecord::Base` and some four or five, through its own `Shop::Base`. Each hop resolves the
  spelling Rails' way — `generated::candidates`, innermost nesting first, bare name last — because
  `class Address < Shop::Base` inside `module Shop` and `class LineItem < Base` beside it name one
  class two ways. `seen` guards a
  cycle the *source* can be written with.
- **An abstract class keeps its relation and gets no class side — two different facts.** The coarse
  version (declining to make an abstract class a *collection element*) measures **more down-moves
  than it repairs**, because a `scope` declares nothing when its class has no relation. The
  **class side** is the half that has to go: `ApplicationRecord.order` raises in Ruby, and being
  *inherited* it answers for every model whose own class side is out of reach. A corpus'
  `Assistant.find` under a Zeitwerk-conjured namespace is that position: a conjured namespace costs
  that class its own singleton members, and handing it `ApplicationRecord`'s turned a name-based
  guess that was **right** into a typed answer that was wrong. A handful of positions, the only ones
  in five corpora.
- **Both spellings of abstract are read, and the literal is checked.**
  `self.abstract_class = true` is the old one, `primary_abstract_class` what Rails 7 generates;
  most corpora write the first, one the second, and one writes the first in the very class the
  defect is about. `self.abstract_class = false` is written **nowhere** in the corpus and is still
  read as the `false` it is, because matching on the name alone would take a concrete model's class
  side away. The set
  is the *project's*, not the file's: the file writing `ApplicationRecord::Relation` is rarely
  `application_record.rb`.
- **A model that writes no macro is on no list, so the list gains a membership decided *after* the
  walk.** Every predicate in a `Wants` row is a question about one document; whether a class is a model is
  a question about the chain above it, complete only when every document has been seen. So
  `Context::models` is computed after the loop and each model's defining document joins
  `rails::MODELS` then. `Context::settle` sorts every list afterwards, so appending cannot change
  which document writes what.
- **A relation class that already had a home keeps it — measured, not reasoned.** The first build
  emitted every relation class into the document that *defines* its element, on the argument that
  nothing in one is mapped. It cost one corpus **dozens of positions**, among them a
  `Channel::Telegram.find_by`, which stopped resolving to its own class side and started resolving
  to `ApplicationRecord`'s — the very defect the class side exists to fix, reintroduced by it. So
  the first document that *asks* still writes it, and the defining document is used only for a model
  no macro asks about. **The mechanism is not understood, and until it is, nothing that already had
  a home may be relocated.** Where more than one document names a superclass — a corpus reopens
  `class Shop::Product < Shop::Base` in its specs — the **lowest URI** wins, because the loop
  visits the graph in no defined order.
- **A name rubydex invented is not a name a generator may declare on** — found by measurement twice
  now. An anonymous `Class.new(Shop::Calculator) do … end` is spelled
  `<hash>:<offset><anonymous>`, an ActiveRecord model by every rule here and not RBS, so
  `Synthesized::record`'s parse gate threw away **the whole document**. One corpus writes dozens of
  such specs, reading in the sweep as hundreds of positions getting worse. The identical shape reaches a
  route-helper host. The test is `generated::is_constant_path`, called by `Namespaces::spellable`
  and by `rails::hosts_routes` rather than inlined in the second.
- **The block parameter depends on this and belongs to `types.md`.** What belongs here is the
  dependency: it measures **better with real down-moves** without a per-model class side and
  **better still with none** once every model owns its own relation, because every down-move was an
  `@categories.each do |x|` typing `x` faithfully as `ApplicationRecord`. Over five applications the
  pair measures **better in bulk, worse nowhere, laterally nowhere**, split roughly evenly between
  the two halves, and *answered* does not move.
- **The name-based candidate list gets longer, and that cost is real.** Every relation class and
  class side declares the same names, so a `.where` on an untyped receiver offers **more than twice
  the possible definitions** it used to. The tier does not move and no answer is worse, but a reader
  meeting that card is not better off. The fix was said to be elsewhere — a generated declaration
  mapped to nothing is not a *place*, so it does not belong in a list of possible definitions —
  and **that turned out to be half an answer**. Those declarations are mapped now, to the `def`
  Rails really wrote, so they *are* places and they belong on the list. What was actually wrong is
  that they are the **same** place: `locator::all_places` deduplicates across declarations, and the
  list is shorter than it was before either change.
- **What the whole thing costs.** Generated documents, RBS bytes and declared members all grow by a
  fraction; the pass and the resolve each grow by a few per cent of the settle. Measure it on the
  largest corpus, where the settle is longest.

## The table a nested model claims

**Two rules, and the second is worth more: the table a namespace prefixes, and the fact that a table
is read by more than one class.** Claiming a table by pluralizing a **top-level** class's name and
declining every other is defensible only if `compute_table_name`'s `full_table_name_prefix` is Ruby
that only runs. It is a `def` in a file, and reading it is the first half.

- **`compute_table_name` is copied; what is not copied is declined rather than approximated.**
  `model_schema.rb` is
  `"#{full_table_name_prefix}#{contained}#{undecorated_table_name(model_name)}#{full_table_name_suffix}"`.
  `undecorated_table_name` is `demodulize.underscore` then `pluralize`, which is `rails::table_of`.
  The affixes are `module_parents.detect { |p| p.respond_to?(:table_name_prefix) }` — **innermost
  parent first**, which `affix` walks. `contained` is the one part that is not syntax: a model
  nested inside another *model* is `parent_singular_child_plural`, needing the parent's table and
  its parent's, and it is declined — measured, **the classes nested inside a model across six
  applications name no table any of those applications has.**
- **Two spellings of the prefix, and the commoner names its module by what the constant
  *resolves to*.** `def self.table_name_prefix` is the minority spelling across six corpora;
  `isolate_namespace` is the rest — every plugin and every engine writes it. Rails' precedence is
  one clause, `unless mod.respond_to?(:table_name_prefix)`, so a module writing the
  method out wins; hence two fields, not one list. `Rails::Engine` installs
  `generate_railtie_name(mod.name)` — `underscore(name)` with the separator underscored away — and
  `mod` is the module the constant **resolved to**: a plugin writing `isolate_namespace Provider`
  inside `module ChatIntegration` has tables beginning `chat_integration_provider_`.
  So `TableNames::isolated` carries the `compute_type` candidate list and no prefix, and
  `rails::engine_prefix` is what the caller asks once the constant is settled. A leading `::` takes
  the one-entry list, and it is the node rather than the spelling that says so —
  `constant_spelling` drops the colons. **One function builds both candidate lists**,
  `syntax::candidates`, because Rails modelled `compute_type` on Ruby's lexical lookup.
- **`table_name_suffix` is written nowhere in six applications and is read anyway.** Same syntax, same walk,
  and ignoring it is the only way this reader can name a table that exists and is not the one the
  class reads. Five lines.
- **A table really is read by more than one class, and that is the half that pays.** "A table two
  classes claim is claimed by neither" protects against the inflector landing two names on one
  table — but read literally it also declines `Account` against a
  `CLI::Maintenance::Account`, which are the same convention applied twice and both
  really read `accounts`. **One corpus writes over a hundred such classes.** So the guard is narrowed: several
  claimants survive when they all **demodulize to the same name**. Over six applications that
  declines a handful of tables — mostly a `GroupUser` against a `GroupUsers`, the inflector collision
  the rule exists for — and keeps every legitimate extra claimant.
- **The gate on new claimants is the superclass chain, load-bearing rather than tidy.** `claims` has
  always been every top-level class the application defines and not only its models, which costs
  nothing while a table has one claimant and a great deal once a nested class may claim one: the six
  corpora hold dozens of nested non-model classes whose name lands on a real table under a different
  last segment, and every one would take that table from the model that reads it.
  `rails::Projection::is_model` climbs `superclasses` to `ApplicationRecord` or `ActiveRecord::Base` and
  declines every one of them. It is a **chain**, not one hop, and carries a `seen` set — not caution about Ruby
  but about *source*: a superclass cycle cannot run and can be written.
- **A written `self.table_name` and the class whose name implies the same table meet, and both
  readings are in the corpus.** Letting the written one simply *replace* the inflected one is right
  where the writer is the real model (a `TopicViewItem` saying `topic_views` where the `TopicView`
  whose name implies it is a plain view object) and wrong where the writer is a throwaway (a
  migration's `LegacySetting` saying `settings` and taking those columns off `Setting`; test doubles
  saying `posts` and taking them off **`Post`**). So the guess survives the meeting only when the
  class it is about is a model. **Several models across six corpora lose their table without that
  clause**, and the one class the replacement is right about is a view object that still does not get
  it. The two rules are coupled: reading a symbol `table_name` without the multi-claim change takes
  yet more tables off the models that read them.
- **A model reopened to nest something under it loses every column unless `claims` dedups.**
  `claims` is filled per *definition*, so `class AuditLog` written again in
  `app/queries/audit_log/unpublish_alls_query.rb` pushed its own name into the claimant list
  **twice**, and the two-claimant rule declined it. **Several models across six corpora lost every
  column to it**, one of them reopened in half a dozen `lib/` files. The narrowed rule admits them
  with no clause of its own, since one name demodulizes to itself; the claimant list is deduplicated
  so "one claimant" means one class rather than one `class` keyword. This is the largest single
  effect in the section: one corpus' entire up-move is one such model.
- **A symbol is a table name.** `table_name=` is `value&.to_s` in Rails' source, and this reader took
  a string literal only — so **the symbol calls in six corpora were invisible**, and in the corpus
  that writes the most of them almost every one was, every one a nested throwaway model.
  `symbol_or_string` is the one call site.
- **The joined name a new owner is written under must introduce no namespace.** A generated
  `class A::B::C` where nothing declares `A` costs `A::B` its own singleton members, silently — so
  `Namespaces::spellable` is asked here as well as by the struct reader, beside `Declarations::open`'s
  `nesting`. It declines **none** of the conventional nested claimants in six corpora and is there
  as a bound on what a later rule may widen.
- **A `def` is the fifth thing that puts a document on a list, and it had to be.** A module writing
  `def self.table_name_prefix` calls nothing at all, so `Wants::defines` reads the **definitions**,
  gated on a `self.` receiver. The lookup is a string rather than a hash of one: rubydex records a
  `def` under its name *and* its parameter list, `table_name_prefix()`, and a row spelling that out
  would stop matching silently if the rendering moved. One `strings()` lookup per singleton method.
- **What it measures, and two of five corpora have nothing it can reach.** Swept over five
  applications: **better in bulk, worse nowhere, with real lateral movement**. One corpus' only
  nested models are throwaways under `db/old_migrations`, and the corpus with **no `db/schema.rb` at
  all** measures exactly nothing despite holding the most nested models of any of them. What moves is
  concentrated in one or two reopened models per corpus — checked one at a time with `atpos.py`.
- **The lateral moves are `name -> list` and they are the real cost.** A column declared on every
  class that reads the table makes a *name-based* list longer, and what lengthens it is throwaways: a
  controller asking `reject_media` now gets a short candidate list headed by two `DomainBlock`
  classes defined inside migrations. `RANK` calls it a
  draw; a reader would call it slightly worse. The alternative — excluding `db/migrate` by path — is
  a rule about where a file is rather than what it says.
- **A Rails engine shipped as a gem is declined, closed at both ends.** `claims` and `nested` are
  `own`-only, because a table is the *application's* database table, and the prefix a gem engine
  declares is written in its `lib/<engine>/engine.rb`, which is indexed but is not an
  `is_generator_source` document. So `Blazer::Query` does not claim `blazer_queries`.

## The nesting an association is resolved in

**Built from Rails' source rather than from a remembered convention.** Every other reader here
camelizes a name and asks whether the application defines it; an association's class is not looked up
that way by Rails, and the difference is a whole namespace wide.

- **`ActiveRecord::Inheritance#compute_type` is the list, copied not approximated.**
  ``name.scan(/::|$/) { candidates.unshift "#{$`}::#{type_name}" }`` then `candidates << type_name`,
  so `Shop::LineItem` naming `Adjustment` asks `Shop::LineItem::Adjustment`, then
  `Shop::Adjustment`, then the bare `Adjustment` **last**. `Models::candidates` is that, and
  `Association::resolved` takes the first the application defines. The empty prefix is not among
  them, and the walk is over the class's **joined** name, so `class Shop::LineItem` at top level
  asks exactly what `module Shop; class LineItem` asks.
- **The order is the whole feature, because it makes wrong answers right rather than absent ones
  present.** Where a bare name and a nested name both exist, taking the bare one is wrong at
  **dozens of** sites over six corpora: almost all of them a `db/migrate` throwaway
  `class Account < ApplicationRecord` shadowing the application's own, the rest gem-owned classes
  like `ActiveStorage::Attachment`. **A fixture that does not contain
  a name spelled both ways cannot tell the two readers apart**, which is why both tests declare the
  class twice and assert on the *place*.
- **`Model::collections` had to learn the same answer, and that is not a repetition.** Which relation
  classes a workspace needs and what each member returns are asked by two functions, and two
  different answers put a `Shop::Order` member behind an `Order::Relation`. `collections` takes
  `known` for that and nothing else.
- **A written `class_name:` is walked too, and `::` is Rails' own escape.** `compute_type` is handed
  whichever name the reflection has, so `class_name: "Order"` inside `module Shop` is
  `Shop::Order`. A leading `::` takes `compute_type`'s first branch — absolute, constantized with
  no candidates — the one spelling producing a one-entry list. A `scope` produces one too, for a
  different reason: it returns a relation of the class it is written on.
- **A prefix in front of a name that is not a constant leaves a name that is still not one**, which
  is why the walk is safe to widen a decline into. One corpus' admin controllers write
  `belongs_to "shop/order"` repeatedly; every candidate still ends in `Shop/order`. Nothing in
  `candidates` validates a candidate, because the caller's `known` is an exact set-membership test —
  also `compute_type`'s own `candidate == constant.to_s`.
- **`composed_of` is deliberately not given the walk, and Rails' source is the reason.**
  `aggregations.rb` reads `class_name.constantize` and `reflection.rb:441` is a bare
  `name.constantize`, both absolute, against `reflection.rb:497`'s
  `active_record.send(:compute_type, name)` for an association. Two macros in one directory resolve a
  class name by two rules because ActiveRecord does; `Typing::OwnClass` in `tail.rs` is unchanged.
- **What it measures, and it is one corpus.** Swept over five applications: **better in bulk, worse
  at a handful of positions, with real lateral movement**. Almost all of the gain is the engine
  monorepo, where every model is inside a module and a large share of its associations were
  declining; the corpora that namespace nothing move barely at all. A namespaced `InventoryUnit#order`
  is the shape: a candidate list running to the hundreds becomes one resolved answer.
- **The lateral moves are `name -> list` and the rank is the wrong instrument.** An `option_values`
  went from one helper's `#option_values`, which is not what the code means, to a list containing
  `OptionType#option_values` and `Product#option_values`, which is. The content improved and the rung
  did not.
- **The few that got worse are one shape, they became *right*, and what they cost is the table
  rule.** A CLI file and a post-migration define throwaway models inside a class, and
  `has_one :account_stat` in the dummy `Account` beside them names the dummy — which is what
  Rails names. But a **nested** class claims no table, so the card goes from
  `AccountStat#statuses_count` with the schema's provenance to a four-candidate list. Widening
  `claims` naively is wrong on exactly the corpus this walk was built for, so it needs the prefix
  reader above — which is why the two are described together and why those positions answer again.

## What the pass may look at

- **`Graph::get` cannot be used by this pass at all.** The declaration map is built *by*
  `Resolver::resolve` and the pass runs immediately before it, so it answers about the **previous**
  settle. Measured when `Context` is built: the declaration map holds **a rounding error's worth of
  declarations against the definitions actually present** on a cold open, and is still one settle
  behind when the bundle lands. Same shape on all six corpora. A reader asking it declares nothing
  until
  the user's next keystroke, and a unit test will not catch that, because `card()` opens a document
  and so settles twice.
- **So the lookup reads the definitions, and a whole-graph projection is affordable *if bounded*.**
  Spelling every class and module costs about as much a settle as the whole resolve does. Filtering
  on the **last segment** first — a `StringId` compare — and spelling only what matches costs
  **roughly an order of magnitude less**. `wanted_namespaces` is
  that bound: the proper prefixes of everything the application declares, minus what it declares
  itself, plus the four constants this crate invents or looks for by name.
- **`Context::namespaces` is a second set beside `classes` and must stay one.** `classes` is *may a
  macro name this*; `namespaces` is *may a generated name be spelled around this*. Merging them was
  measured and is wrong: across every association site in six applications, most resolve and the
  rest decline; about half the declines are answered by a spelling this reader never tries, and the
  whole graph supplies a bare handful of the remainder — **every one a Ruby core class a camelized
  name collided with**, almost all of them `Object`.
- **`Typing::Gem` demand is four sites larger than any count of application code.** Three Rails
  engines write the macros in their own `app/`, which is a generator source:
  `ActiveStorage::VariantRecord` (`has_one_attached :image`), `ActiveStorage::Blob::Representable`
  (`:preview_image`), `ActionMailbox::InboundEmail` (`:raw_email`), `ActionText::RichText`
  (`has_many_attached :embeds`). That is every Rails 6+ bundle, and on some corpora it is the whole
  of what moves.
- **The row that pays is the conjured namespace, and it belongs to every generator.** Over six
  corpora the generators declare on owners carrying thousands of namespace segments, and **a quarter
  of them are introduced by a joined name with nothing declaring them**. A small minority of those
  are declared by the bundle (`ActiveStorage`, `Turbo`, `ActionView`,
  `ActionView::Helpers::Tags`, `ActionMailer`, a gem's `ConnectionPool`). The rest are
  Zeitwerk-conjured application namespaces.
- **The struct reader's own namespace gate measures zero and is widened to match anyway.** Over
  every `Struct.new`/`Data.define` site in the corpus, some namespace segments are gaps and the
  bundle supplies **none** of them. `spellable` and `Facts::render`'s `nesting` ask **one** question,
  pinned by a test rather than claimed from a corpus.
- **A name two files spell differently is a `module` only if every one of them says so.** Opening a
  body for a name somebody else declares as a class costs hundreds of positions on one corpus, and
  `false` is the answer that writes nothing.
- **A document this crate generated is never read, and the filter is the URI scheme.** The graph
  still holds the previous settle's generated documents when the pass reads it — the raw spike shows
  `ActionMailer::MessageDelivery` answering `class:generated` on four corpora — so a lookup that
  could see one would answer differently on every pass. `synthesized::GENERATED_SCHEME` is a
  property of the URI rather than a list, and
  `two_settles_over_an_unchanged_workspace_generate_the_same_bytes` asserts it.
- **Three readers need *ancestry* rather than a name lookup.** A mailer and a job are recognised by
  the *spelling* of a superclass; a name lookup gives a kind, not a chain. A serializer is told from
  a model by a host test. And `delegate` has **no** `Context` gate at all — its second phase asks
  `Facts`, which holds only what this pass wrote.
- **Two of `Context`'s outputs are `own`-only by design.** `hosts` and `claims` mean *the
  application*.
- **What the projection measures, over five applications: better in bulk, worse nowhere, laterally
  nowhere.** Almost all of it is the one corpus that writes Active Storage attachments in quantity,
  and the shape is a *tier*: the answered total barely moves while **precise moves in bulk**, most of
  that a name-matched list becoming a typed answer. The corpora writing one or two attachments move
  by one or two. The **namespace** row measures nothing on any of them — its segments are
  `ActiveStorage`, `Turbo`, `ActionView`, `ActionMailer`, and nothing in this corpus calls a
  singleton method on one. Kept because a conjured namespace's damage
  is silent and this is the half that can be repaired at all.

## The conjured namespace

**Everything below is a property of rubydex 0.2.5, and the pinned rev retired it.** It is kept
because the code is: `Namespaces::spellable` still declines, and removing a decline is a behaviour
change owed its own sweep rather than a side effect of moving a pin. What changed is the tense. On
the pin a generated joined name costs a conjured namespace nothing — the fixture that reproduced it
reaches `build()` in every row, the subclass row that was the trigger included — and `spellable`
reduced to `is_constant_path`, the decline removed entirely, sweeps **74 up, 0 down, 41 sideways**
over five applications, the corpus that once cost 234 positions among them. So the rules below are
why the decline was built and what widening it would cost, not a description of damage the server
can still be shown. **The one thing not attributed is which upstream commit removed it**, so the
decline outliving its cause is the conservative half of that.

**The damage was real and only one rule for it cost nothing.** A handful of positions in one corpus
resolved with the decline in place and fell to a name-matched list without it. Five rules were built
and swept before one got there with nothing worse.

- **A joined RBS name introduces every segment above the last.** `class Reports::Registry::Metric`
  introduces `Reports::Registry`; where nothing declares `Reports`, the module loses its **own**
  singleton members, on itself and on every subclass, and so does anything that inherits them. Both
  halves are silent.
- **It is not about `class` versus `module`, and three of the five cuts were spent finding that out.**
  The minimal repro is `class Ns::Holder` — a **class** — holding a `Struct`, with `Ns` conjured:
  `Ns::Sub.thing` falls to the name rung. What matters is only whether the top segment is declared.
- **Two safe spellings and no third.** Either every segment above the owner is declared, so the
  joined name introduces nothing; or the owner's **immediate parent** is a name the application
  writes `module` for, which `Declarations::open` opens as a body of its own. `Reader::spellable`
  declines everything else.
- **Writing the namespace out is not a third spelling.** An explicit wrapper *declares* a kind where
  a joined name only *implies* a namespace. A build that wrote every segment out emitted
  `class Api`, `class Accounts`, `class Actions`, `class ActiveStorage`, `class ActionMailer`,
  `class ActionText`, `class ActionView` — every one a **module** in reality — and on one corpus
  **cost more positions than it gained**. The kind of an undeclared segment is unknowable: it may be
  Zeitwerk's (a module) or a gem's (some gems really declare `ConnectionPool` a class).
- **So the decline is narrowed rather than removed.** `Namespaces::spellable` recovers exactly the
  declared-module corner. Measured: **better in the two corpora that write the shape, worse in
  none, and unmoved in the rest.**
- **Only the struct reader reaches the shape.** A relation class is named after the *element*, which
  is a model and so a class; `enum` scopes go on the model itself; an association on a model inside a
  module declares a name the file already wrote, which rubydex reopens.
  `class Channel::Api < ApplicationRecord` with an `enum` and a `def self.` answers **identically**
  either way.
- **The damage needs a second reference site**, which is why it reproduced over a corpus long before
  it reproduced in a test. One call to `Reports::Registry.supported?` survives the joined name; two
  do not. The regression test is three files and asserts the **tier**: a workspace that small holds
  one `supported?`, so the failure spells itself *"Matched on the method name alone"* and not *"N
  possible definitions"*. Asserting on the candidate list is what makes this look unreproducible.
- **A wrapper declares no member, so it records no span and can never be a place to jump to** — the
  no-mapping rule doing its job. The canary's file count does not move.
- **What is still not repaired.** Only the *immediate* parent is opened, so a declared module further
  up a name is not protected: with `module Api` declared and `Api::V1` conjured, a generated body on
  `Api::V1::UsersController` still introduces `Api`. A namespace the **bundle** declares *is*
  reachable. **Extending to the nearest declared ancestor must not be attempted on argument alone**:
  declaring a namespace no file declares costs hundreds of positions on one corpus *whatever keyword
  it carries*, measured three ways with identical down-sets and unexplained. What the shipped rule writes is
  always a line some `.rb` wrote verbatim; a nearest-declared-ancestor rule would have to synthesise
  the segments in between.
- **`annotations.rs` is in the same family and must stay in step.** A `def self.` carrying a `sig` or
  a `@return` inside a `module` is `Owner::Module` / `Owner::ModuleSingleton`, picked from the
  innermost body the walk is in.

## The namespace a directory declares

**`class A::B::C` where `A::B` is undefined raises `NameError` in Ruby.** discourse writes 125 of
them and they run, because **Zeitwerk defines the namespace from the directory**: an autoload path
with a `policy/` in it and no `policy.rb` beside it gets a `module` named after the directory — an
*implicit namespace*. So the declaration this generator writes is not an invention. It is the one
constant in this whole section that the running application certainly holds and no file writes down.

- **The damage is a wrong answer everywhere, and silence is only how it is noticed.** rubydex binds
  a plain `def` in `handle_remaining_definitions`, once, after the convergence loop;
  `resolve_lexical_owner` walks *up* the lexical chain whenever the enclosing definition has no
  declaration **yet** and ends at `break Some(*OBJECT_ID)`. A compact-path class whose middle segment
  is missing is declared a pass too late, so every `def` inside it lands on `Object`. Measured on
  discourse before the fix: of the 126 method names its 46 compact-path service classes define,
  **57 reported `Object` as their owner** — `initialize`, `call`, `delete`, `edit`, `enable`,
  `reason`, `users` — so every receiver in the workspace answered all of them and the jump landed in
  a service object. After: **7**, and every one of those is a real top-level `def` in a spec file or
  RBS's own `Object#to_json`. The `def` line itself answering nothing is the same bug read from the
  other end: the class does not hold the member, so `definition_to_declaration_id` misses.
- **`resolve_lexical_owner` already excepts `SingletonClass` from that walk**, with the reason
  written out — "returning the surrounding scope would attach its members to the wrong owner and
  never recover". A `Class` with no declaration yet wants the same exception. That is the upstream
  fix and it is three lines; this one is in-house because the namespace is missing from the graph
  whatever rubydex does with it.
- **Rails' own three bounds, and none of them is this crate's** — `rails::autoloaded_namespaces`.
  The anchor is a segment literally named `app`, which an engine and a discourse plugin keep.
  `NOT_AUTOLOADED` is `assets`, `javascript` and `views`, the three Rails leaves out. And Rails'
  glob is `app/{*,*/concerns}`, so a `concerns` directly under a root **is a root** and nothing
  named `Concerns` is ever conjured.
- **The path proposes and the file confirms.** A directory conjures nothing unless a document in it
  declares a constant that is the directory's name plus exactly one segment, which is Zeitwerk's own
  contract for the file. Doubling the evidence is what keeps a directory of unrelated code — a
  `spec/` fixture tree, a plugin's `app/` holding one oddly-named class — from declaring a namespace
  nobody uses.
- **The filter runs after the walk and never in it.** What makes a directory's namespace real is
  that **nothing else declares the name**, and "nothing else" includes the bundle, which
  `Analysis::bundle_namespaces` only answers for once every document has been seen. Every conjured
  name is a proper prefix of a class the application declares, so it is already in
  `wanted_namespaces` and the bundle has already been asked about it.
- **The whole chain, not its last link.** `app/services/chat/thread/policy/` answers `Chat`,
  `Chat::Thread` and `Chat::Thread::Policy`. That is what makes every survivor **spellable** by
  construction: each prefix is either declared already or is itself in the map, and `walk` declares
  the survivors into `Context::namespaces` before any generator renders.
- **A module per directory, and the directory is the source.** `Facts::namespace` is the one fact
  with no member, no type and no place: the classes under a namespace are in the files below it and
  rubydex holds every one, so this declares the constant and stops. No member means no span, so
  `locator::site` has nothing to offer and the no-mapping-no-place rule needs no exception. A
  `policy.rb` written tomorrow takes the name out of the map and `forget_stale` drops the document,
  with no list to prune.
- **It widens the section above rather than arguing with it.** More names are now `module`s the
  application declares, so `Declarations::open` wraps more bodies in their immediate parent — the
  spelling that rule already calls the safer of the two. `a_document_that_opens_one_body_twice_is_still_indexed`
  is where that shows: one body is wrapped and the other joined, which is still two openings.
- **It is not the rule that section forbids, and the difference is the evidence.** What must never
  be attempted on argument alone is *synthesising the segments between a name and its nearest
  declared ancestor* — inventing `Api` because somebody wrote `Api::V1::UsersController`. This
  invents nothing: a directory on an autoload path is a declaration Rails itself makes, and where
  there is no such directory the name is still declined. The distinction is checkable, which is why
  the two tests in `rails/mod.rs` are the same class with the same superclass in two directories.
- **The audit reads 13 findings *new* on discourse and every one of them is the removal of a wrong
  answer.** They are the same 13 positions that were already findings under another name, all of
  the shape `SomeService::Class.call(` — and the item completion ranked **first** at each was
  `Object#call`, the leak itself. Probed both ways at
  `app/controllers/admin/search_controller.rb:9`: without the namespace, 20 items led by `call`
  (`detail: Object#call`), `params` and `reason`; with it, 4 items and none of those three. What is
  left there is a **pre-existing** gap this never touched — discourse writes `.call` in a
  `class_methods do` block inside an `ActiveSupport::Concern`, so rubydex records it as an
  *instance* member of `Service::Base` and the class object cannot reach it. A counter that goes
  the wrong way because a wrong answer stopped being offered is the audit working.
- **`own` code only, and that is a bound rather than a proof.** A Rails engine's `app/*` really is on
  the application's autoload paths, so an engine could conjure a namespace too; it is left out
  because a gem that ships an engine declares its top namespace in `lib/` almost without exception,
  and the measured damage is all in workspace code. If an engine ever needs it, the widening is one
  flag — the same one `Context::classes` already took.

## `Struct.new` and `Data.define`

Swept over five applications — **the largest corpus is not swept**, and it holds most of the
declaring calls, so the claim is "worse nowhere over five": **better in bulk, worse nowhere, with
real lateral movement**, almost all of it in one corpus. The lateral moves are all `name -> list`,
because a struct member is spelled the same as somebody else's method — `category`, `url`,
`redactions`, `username`. That is its one real cost.

- **The one generator that is not Rails, and it is in `analysis/` for that reason.** A `Struct` is
  plain Ruby, so `analysis/structs.rs` sits beside `annotations.rs` — same contract, same `Wants`
  row, same merge into the one generated document its source file names.
- **The class is whatever the call was assigned to, and there are two spellings.** `Struct.new`
  returns an anonymous class, so a call with no name declares nothing. Most uses over six corpora are
  `CONST = Struct.new(…)`; **a handful are `class Line < Struct.new(…)`**. The second
  ships because a low count *requires* it here: RuboCop's `Style/StructInheritance` is on by default
  and flags it, so that count is *suppressed* — some of the survivors carry a `rubocop:disable`.
  **A zero may exclude a construct only when the construct also costs nothing.**
- **A namespace nobody defines declares nothing, narrowed to the declared-module corner.** This is
  the first generator that declares **inside a body somebody else wrote**.
  `module Reports::ReportMetricRegistry` is a whole application's spelling for a `Reports` that
  Zeitwerk conjures and no file writes. Refusing outright costs **more than a tenth of all calls and
  members** over six corpora, concentrated in two of them. `Namespaces::spellable` recovers the
  declared-module corner and nothing else.
- **`Foo::Point = Struct.new(:x)` is declined, and it is the one decline that really does name a
  class.** It never occurs in the corpus. The reason is the difference between `Analysis::qualified_name` and
  `Analysis::spelled_name`: a constant *path* on the left of an assignment is resolved rather than
  nested, so `Foo::Point` inside `module A` is `A::Foo::Point` or the top-level one depending on what
  else exists — a constant lookup, in a pass that runs before anything is resolved.
- **Naming no class at all is the largest single group.** `Struct.new(:state).new({})` inside a
  `let`, a local, an instance variable — in one corpus every single use is that shape.
- **A constant assignment and a generated `class` are one constant.** rubydex records
  `Point = Struct.new(:x)` as a constant *write*; this pass writes `class Point` in RBS;
  `Point.new(1, 2).x` resolves through the second with nothing added to resolution — the same
  sentence that made a generated `module Storyish` and the user's own one module. Worth proving
  before building on: the alternative needs a rung in `types.rs`.
- **Every positional argument must be a symbol literal; one that is not declines the call.**
  `Struct.new(*NAMES)` names members this cannot see, and declaring the ones it can would be a class
  *missing* methods rather than one with none. A couple of calls in six corpora.
  `Struct.new("Name", :x)` is declined by the same rule and is the one place declining is not merely
  conservative — that call defines `Struct::Name`. It never occurs.
- **A `keyword_init:` hash is skipped rather than refused**, because it changes the constructor and
  nothing this declares — and roughly a third of all calls write one.
- **A name that is not a legal method name is declined on its own, not with the call.** An
  unspellable name makes `Synthesized::record`'s parse gate refuse the *whole* generated document.
  `Struct.new` raises on such a name at runtime, so this is a belt for source that never runs.
- **The block is not decoration, and a `def` in it wins.** Dozens of calls carry a `do … end`
  holding a `def`. A member whose name the block also `def`s is not declared, because the `def` has a place to
  jump to. It bites twice in six corpora, both on `to_h`. **Instance side only**: `block_methods`
  collects receiverless `def`s, and a `def members` in the block says nothing about `Point.members`.
- **Those `def`s are read and deliberately not declared.** rubydex indexes a `def` inside a block
  as **`private Object#area`**, so they are answered on the *name* rung. Declaring them would be text
  this crate wrote pointing at a `def` rubydex owns, and if rubydex ever gives a block's `def` its
  enclosing constant it becomes a second place. Measured: declaring `class Boxed` with its readers
  **does not take `.area` away**, because `locator::resolve_typed` falls back to the name list when a
  typed receiver has no such member.
- **`members` is declared on both sides and is the only fixed member that is.** Declaring only the
  instance half leaves `Point.members` on the name rung with a candidate list *three entries longer*
  than before this reader ran. **That is how a generator makes an answer worse without making one
  wrong**, and every reader here adds names to lists it does not otherwise touch.
- **`Struct#each` returns `untyped` although Ruby returns `self`, and the reason is the fact table.**
  `Facts` holds one declaration per `(owner, name)`, so a member cannot be given two arms. Ruby
  returns the struct with a block and an `Enumerator` without one, and the only honest thing one
  declaration can say about both is nothing. `Types::harvest` drops `untyped`, so the chain falls to
  the name rung. `Data#with` has no such split and returns the class itself, which is what makes
  `coord.with(lat: 1).lng` answer.
- **The fixed half is `Source::Interface` and the per-name half is `Source::Struct`.** `Interface`'s
  docstring already said what it is for — "the one thing in the table no file says at all" — and
  `Struct#each` is that. The per-name half sits **directly below `Annotated`**: a `sig` above a `def`
  overriding a reader is the one collision either can reach, and a human writing the type down beats
  `untyped`.
- **The list is filled by a *constant* reference, a fourth predicate.** Every other `Wants` row asks
  about a call name, a path or a superclass; what puts a document on this one is `Struct.new`, whose
  **method** name is `new`. The constant is the rare half: barely one file in a hundred mentions
  either name. The match is on the last segment, so somebody's own `Foo::Struct` costs one parse and
  declines.

## The long tail

- **Twenty-nine macro names in seventeen families are one reader, and the shape is what makes them
  one.** The largest runs to dozens of calls across five applications, the smallest to one. Every one installs a **fixed
  set of members named as affixes around the names the call was given**, so the marginal cost of the
  twelfth is a row. `workspace/rails/tail.rs` is that shape — `Names` says where the names come from,
  `Shape` what is installed around each, `Typing` where an unwritten type comes from — and
  `LONG_TAIL` in `mod.rs` maps twenty-nine names to seventeen families. **A macro not in that table
  looks exactly like one nobody thought about**, which is why the four that declare nothing are *in*
  it.
- **Four of the twenty-nine declare nothing, in two categories.**
  - **There is no method.** `normalizes` (22 calls) calls `decorate_attributes` and appends to
    `normalized_attributes`; its two methods are defined once on the module. `encrypts` re-encrypts
    an existing attribute; `generates_token_for` installs its pair once in
    `ActiveRecord::TokenFor`. `Installs::Nothing`.
  - **There is nowhere to put it.** `helper_method` (49 calls, the largest in the tail) **does**
    define a method — `abstract_controller/helpers.rb` writes
    `def #{method}(...); controller.send(...); end` per call into the controller's `_helpers`
    module, which the **view context** includes. That host is the one thing in Rails this crate has
    no type for, so declaring on the controller would restate a `def` rubydex has and add a second
    *place*. `Installs::Elsewhere`.
- **What a template answers for a helper call today is already the jump.** A bare `current_user` in
  `app/views/stories/show.html.erb` hovers as `StoriesController#current_user` and goes to the `def`,
  on the **name rung**. What it does not do is complete — and completion in a template is empty for
  `ApplicationHelper#time_ago` too, which has no macro and is **an order of magnitude more demand**:
  template call sites naming an `app/helpers` method far outnumber those naming a `helper_method`
  export. So the gap is the **receiver**, not the declaration. Where declaring would win is the
  ambiguous half — most of the export half and a fifth of the helper half name something more than
  one `def` answers to; a corpus' `title` is written at ten times as many call sites as there are
  `def title`s. That gap is
  closed by the view-context rung in `analysis/views.rs`, and this table's job there is to be the
  **allow-list**.
- **A module needed a fourth `Owner`, and it is not `Singleton` with a module's name in it.** Almost
  every `mattr_accessor` call in the corpus is in a `module`, and the point is
  `Devise.pam_authentication`.
  `Facts::render`'s key is `(is_module, name)`, so an `Owner::Singleton("Devise")` would open
  `class Devise` beside the `module Devise` the instance half opened — two declarations of one
  constant. `Owner::ModuleSingleton` renders `module` and prefixes `self.`.
- **`alias_attribute` reaches the two-phase seam by a second road, which validates it.**
  `alias_attribute :sent_at, :created_at` needs `created_at`'s type, which is a **column** written
  into the schema's generated document in the same pass — one hop where a `delegate` takes two,
  asked of the same `Facts::returns`. So `Model::delegates`/`delegated` became `derives`/`derived`:
  the phase was never about `delegate`, it is about a type another document states. **What is
  declined there is the type and never the name** — `attribute_aliases` installs the pattern set
  whatever the old name turns out to be.
- **`serialize` re-types a column, and it is the one re-type that is right even when what replaces it
  is `untyped`.** It defines no method: `decorate_attributes` replaces the type of an existing
  member, which puts it in `Model::retyped_columns`. A `text` column carrying YAML answers a `Hash`
  or an `Array` and **never** the `String` the schema says, so withdrawing removes an answer known to
  be wrong, and a `serialize` with no `type:` (12 of 16) is still worth reading. **`store` withdraws
  nothing even though Rails implements it by calling `serialize`**: both of the corpus' two `store`
  columns are `json`/`jsonb`, not one of `COLUMN_TYPES`' ten, and the schema already declares them
  `untyped`. `Typing::Written` is the one place a class name is written down with **no gate at all**:
  the member is the column and certainly exists, so a `type:` naming something unreachable degrades
  to no answer.
- **Two of the four `type:` values the corpus writes are generic in RBS, and no unit test would have
  caught it.** `Array` and `Hash` take type arguments; had rubydex's parser refused a bare one,
  `Synthesized::record`'s gate would have thrown away the **whole** generated document while every
  rendering test passed, because none of them parses what it renders. It does not refuse, and
  `a_serialize_re_types_its_column_with_a_class_that_takes_type_arguments` pins that end to end.
- **The attachment macros are gated on a lookup rather than a projection.**
  `ActiveStorage::Attached::One` is in activestorage's `lib/`, on the far side of the engine gate, so
  `Context::classes` can never hold it. `Context::framework` is three `graph.get` calls per pass
  against `rails::framework_classes()`, and a bundle without Active Storage declares **nothing** for
  a `has_one_attached`. It declines the whole call rather than one member, because the reader *is*
  the macro's reason to exist.
- **A `prefix:` this cannot read costs the whole call — the only option in the file that does.** Every
  other keyword removes one member or changes one type; a `store_accessor`'s prefix changes what each
  accessor is *called*. `prefix: true` is Rails' shorthand for the store column's own name and is
  read; `prefix: SOME_CONST` declines every key.
- **An `instance_*` keyword is honoured only when written as the literal `false`.**
  `class_attribute :setting, instance_writer: false` really does not install `setting=`.
  `instance_writer: options[:w]` is Ruby that only runs and keeps the member.
  `instance_predicate: false` removes the **class**-side predicate too, which is `attribute.rb`'s own
  `if instance_predicate` around both branches.
- **`delegated_type`'s static half ships and its per-type fan-out does not.** It installs
  `entryable_types`, `entryable_name` and `build_entryable`; `entryable_type` is the polymorphic
  **column**. The fan-out needs each element of `types:` to be a class the application defines and
  needs a relation of the owning class, which is a reader and not a table row.
- **Everything here is `Source::Derived`**, so a column, an `enum`, an association or a `sig` of the
  same name has already claimed the position. The one arguably ranked too low is `composed_of`: it
  names a class outright, so a column of the same name displacing it is wrong (Rails' `define_method`
  overrides the attribute method). **A couple of calls in five applications**, neither colliding with a column,
  so the fix is a per-family `Source` and a table column, not worth one until a corpus shows the
  collision. The one ordering that needs thought is `serialize`, and it is not a rank at all — it is
  a withdrawal, spent the way an `enum`'s is.
- **The three `Shape` constructors are values rather than `const fn`s, and coverage is the reason.** A
  helper whose only caller is a `const` item is compiled and never *run*, so `fn reader(...) -> Shape`
  would be three function bodies this file's 100% floor could never reach. `READ`, `WRITE` and `ASK`
  with struct update syntax say the same thing.

## `attribute`

- **`attribute` in a class body is not evidence that a method exists.** Three gems spell a macro
  `attribute` across the five applications and **only Rails' defines a method**.
  `active_model_serializers`' stores an `Attribute` in `_attributes_data`; `jsonapi-serializer`
  `alias_method`s `attribute` to `attributes` and stores a `Scalar`. Both answer `respond_to?`
  **false** for every name their macro wrote, because the value is on the record they serialize.
  **most of the corpus' `attribute` calls are on a serializer**, so a reader taking the macro at its
  word would be wrong far more often than not.
- **The cast type is the discriminator, and it is syntax rather than a guess about the class.**
  Rails' second positional is the cast type; AMS' is an options hash; jsonapi-serializer's is
  *another attribute name*. So `attributes::read` declares a member **only** where the second
  positional is a symbol naming one of `COLUMN_TYPES`' ten — which neither serializer can produce.
  Measured: **not one** call naming a mapped cast type is on a serializer.
- **A type symbol this crate cannot map is declined too, and that is *not* the directory's usual
  rule.** Everywhere else an unmapped type declares the member with `untyped`. Here it cannot,
  because `attribute :name, :tag_line` **is** jsonapi-serializer's list form: "a type I do not know"
  and "another attribute" are the same shape.
- **What the narrowness costs is a handful of calls across five corpora**, each naming a cast type
  this crate does not map (`:json`, an application's own `:geolocation_array`) or naming none at all.
  A few real members declined against an order of magnitude more false ones avoided.
- **Rails documents the precedence, and it runs the way an `enum`'s does.** `attributes.rb` says the
  macro "will override the type of existing attributes if needed", and that with no cast type "the
  previously defined type (if any) will be used" — so a cast type *re-types its column*.
  `Model::retyped_columns` tells the schema which columns to withdraw.
- **No rank moved, and moving one would have been wrong.** Putting `Attribute` above `Column` also
  puts it above `Association`, because a rank is a total order — and then `attribute :user, :string`
  on a class with `belongs_to :user` answers `String`. The rank table settles which of two generators
  *naming one member* survives inside one document; the retyped map is a generator **withdrawing a
  claim** across two.
- **The type is optional whatever the column said**, for the reason `Enum::declare` gives: nothing in
  an `attribute` call says the value is present, and an ActiveModel attribute is `nil` until
  assigned. It also bounds what withdrawing a column costs — `Integer?` is strictly weaker than a
  `null: false` column's `Integer`, so re-typing can never turn a right answer into a wrong one.
- **A concern owns an `attribute` where it does not own a `scope`, and it withdraws no column.** The
  member is one type whoever includes the module. The withdrawal is the opposite: a concern claims no
  table, and the classes whose columns it really re-types are its includers, which this pass cannot
  see — so the column stands.
- **Only the first name is read.** `ActiveSupport::CurrentAttributes.attribute` takes a list, so
  `attribute :user, :account` declares nothing here: `:account` is not a cast type. The miss is a
  *name*, never a wrong type.
- **The same problem reaches the association macros and is solved by the host test.** AMS also spells
  `has_many`, `has_one` and `belongs_to` — one corpus' serializers write over a hundred, a third of
  them naming a class the application defines. The cast-type discriminator cannot reach that; the
  host admit list does.
  `delegate` must never be gated that way.
- **What it is worth: a modest gain, one "worse", and real lateral movement** over five
  applications, from a small number of declared members. One corpus moves *provably* nothing — it
  writes no `attribute` in any body this reader walks, so both documents are byte-identical; another
  moves nothing the ordinary way. Only a couple of positions reach *derived*, and most of the gain is
  the **name** rung reaching a member that did not exist. Once the serializers are gated out the
  macro is a form-object feature, and form objects are not much chained through.
- **The one "worse" position is the instrument, and it is now a test.** A corpus writes
  `attribute :user_identifier, :string` and a `def user_identifier` under it, so the card gains
  a second **place** — and `sweep.py`'s `tier` reads any card containing "Defined in " as the
  name-based list. `an_attribute_beside_a_def_of_the_same_name_is_two_places` pins the behaviour. If a
  future sweep reports a position going *down* into `list`, check for this first.
- **Counting the shapes needs the reader's own walk, because `args[1]` lies.**
  `attribute :thing, default: 1` puts a `KeywordHashNode` there, which a naive survey reads as a
  custom type object and inflates the count several-fold. The real distribution over five corpora:
  **most calls name no cast type at all**, a minority name a mapped one, fewer still a type object,
  and a handful a symbol with no class behind it.
- **With no cast type the question moves to the host** — the same admit list `has_many` uses.
  What is declined is the type and never the name: `ActiveModel::AttributeMethods` defines the reader
  and writer whatever the cast is, and `Types::harvest` **drops** `untyped`. A cast type still
  re-types its column and an untyped call withdraws nothing, because `attributes.rb` says a call
  without one uses "the previously defined type (if any)".
- **The column wins, and it wins from another document.** `Source::Column` is rank 4 and
  `Source::Attribute` rank 7, but `Facts::declare` settles a collision only *inside* one document —
  and an untyped `attribute :note` and the `t.string "note"` it would shadow are in two, where two
  `def note:` lines are a silent overload set. So the loser declines: `Facts::declared` hands over the
  names the schemas said and `Elsewhere::columns` carries them into the model reader. The suite caught
  this — `an_attribute_re_types_the_column_it_overrides_and_defers_where_it_names_no_type` went from
  one declaration of `Story#note` to two.

## `delegate`

- **This is the generator that derives, and the fact table's second phase exists for it.**
  `delegate :name, to: :user` on `Story` needs `Story#user -> User` then `User#name -> String`; the
  first is an association this pass wrote and the second is a column it wrote into a *different
  file's* generated document, in the same run, with nothing resolved and nothing indexed.
  `Facts::returns` is the question and `Analysis::delegate_declarations` the union it is asked of.
- **The union is built once, last, and only when something asks.** Once, because a `delegate` may
  derive from any file's facts; last, because assuming a generator order is what `Facts::returns`
  exists to remove; on demand, because a workspace with no `delegate` must not pay a merge of every
  fact in the project. It is `Facts::absorb` and not `Facts::extend`: nothing renders the union.
- **What phase two writes is not in the union, so a `delegate` through a `delegate` is `untyped`.**
  The phase boundary, and the right answer: otherwise it means iterating to a fixed point over a graph
  a user can write a cycle into. Both members are still declared and both still jump.
- **What is declined here is the type, never the member — this reader is the directory's exception.**
  Everywhere else a declaration is declined whole, because the member's *name* is derived and a bad
  derivation invents a member (`belongs_to :parent_comment` would declare a `ParentComment` no
  application has). A `delegate` derives nothing — the name is a symbol literal and `Module#delegate`
  defines that method whatever `to:` holds — so an ivar target and a failing hop still declare the
  member with `untyped`. Safe rather than merely cheap because `Types::harvest` **drops** `untyped`:
  the member adds no entry to the return table, so a chain through it falls to exactly the rung it
  reaches with no declaration at all, and what is bought is the jump to the `:name` symbol. **The
  navigational half is free**, and it is where most of the movement comes from.
- **A rank has to be spendable from *outside* one document.** `Facts::declare` settles a collision
  inside a document; a `delegate :title` and the `t.string "title"` it shadows are in two, where two
  `def title:` lines are a silent overload set and the type is document order. An `enum` spends that
  rank by *telling* the schema which columns it re-types — two generators, one of them tellable.
  `delegate` can collide with any of them, so the rank is asked generically: `Facts::source` and
  `Source::outranks`, and the loser declines — always this one, since `Source::Delegated` is rank 8
  and the only thing below it is the query interface.
- **`prefix: true` is Rails' own guard and one character short of enough.** `Module#delegate` raises
  `ArgumentError if prefix == true && /^[^a-z_]/.match?(to)`, so a `prefix: true` on a constant or an
  ivar declares nothing here either. What Rails need not care about is `to: :user=`, which passes its
  guard and makes `user=_username` — a name `define_method` takes and RBS cannot parse, and **one
  unparsable `def` costs the whole generated document**. So a prefix must be an identifier with no
  punctuation, which is `plain` and is stricter than the test for a delegated *name*.
- **`enums`' method-name test cannot be reused, and a fifth of the names are why.** An `enum` label
  is *suffixed* into `draft?` and `draft!`, so it must be punctuation-free; a `delegate` name is
  written as the method it already is, and a real slice of the corpus' names end in `?`, `=` or `!`.
  Nothing wider is allowed or needed — **not one of them is an operator**.
- **A setter is the one signature that is not `(*untyped)`, and RBS is not the reason.** Rails writes
  `def #{name}(...)`, so every argument is forwarded and `(*untyped)` is what the arity partition
  needs. `name=` takes exactly one value whatever the target, so it says so.
  `def name=: (*untyped) -> String` parses fine — this is the signature being true.
- **`to: :class` is not a case.** It means `self.class` in Rails and reads here as a method named
  `class`, whose first hop finds nothing; all 10 corpus uses delegate to a `def self.` that is real
  Ruby in the file, so a case for it would reach the same `untyped` by more code.
- **A `delegate` in a concern hangs on the module, and unlike a `scope` it can.** `delegate :name, to:
  :user` is `User#name` in every includer, so there is one type to write down and `Owner::Module` is
  where it goes. A handful of the corpus' calls are in a module, most of those inside an `included do`.
- **The few `delegate` calls in a `class_methods do` are not read, and the `def`s beside them are.**
  `ActiveSupport::Concern` `module_eval`s that block on a nested `ClassMethods` module, so what it
  installs reaches the includer by `extend` rather than by the `include` a `delegate`'s owner is
  reached through. `concerns.rs` declares the `def`s onto that module; a `delegate` written there
  would have to be typed against a receiver the block does not name, and stays declined.
- **`to:` an ivar declines its type, and the reason is a measurement.** `Context` cannot know an
  ivar's class — that is `types.rs`' rung 3, which needs the graph this pass runs before. Reading the
  assignments out of the *file* was measured before it was declined: nearly every ivar target is
  assigned somewhere in the file, but only about a third are assigned `Const.new` — the only shape a
  class name can be taken from — and nearly half come from a local variable naming no class at all. A
  syntactic ivar rung is worth a handful of calls across six applications and is blind to nearly half
  of what it aims at. The members are declared either way.
- **The typed half has a small ceiling in real code.** A `delegate` is typed only when *both* hops
  answer. Across six corpora, **under a fifth** of readable `to: :symbol` names have a first hop this
  pass writes, and only a fraction of those delegate a name that is a column on the target.
  Everything else delegates to a PORO, a `Current`, a config object or a presenter. Swept over five
  applications: **better in bulk, worse nowhere, with heavy lateral movement**, and *derived* barely
  moves — so **most of the gain is positions that answered nothing and now reach the name rung**. A
  navigational feature with a typed corner rather than the other way round.

## Rails engines shipped as gems

- **`is_own_code` must not be widened for this, and the second predicate is the design.** Three
  features turn on it — diagnostics, rename, `workspace/symbol` — because nobody fixes a warning
  inside somebody else's engine. A generator wants "code whose declarations the user can reach", so
  `Analysis::context` asks `is_generator_source`. Widening the one predicate is one line and three
  regressions.
- **A list is open to an engine when what it reads declares members on a class the reader can name,
  and closed when it declares something scoped to an application.** That is `Wants::engines`, one flag
  per row. `Models`, `Annotated`, `Entrypoints` and `Routes` open; `Schemas` and `Renamed` close,
  because a `schema.rb` is the *application's* database. `Routes` being open is only permission to
  open the file — **what it may then say is `rails::Whose`**.
- **Two of `Context`'s six outputs mean "the application" and did not widen.** `claims` maps a table
  to the top-level classes whose name implies it; `hosts` is who gets `include RouteHelpers`, which an
  engine's controllers must not — `ActiveStorage`'s six would each cost an `include` of a module
  holding nothing for them. Both are guarded by `own` where they are filled, tested in both
  directions, which is why `hosts` stays closed while `rails::ROUTES` is open.
- **`Context::classes` did widen, and that is the bound on what a macro may name.** An engine's
  `has_many :variant_records` has to reach `ActiveStorage::VariantRecord` and both are under `app/`,
  so the exception is one directory per gem: a gem defining `class Story` in its `lib/` is still not
  this application's model.
- **A `Declared::at` may point into a gem, and that is not the no-place rule being bent.** That rule
  is about a *generated* document; `app/models/active_storage/blob.rb` is a real place.
- **The generator gate is worth little on its own, and measuring it apart from the indexing gate is
  what makes that sayable.** Indexing an engine's `app/` moves constants and no tiers; treating it as
  a generator source moves tiers and no constants. As positions that change tier it is **better in
  every corpus and worse in none**, concentrated in the two that lean hardest on engines — and the
  two gates move different position sets. It declares far more than it moves: the pass reads half
  again as many files, declares a third more members, runs **two to three times slower** (noisy, but
  the direction is consistent), and only a handful of `.member` positions in the whole corpus are
  better. **It is worth having because the indexing gate needs the same walk.**
- **An engine's routes go in the same `RouteHelpers` module, and the application's file wins a shared
  name.** `sources` is sorted workspace-first rather than by URI, which is load-bearing because the
  assignment below it is first-wins: an application that names `rails_blob_path` itself must be the
  one that declares it. It also keeps the `include`s in a file the user has, since they go in
  `sources[0]`.
- **A caption names the gem a file came from.** `workspace_relative` used to say nothing outside the
  root could reach it, so the first engine-routes card read "From `routes.rb`" — the same caption the
  project's own routes file gets. `gem_relative` walks up to the directory whose parent is named
  `gems`, so it reads `activestorage-7.2.3.1/config/routes.rb`.

## Route helpers

- **The wrapper may be inside a modifier `if`, and missing that cost ten of twenty-one helpers.**
  `activestorage` and `turbo-rails` both end their routes file `end if ActiveStorage.draw_routes`, so
  `Rails.application.routes.draw` is an `IfNode`'s body rather than a statement of the program. This
  was **latent on the workspace side too** — `Reader::walk` skips a call that has a receiver, so
  falling back to the top-level statements never found a wrapped `draw` either — and `wrapper`
  descending through `if`/`unless` fixes both. The one thing the engine differential caught that no
  test had.
- **The reader is checked against the real router, and that is the whole safety argument.** Every
  other generator fails toward *nothing*; a routes reader cannot, because `resources :stories` always
  names something. So the check is `ActionDispatch::Routing::RouteSet` itself, made to draw each
  corpus' own routes files with its `named_routes.names` compared file by file. Of every helper Rails
  names in the readable files this reader names **nearly all**, invents **next to nothing** and misses
  a small remainder; half the corpora match **exactly**. What it invents is behind an
  `if !Rails.env.production?`, which the harness drew with a stubbed `Rails` and a developer's editor
  really has. The fixture in `routes.rs` is that output written out in full.
- **`name_for_action` is copied, not approximated, and the scope level is the whole of it.** Four words
  joined in an order the level picks — `[prefix, names, collection_name]` at `:collection`,
  `[prefix, "new", names, member_name]` at `:new`, `[names, prefix]` at `:nested`. Three clauses are
  easy to get backwards and each was caught by the differential: `Resources#namespace` is
  `nested { super }`, so `resources :accounts do namespace :whatsapp` names `account_whatsapp_calls`;
  a plain `scope` never nests, so the same shape with `scope as: :x` names `x_a_bs`; and
  `Resource#name` is `@as || @name`, so `as:` **replaces** a resource's word.
- **The module is a top-level constant, and the reason for that is no longer rubydex's.** On the
  0.2.5 pin `Facts` could spell `module A::B::C`, rubydex indexed the module under exactly that name,
  and an `include A::B::C` written in RBS then did not reach it — the mixin was recorded as a
  `<partial>` ancestor and `find_member_in_ancestors` walked past the host as though it were not
  there. `ActionDispatch::Routing::RouteHelpers` was the first spelling and silently bought nothing.
  **The pinned rev linearizes it**, measured both ways: the ancestors read
  `[StoriesController, App::Web::Helpers, Object, Kernel, BasicObject]`, and a generated document
  holding `module App::Web::Helpers` with an `include` of it hovers `story_path` as
  `App::Web::Helpers#story_path`, identically to the top-level spelling.
- **`RouteHelpers` stays top-level on three arguments that are this crate's own.** Every route
  helper's hover card prints its owner, so moving the constant renames what a user reads at 9,479
  call sites; the collision a namespace would guard against measures **0 in six corpora**; and Rails'
  own module is anonymous, so a qualified spelling would be either a lie about `ActionDispatch` or a
  namespace invented for one constant. An application that declares `RouteHelpers` itself keeps it
  and this pass says nothing — here that costs the whole feature, which is the right price for never
  shadowing a name somebody chose.
- **`Facts::mixins` exists for this and nothing else.** A concern is a module the user already wrote an
  `include` for; route helpers have none, so the pass writes both halves. An `include` is deliberately
  not a `Declared`: it names no member, has no return type and cannot collide. A `Facts` holding
  nothing but `include`s is **not empty**, or `merge` would drop the document carrying them.
- **One `include` per controller, not one on the base — Rails' own behaviour.** The helpers are
  installed by an `inherited` hook on `ActionController::Base` and `ActionMailer::Base`. Writing them
  all costs nothing to get right: an application whose controllers descend from a **gem's** class has
  no base this pass defines, and each of its controllers is still a host by the `Controller` suffix.
  One line each, against the product of every helper and every controller.
- **A name that is not a constant path is not a host, and that clause was found by the parse gate.**
  rubydex's name for an anonymous class is `<hash>:<offset><anonymous>`, and
  `Class.new(ApplicationController)` in a spec is something a corpus really writes. `class ` + that
  is not RBS, so `Synthesized::record` refused **the whole document** — the one carrying every
  `include` — and the feature silently fell back to the name rung across the entire application while
  every test stayed green and the tier sweep still showed a large gain.
- **A `draw` is read at the prefix it was drawn at, into its own document.** `draw :admin` is the only
  thing that says where `config/routes/admin.rb` sits in the scope, so a drawn file cannot be read
  until its drawer has been. Its declarations go into **its own** generated document, because a span
  is a byte range with no URI. Not a corner: in one corpus **nearly every helper is in a drawn
  file**, and another draws one of its twice, under `scope module: :v1` and `:v0`.
- **Exactly one file declares each helper, first in URI order.** Two routes files naming one would
  land in two generated documents, where `Facts`' precedence cannot see them and RBS holds two
  `def story_path:` lines as a silent overload set.
- **Three constructs are read that look like Ruby running and are not**, each worth a measurable
  amount. Both arms of an `if`/`unless` — what the condition decides is whether a route is *mounted*,
  which no reader of text can know, and which is also true of `Rails.env.development?` blocks that are
  real in an editor's environment; one corpus writes its whole front end inside one, and an engine
  all of its. A literal array with a block, walked **once** — a `%w[users u].each do |root_path|`
  declares a run of helpers. And an unknown call carrying a block, walked transparently —
  `devise_scope`, `authenticate`, `constraints` change what a route *requires*, never what it is
  called, and without it a corpus loses dozens of helpers to one `devise_scope :super_admin do`. No allowlist, because a list of six is wrong for the
  seventh and the real router does the same thing for the same reason.
- **`mount` and `devise_for` are declined, and `devise_for` is the largest single miss.**
  `mount Sidekiq::Web, at: "/sidekiq"` really installs `sidekiq_web_path`, from a second inflector
  over somebody else's class name, at a couple of dozen call sites. `devise_for :users` installs
  about fifteen helpers whose list depends on which Devise modules the *model* declares, and a single
  one of those — `sign_up_path` — is written at over a hundred sites in one corpus. Both are misses
  rather than guesses.
- **Swept over five applications at every route-helper call site: better in bulk, worse nowhere, with
  a little lateral movement.** The *answered* total more than triples and the *resolved* tier grows by
  half — a controller, a mailer and a helper module answer precisely, everything else on the name
  rung. The lateral moves are all `name → list`. **Nothing moved down in any corpus.** The pass costs
  a small fraction of a settle, and the canary's file count does not move.
- **The gap is where a call site *is*, and it is most of them.** Of the corpus' receiverless
  `*_path`/`*_url` calls, only a small minority are in a controller, a helper module or a mailer —
  and **most have no enclosing class at all**, being in `RSpec.describe` blocks and templates. Those
  reach the helpers through the **name rung**: the declaration exists, `locator` matches by name, and
  the jump lands on `resources :stories`. Enough for goto-definition and hover, not enough for
  completion. Declaring on `Object` would fix it and is refused, because `story_path` inside a plain
  PORO is a `NoMethodError`.

## Mailers and jobs

- **The gate is what a class inherits or includes, never the `def` alone.** `perform` is an ordinary
  method name: across six corpora **hundreds of classes define a public `def perform` and are not a
  job** — service objects, protocol handlers, exporters — against a comparable number that are. A
  reader keyed on the `def` would have declared `perform_later` on every one of the former, where it
  raises.
- **Sidekiq is the majority of the job half.** `include Sidekiq::Worker` or `Sidekiq::Job` covers
  roughly twice as many classes over six corpora as ActiveJob does, with a further group subclassing
  one of those bases. Both spellings ship because `Sidekiq::Job` is what 7.0 renamed
  `Sidekiq::Worker` to.
- **`< ActionMailer::Base` is a spelling of its own, and a third of the corpus' mailers use it.**
  Testing "the superclass name ends `Mailer`" finds `ApplicationMailer`, `Devise::Mailer` and an
  engine's `BaseMailer` and misses every class subclassing the framework directly, including **every
  mailer in the largest corpus**. So the table is two: a suffix list and an exact list of the two
  framework bases.
- **The suffix is on the *superclass*, never on the class's own name.** A corpus' migrations are
  called `EnqueueValidateOpenaiHooksJob` and inherit `ActiveRecord::Migration[7.1]`.
- **A mailer action's parameters are the `def`'s and a job's entry points are `perform`'s, and that is
  the half that has to be exact.** An answer is partitioned by how many positional arguments the
  *call* wrote, so a `perform_later` declared `()` answers nothing for every call anybody makes. Every
  parameter shape Ruby has is rendered with every type `untyped`; keyword *names* are kept, because
  RBS needs them and arity counts positionals only.
- **A keyword parameter's name is sliced rather than matched.** Prism's keyword list holds exactly two
  node kinds and both spell the name the same way, so asking which would put a third arm in a file
  carrying a 100% branch floor. `syntax::keyword_name` takes the text up to the first `:` and is
  total.
- **A method name RBS cannot spell is declined.** `Synthesized::record` refuses a generated document
  it cannot parse **whole**, so one `def <=>` in a mailer would take that mailer's other eleven
  actions with it. An action Rails routes to is a plain identifier by construction, because it has to
  be a template's file name too.
- **`ActionMailer::MessageDelivery` is written by this crate — the first generated class whose *name*
  is somebody else's.** `Comment::Relation` is a name ya-lsp invented and nested under the user's own
  class; this one is real framework text, because a chain through `MessageDelivery` has to reach a
  class that exists. What makes it safe is the no-mapping rule: **nothing in it is mapped**, so when
  actionmailer *is* in the bundle the gem's own `def deliver_later` is the only place and the two
  declarations are one. Exactly one file writes it — the first mailer in URI order — and an
  application that declares the constant itself writes it instead.
- **A class that already writes the class method keeps its own, and the case is real.** An
  `UpdateArticleActivityWorker` writes its own `perform_async` inside a `class << self` to debounce
  the real one; declaring over the top added a second place and nothing else. **One class in six
  corpora**, and without the rule it is the only position in the whole sweep these conventions make
  worse.
  Both spellings are read, `def self.x` and `class << self`, because the corpus writes the second.
- **`Source::Convention` is a ninth rank, below the association.** The `def` is really in the file and
  Rails really installs the class method, so it is as direct as a macro — but what it names is a
  *framework* class this table chose. Every collision above it is unreachable in practice except rank
  1: a `sig` on a `def self.perform_later` wins.
- **What is deliberately not read.** A `module` that includes `Sidekiq::Worker` declares nothing,
  because the class methods land on whoever includes it. `set(wait: …)` is not declared: it returns an
  `ActiveJob::ConfiguredJob` that would have to be stubbed too, and its apparent call sites are
  mostly `Set` and `Hash` operations. And **one corpus' own job convention is out of scope on
  purpose**: hundreds of its classes subclass a `Jobs::Base` and implement `def execute`; reading it
  would be application knowledge, and it enqueues with `Jobs.enqueue(:job_name)`, a symbol, so the
  whole application has almost no `perform_*` call sites at all.
- **Resque is deliberately not a third provider, and the reason is not its zero.** Classic Resque
  costs **nothing, because it installs nothing**: `Resque.enqueue(Archive, id)` takes the class as an
  *argument*, so `Archive.perform` is the user's own `def self.perform` — already resolving, already a
  jump target. (Asserted from Resque's documented API, not read out of a checkout.) Its recognizer
  would also be a *measured* false-positive generator: the marker is `@queue = :name`, and **every**
  `@queue =` assignment in six corpora is an ordinary instance variable holding a `Thread::Queue`.
  Resque used the modern way is an ActiveJob **adapter**, and the job class is still
  `< ApplicationJob` with a `def perform`.
- **A `Job` suffix whose entry point is not `perform` declares nothing, right by accident rather than
  design.** `class MyJob < Que::Job` matches `INHERITS`, is recognised as `Convention::Job`, looks for
  `def perform`, finds Que's `def run`, and says nothing. Nothing in six corpora is shaped like this;
  a provider worth reading that spells its entry point differently needs a row saying which `def` it
  reads.
- **`Context::superclasses` is the projection these two conventions read**, asked one question,
  `rails::convention_of` — **the same function that answers it for the reader**: `synthesize` asks it
  of what the graph recorded to decide which documents to open, and `read_entrypoints` asks it of what
  Prism read to decide which classes to read. It is the one registered list filled by what a class
  *inherits*.

## The concern bodies

- **A concern's macros are declared on the module, and the `include` the user wrote carries them.**
  `a_concerns_macros_reach_every_class_that_includes_it` is the proof kept as a test. An RBS
  `module Storyish` and a Ruby `module Storyish` are one constant to rubydex, and
  `find_member_in_ancestors` — which `types::member`, `locator` and `completion` all look through —
  crosses it. **Nothing about resolution was added**, which is why the module route was worth
  proving: the alternative needs the includers, and this pass runs before `resolve`. A concern nested
  under the class it belongs to — `module Account::Interactions`, the dominant spelling — works the
  same, so a generated `module` may have a class in its own path.
- **Two hosts, not three, and each is a host only where it is the Ruby it claims to be.** `included
  do` is `ActiveSupport::Concern`'s and exists only on a **module**: **every** macro-bearing one in
  six corpora is in a module body, and one written in a `class` is a `NoMethodError`. `with_options`
  is a plain method on `Object` and hosts anywhere — **every** one in the corpus is in a class.
  Everything else is unchanged:
  the body of a `def`, an `if`, or any other block declares nothing.
- **`class_methods do` is not a macro host, twice over — and its `def`s are a different question,
  which `concerns.rs` answers.** It holds **no macro at all** in any of the six
  corpora — its calls are `private`, `delegate` and `attr_reader` — and one written there
  would be broken Ruby: `ActiveSupport::Concern` `module_eval`s the block on a nested `ClassMethods`
  module, and a plain `Module` has no `has_many`. Nor does it declare on the module's own singleton:
  those methods reach the includer through `base.extend ClassMethods`. The host test was never
  wrong; it was answering about macros while the `def`s went unread.
- **A concern's class methods are declared on the singleton of every class that includes it, and
  `locator` lost its walk for it.** `ActiveSupport::Concern#class_methods` builds — or reopens — a
  nested `ClassMethods` module and `module_eval`s the block on it, and `append_features` ends with
  `base.extend const_get(:ClassMethods)`. The edge used to be **walked** in `locator`, behind
  `const CLASS_METHODS`; it is now spent here, once per includer, so an ordinary ancestor walk finds
  the member and no Rails word is left in the module that answers `definition`.
- **The obvious shape — one module and one `extend` — is measured dead, and the measurement is the
  same one the query interface's cheap version failed.** A mixin arriving in a document indexed
  *after* its class was resolved is **never linearized**, and a generated document is always that
  shape. Re-checked across every spelling on 2026-09-15: RBS `extend M::CM`, Ruby `extend M::CM`,
  `class << self; include M; end` and `singleton_class.include M` all leave the member unreachable,
  while a generated `include` on the instance side and a generated `def self.` both resolve. It is a
  rubydex defect and `navigation.md` names the two code sites.
- **Three spellings, one list.** A `class_methods do` block; a hand-written `module ClassMethods`,
  which six corpora write in **17** files against the block's **120** calls — discourse 83,
  mastodon 21, forem 8, solidus 6, chatwoot 2, lobsters 0, holding **270** `def`s; and an
  `included do` holding a bare `extend M`, which puts `M`'s **instance** methods on the includer's
  singleton with no `ClassMethods` module anywhere in it. **The block speaks first**, because
  `module_eval` runs it on the module a hand-written one already declared, so a name written both
  ways is the block's — and `Facts` keeps the first of an equal-ranked pair.
- **The third spelling is the one worth the item, and it is finished against the graph.**
  `activemodel/lib/active_model/api.rb` is two such lines — `extend ActiveModel::Naming` and
  `extend ActiveModel::Translation` — reached by every model in all six applications through
  `ActiveRecord::Base`'s `include ActiveModel::API`, and they install `model_name` and
  `human_attribute_name`. The `def`s are in a *second* file the concern only names, which no
  predicate over documents could have selected in advance, so `Analysis::concern_declarations` asks
  the graph which document declares the module and hands that document's text back to
  `concerns::installed`. **Measured over solidus' 369 such cursors, 20 drawn on seed 6: 20 of 20
  guessed before, 0 of 20 after, every one landing in `active_model/translation.rb` or
  `naming.rb`.** On mastodon both jumps moved off a presenter and a shoulda-matchers test model onto
  `naming.rb`.
- **That one is keyed by the module's own file and not by the concern's**, which is what gives it a
  place: a span is an offset into the text the generator read, so writing it into `api.rb`'s
  document would point every jump at bytes of a file holding no such `def`.
- **A fan-out over the includers, which is where this parts company with what it replaced.** The old
  shape was one module, because `extend` would put it in front of every class Ruby does — *including
  the ones this pass cannot enumerate*. It can enumerate them: `Context::includers` resolves the
  project's `include` edges after the walk, transitively through concerns that include concerns, and
  a concern nothing includes now declares nothing, which is Ruby — `Tallyable.tally_by` raises.
  **Measured, the fan-out is small**: over each corpus' own code plus every gem its lockfile pins,
  537 (solidus), 608 (lobsters), 1,167 (discourse), 1,293 (chatwoot), 1,837 (mastodon) and 3,355
  (forem) `def self.` lines, because class inheritance carries a generated `def self.` down and
  Rails' own concerns are included into base classes.
- **`rails::CONCERNS` is the one projection a gem's `lib/` may join**, and it exists so that opening
  `rails::MODELS` to gems is not the price: `validates`, `scope`, `belongs_to` and `has_many` are all
  a Rails concern's class methods, and every one of them is written in a gem. The walk's gem path
  skips the `Definition::Method` arm outright, which is most of what a document holds — measured,
  settle is unmoved on lobsters (canary) and on chatwoot (1.8/1.8/1.9 s against 1.9/1.8/1.9 s).
- **A gem's names feed exactly one thing.** `Contribution::foreign` is separate from `declared`
  because `Context::classes` and `modules` are read as *the names this application declares* —
  `Elsewhere::known` gates a `delegated_type` on them, `Namespaces` decides what a generated name may
  be joined onto, `models_of` climbs them. What a gem's names are needed for is `includers_of`:
  `ActiveRecord::Base` includes `ActiveModel::API`, and resolving that edge needs both names and
  needs them for nothing else.
- **`Source::Convention`, for a mailer action's reason**: the `def` is really in the file and the
  class method is really installed, but the *type* is this table's and not the file's. It is also
  what gives the declaration a **place** — `at` is the `def` itself, so the jump lands on the line
  the user wrote rather than on a macro that implied it, however many classes it was written onto.
- **The one decline is the joined-name rule, and it moved with the owner.** It used to be asked of
  `<concern>::ClassMethods`; it is now asked of the **includer**, because `Owner::Singleton` opens
  `class <includer>` and a name with an undeclared namespace above it would introduce that namespace
  — RBS that does not parse, which `Synthesized::record` answers by refusing the *whole* document.
- **Three shapes of the call reach nothing, and one of them is the interesting one.** `class_methods`
  with no block, with a block passed as an argument (`&:sym`), and with an empty one all declare
  nothing. Written **in a class** it declares nothing either, and that is Ruby rather than caution:
  `class_methods` is defined on `ActiveSupport::Concern`, which is extended onto modules, so the
  call raises `NoMethodError`. Whether the call has a *receiver* is not asked in `concerns.rs` at
  all — `models::bare` has already asked it, and it is the one that can tell a `with_options`
  merger from somebody else's object.
- **Visibility is the file's own, read as `entrypoints.rs` reads a mailer's.** A bare `private` or
  `protected` closes the public section and `private :name` names one already written; the corpora
  write **19** bare ones inside these blocks. A `def self.` is declined — it is a singleton method
  of the `ClassMethods` module, which is the thing being extended rather than a thing extending, and
  six corpora write **zero**.
- **`with_options`' keywords are merged into the macros inside it, and the merge is load-bearing.**
  `with_options class_name: 'Account', optional: true do belongs_to :approved_by_account end` is a
  `belongs_to` naming `Account`; without the merge it names an `ApprovedByAccount` no application
  defines and declares **nothing**. Measured: **dozens of macros inherit at least one option they do
  not write**, almost all in one corpus, and what they inherit is mostly `class_name:` and
  `optional:`, never `polymorphic:`. One function does all of it: `inherited` asks the call and then each enclosing
  `with_options`, innermost first, and the call's own keyword wins, which is
  `ActiveSupport::OptionMerger`'s own `deep_merge` order.
- **Both `with_options` spellings ship, and the rare one is one `Option<String>`.** A block taking the
  option merger calls the macros on it (`do |t| t.has_many :x end`); a block taking nothing is
  `instance_eval`'d and so receiverless. Nearly every one in the corpus is the second. A receiver counts only when
  it is a `LocalVariableReadNode` naming *that* block's own parameter.
- **A `scope` in a concern is read and deliberately not written down here** — see *The class side of a
  concern*. A module therefore also asks `Model::collections()` for its elements' relations and never
  for one of its own, because a `Storyish::Relation` is a relation of a thing with no records.
- **An `enum` in a concern is declined**, and a `Status::Visibility` concern is exactly why: it
  declares `enum :visibility` inside `included do` and `statuses.visibility` is an `integer` column.
  That pair is resolved by telling the schema which `(class, attribute)` pairs an `enum` re-types,
  which it can only do for a class that claims a table.
- **The hosts nest, and the recursion reaches a third of the macros in the corpus that writes most
  of them.** Macros three hosts deep — `included do > with_options > with_options` — are common
  enough that a flat one-level walk misses a large minority of them. Counted the way
  `Models::collect` counts, the six corpora hold a couple of hundred macros inside a host, most
  declared here and the rest class-side, which the includer fan-out handles — and those fan out to
  several times their own number of declarations.
- **Swept over five applications: better in bulk, worse nowhere.** The corpus writing the most
  concern macros carries it, and its shape is **far more *precise* than *answered*** — most of what
  moved was already answering with a name-matched list and now answers with a type. The handful the
  harness calls regressions are a sweep artefact — every one is an `account.account_stat` whose
  concern writes both `has_one :account_stat` and a hand-written `def account_stat`, so the card
  gains a *"Defined in 2 places"* line and the classifier bins on the first footnote it
  recognises.
- **A `def` inside `included do` is deliberately not read.** The hosts change which *macros* count, not
  which constructs this pass understands.
- **`Context::modules` is for the includer fan-out, not the macro reader.** Whether a body is a
  `module` is a property of the file `read_model` is already reading. What needs the projection is
  "does this `include Storyish` name a concern *this application* defines".

## `enum` and `has_and_belongs_to_many`

- **What an `enum` installs is 3 + 4N names, every one read out of Rails.** `_enum` writes the
  attribute, its writer and `self.<name.pluralize>`; `define_enum_methods` writes `<label>?`,
  `<label>!`, `self.<label>` and `self.not_<label>` per value. The class-side pair really is
  `klass.scope`, so an `enum` feeds `Model::collections()` exactly as a `scope` does and reuses the
  relation class and model class side **unchanged**.
- **Both spellings ship.** `enum :status, { … }` is the majority of the corpus' calls; the older
  `enum status: { … }` is the rest, all in one application still on Rails 7.2, since Rails 8.1 deleted
  that form. One
  reader, two argument shapes: `values, options = options, {} unless values` is Rails' own line, so
  `enum :status, draft: 0` has **no options at all**, and the older form wears
  `_prefix`/`_suffix`/`_scopes`/`_instance_methods` and defines one enum per non-underscore pair. The
  last two are written **nowhere in the corpus** and are honoured anyway, because each is two lines
  and getting either wrong is a *wrong name*.
- **`enum` outranks the column.** `story.status` is the label — `EnumType#deserialize` answers with a
  key of the mapping, and the keys are strings — while the column is an `Integer`. `Source::Enum` is
  rank 2, `Source::Column` rank 3.
- **That rank is spent by the loser declining, because `Facts`' precedence is per document.** A column
  is declared into `db/schema.rb`'s generated document and an `enum` into the model's, and nothing
  merges the two — so rank alone would leave **two** `def status:` in the graph and a type that
  depends on document order. `synthesize` reads the model files *before* the schema, hands
  `Schema::signatures` the `(class, attribute)` pairs an `enum` names, and the schema says nothing
  about them. The one ordering in the pass that is a data dependency, and it is one-way.
- **The type is `String?` and the `?` is the column's.** An `enum` call cannot see whether the column
  is `null: false`, and the corpus is mixed — a third of the enum attributes whose column this could
  find are nullable and the rest are not. `class_of` unwraps an optional, so the `?` costs nothing at
  a member lookup; what it buys is that the weaker claim is made when the stronger cannot be checked.
- **A values list that is not a literal declares the attribute and none of its values.** Declaring
  nothing is wrong by one step: the three names an attribute owns do not depend on its values, and
  declining leaves the column's `Integer` in place. About one call in ten is like this.
- **A label this cannot write as a `def` is declined, and Rails does something else.** Rails installs
  `'ml-dsa-44': 2` under that exact name through `define_method` *and* under a transliterated alias.
  This declines both: `Synthesized::record` refuses RBS it cannot parse **as a whole document**, and
  honouring the alias would be right for only one of the two ways a label can be unspellable — the
  gsub is ASCII-only. The corpus contains exactly **one** label of either kind. Values beside a
  declined one are still declared.
- **An option that cannot be read is refused, and the two halves refuse differently.** A `prefix:` or
  `suffix:` this cannot read would name *every* value method wrongly, so it takes all of them; a
  `scopes:` or `instance_methods:` it cannot read might only mean there are none, so it is read as
  `false`. Both are written a couple of dozen times — counts a survey reads as almost zero if it
  looks for option names inside the braced *values* hash.
- **A value method never collides with the query interface, measured rather than assumed.** The one
  apparent case — an `enum :level, { item: 0, order: 1 }, suffix: true` — is not one, because
  the affixes are part of the name: it installs `order_level`, not `order`.
- **`has_and_belongs_to_many` is one row in `ASSOCIATIONS` and no new code path.** Rails' last line of
  the macro is `has_many name, scope, **hm_options, &extension`, so declaring it a collection *is* the
  framework's behaviour. **Its zero across all six corpora is a lint result rather than a language
  one** — every corpus runs `rubocop-rails`, whose `Rails/HasAndBelongsToMany` is on by default — and
  a zero may exclude a construct only when the construct also costs something. This one costs one row.
- **An `Association` carries the macro it was written as.** Five macro names reach four `Kind`s, so
  the provenance line cannot be recovered from the kind: a `has_and_belongs_to_many` is a `Kind::Many`
  and a line reading "`has_many :tags`" would name a macro the file does not contain.
- **Swept over four applications: better in bulk, worse nowhere** — and one corpus writes **no
  `enum` at all** while the others write it throughout, which is why a Rails feature must never be
  sized from one corpus. More positions again move between the two spellings of the *name* rung,
  which is no worse an answer and a truer index. The few the harness calls regressions are the
  harness: the classifier bins on the first footnote it recognises.
- **A macro written directly in a `module` body is written nowhere in six corpora, and that number
  was never the question.** A real concern writes its macros inside `included do`.
- **`MACROS` and `ASSOCIATIONS` are one list written twice, and a test is the seam.** Two names are on
  the first and not the second — `enum`, which names a column's values, and `delegate`, which names a
  *method*. `every_macro_is_read_by_exactly_one_reader` spells the two exemptions out rather than
  allowing any difference.

## The generator seam

- **A generator returns a table of facts, not a string, and the table is where two of them colliding
  is decided.** `Facts` holds one `Declared` per `(owner, name)`; `Facts::render` spells all of it as
  RBS *once*, at the end, so no generator's spans are ever shifted by another's length. No incremental
  append and no offset arithmetic outside `render`.
- **One member, one declaration, decided by `Source`'s rank.** Two `def status:` lines in one RBS
  document is legal RBS and produces an overload set that is wrong in a way nothing reports — reached
  in any Rails application by a column named `status` and an `enum :status`. The order is annotation,
  `enum`, column, association, `attribute`, the derived long tail, `delegate` — **less derived wins**
  — with two rules beside it: at equal rank **a typed declaration beats an `untyped` one**, and at
  equal rank and typedness **the first to speak keeps the position**, which is what makes a schema read
  twice render the same document.
- **`Source::Interface` is the lowest rank and the only collision ever reachable.** Every rank above
  it is a thing a *file* says; the query interface is the one thing no file says at all. `scope :first`
  is the case, and it measures **zero** across all six corpora.
- **The user's own `def` is not in this table.** It is a real definition in rubydex's graph and `hover`
  counts it as a second place; a generated declaration does not overwrite it. Precedence here is
  between *generators*.
- **`Owner::Module` is what makes a concern's macros and the route-helper module possible.** Being able
  to *spell* a module is not the same as knowing an RBS `module` reaches its includers, which is proved
  by a test.
- **The pass names no body of knowledge, and a build with none registered still runs.**
  `src/knowledge/` is the contract: a module registers its `ListId`s and `Wants` rows, answers its
  own feature switch, folds its own projection out of what the one walk saw, keeps its own parse
  memo, and declares in three phases — *conjure*, *declare*, *derive* — plus two hooks for what
  only the whole walk decides (`after_the_walk`, before the feature gate; `after_the_bundle`, once
  the bundle has answered). `grep -c 'rails::' src/analysis/synthesize.rs` is **0**.
- **`workspace/rails/` did not move and must not.** Its property is *pure text in, text out, no
  I/O and no graph*, and every file of it is at 100:100. What moved is the orchestration, to
  `knowledge/rails.rs`, which may not open a file or read the graph either: it is handed closures
  for a document's text, its caption, whether it is the user's own, and the one graph question a
  generator may ask — which document declares each of these module names, asked by *name* because
  the pass runs before the resolve.
- **Three things a module owns that are not declarations.** `discover`, for files nothing indexed
  and no list can name (`db/*structure.sql`); `places_members_on`, for the step after the resolve,
  because the query interface's place is a `def` in a gem; and `views`, because what a template's
  implicit receiver can answer is an output of a body of knowledge that is not a `Facts`.
- **`generated::candidates` was misfiled rather than coupled.** It is Ruby's own lexical constant
  lookup, which Rails modelled `compute_type` on; it sits beside `is_constant_path` now, where
  both `workspace/rails/` and `knowledge/` reach it without either depending on the other.
- **`Facts::split` cuts the finished table into one `Facts` per body, and it runs after every
  generator and after precedence.** The partition is of the *rendering* and never of the generation:
  nothing it moves can re-decide a rank, and `declared`, `returns` and `source` are still asked of
  the whole file's facts before it. Two generators that name one member collide on `(owner, name)`
  and the owner picks the part, so a collision cannot cross one; every field of `Facts` is keyed by
  an `Owner`, so the partition is total. It is what makes a generated document the unit of
  invalidation rather than a file — see *The document is a body* below for the measurement.
- **A generator may still return one `Facts`, and that is deliberate.** The split is downstream of
  the merge, so no reader in `workspace/rails/` learns that a document can be cut in two, and a file
  that feeds two generators still merges into one table before anything is partitioned.
- **Phase two is a query and not a loop.** `Facts::returns` answers `Story#user -> User` then
  `User#name -> String`, both written in the same pass into other files' documents and neither rendered
  nor indexed, and the two-hop is tested in both merge orders. There is **no phase-two loop**: the
  union costs a merge per settle and only `delegate` asks for one.
- **One walk of the graph fills every list, and a generator names a list rather than writing a loop.**
  the row table is nine rows over seven predicates — "calls one of these receiverless names", "the path ends
  with this", "a `@return`/`@param` tag sits above a `def`". A document matching two of a row's tests
  is pushed **once**: a file with a `sig` *and* a YARD tag would otherwise be generated twice.
- **Every projection `Context` carries is filled by the one walk**, because a projection added later is
  a second walk over every document. `superclasses` is what recognises a mailer and a job.
- **A superclass is spelled as *written*, and that needs a second name walk.** `qualified_name` answers
  "which class is this" and adds the lexical nesting; `spelled_name` answers "what does this line say"
  and must not — `class Story < ApplicationRecord` inside `module Admin` names `ApplicationRecord`. A
  reference is not a definition and cannot borrow a definition's nesting.
- **`workspace/rails/` is a directory and `mod.rs` is its only public surface.** "How much framework is
  in here" is answered by reading one list: the convention tables are `const` arrays in `mod.rs` and
  every submodule is private. Every file carries a 100% coverage floor, listed one path at a time
  because a floor whose path is stale passes forever while measuring nothing.
- **The pass itself left `analysis/mod.rs` for `analysis/synthesize.rs`.** Privacy is by module
  *descendant*, so a child of `analysis` still sees `Analysis`' private fields.

## What `db/*schema.rb` declares

- **The reader is pure: a `schema.rb` string in, an RBS string out.** No `Graph`, no harness, no
  threads, no `tempfile`. `rails::read_schema(source)` parses, and
  `Schema::signatures(file, classes, enums)` takes a `table -> class` map plus the columns an `enum`
  re-types, and returns the facts and one span per declaration. That purity is what a 100% coverage
  floor is affordable against.
- **There is more than one schema, and assuming otherwise is silent.** Rails has supported several
  databases since 6.0: the primary dump is `db/schema.rb` and every other is `db/<database>_schema.rb`.
  A new Rails 8 application ships three of the second kind, and real applications ship several.
  `rails::is_schema` is a path convention: the name is `schema.rb` or `*_schema.rb`, and the file sits
  **directly** in a directory called `db`. Out of reach and therefore out of scope: `schema_dump:` in
  `database.yml` and `ENV["SCHEMA"]`.
- **The file name is a parameter of the generator, not a constant in it.** A card reading
  `db/schema.rb` above a column really in `db/animals_schema.rb` is exactly the confidently-wrong
  answer these readers exist to avoid.
- **A table two schemas declare is declared by neither** — the same rule as a table two classes claim,
  and one degree worse: the model would answer with two schemas at once and `Types::harvest` would keep
  whichever column it read last. This is why every schema is **parsed before any of them writes a
  line**; `Schema::table_names` exists so the caller can find out.
- **The direction is class → table, and it is the whole safety argument.** Every table is looked up
  *from* a class that exists, by pluralizing its name. Both directions need the same irregular rules;
  only this one fails toward *nothing*. Three bounds on what may claim a table: the user's own code
  only, **top-level classes only**, **one claimant**.
- **A namespaced class claims nothing, because its table depends on `table_name_prefix`** — see *The
  table a nested model claims*. `self.table_name = "..."` is the escape and it **also removes** the
  claim the class's own name made, or one model would answer with two schemas at once.
- **The `table_name=` override costs what it is worth and nothing when absent.** The files to read are
  found by filtering own documents' method references against `StringId::from("table_name=")`, a pure
  hash; only a file that really renames its table is read and parsed. Some applications have none at all.
- **The type map is ten entries because ten is what the corpus contains**, and anything else is
  `untyped` rather than skipped. A mapping is a claim; `untyped` is the absence of one, and it still
  declares the member. `untyped` is never made optional: it already includes `nil`, and `untyped?` is
  not RBS. Two of the ten carry an argument: `datetime` is **`Time`** and not
  `ActiveSupport::TimeWithZone`, because the latter delegates to the former and because `Time` is in
  the graph of a project with no Rails; `binary` is `String`, which is what ActiveRecord hands back.
- **`null: false` is the feature nothing else offers, and the provenance line is where it is seen.** A
  hover card shows a declaration's *name*, not its RBS return type, so `String?` would otherwise be a
  fact nobody is shown. The generated comment above each `def` carries the file, the table, the column,
  its schema type and whether it may be `nil`, and reaches the card as documentation the way RDoc
  above a `def` does. **That is why no module outside `workspace/rails/` has to learn the word
  "table".**
- **The primary key is read, because `story.id` is a member lookup that fails.** `create_table`
  declares an `id` unless `id: false`; `primary_key:` renames it, `id: :uuid` retypes it, and a
  composite `primary_key: ["a", "b"]` declares none. It maps to the `create_table` line, because the
  column's name is nowhere in the file.
- **A column name is validated as text before it is written.** Snake case only — not a keyword rule,
  since RBS takes `def type:` and `def class:` (measured, not assumed), but because this crate is about
  to write the name into a file it then parses, and one column called `foo bar` would take its whole
  table down.
- **The pass prunes what it owns, and it remembers that itself rather than asking the table.** A source
  that stops declaring anything sends no notification, so the pass that writes has to be the pass that
  prunes. `Analysis::generated` is the set of sources `synthesize` wrote last time, and `forget_stale`
  drops the ones it did not write this time. A `retain` on the table is wrong, because the side table
  is **shared**.
- **Generation runs before every resolve, from two call sites, both `self.synthesize()` immediately
  above `self.resolve()`.** Not on a file-watch event, because the *other* input is which classes exist:
  `rails generate model` writes a file with nothing to do with any schema's mtime. Of an
  application's several schema files typically **only one declares anything**, and all three
  generators together cost less than the resolve they run in front of.
- **Not indexed and not read are the same sentence.** A project whose `index.include` excludes `db/`
  has said what it wants looked at. The file list comes from `graph.documents()` rather than a
  `read_dir`, which makes that gate free; the URI *string* is tested with one `ends_with` before it is
  parsed, because `DocUri::from_uri_str` parses a URL and there are eight thousand documents in a
  bundle to say no to.

## What `db/*structure.sql` declares

- **It is a scanner and it must not become a parser.** A general SQL parser has to understand the
  *whole file*; a real one holds `CREATE FUNCTION` bodies in plpgsql, triggers, a view, two
  enum types and four extensions, so a parser choking on any one loses **every** table. A scanner skips
  what it does not recognise. "It needs a Postgres DDL parser" is the wrong reading of the problem.
- **"Top level" is the whole of the machinery.** `Scan::hidden` knows the six things that hide text —
  `'…'`, `"…"`, `` `…` ``, `$tag$…$tag$`, `-- …`, `/* … */` — and a `CREATE TABLE` is only one when it
  is outside all of them. Without that, a plpgsql body mentioning the words, a mysqldump's
  `/*!50001 CREATE VIEW …*/` and a default value's string literal are all tables. Unterminated runs to
  the end of the file, which is the safe direction.
- **The reader ends at `schema.rs`'s `Table` and `Column`, and that is the seam.** `read_structure`
  builds them and `Schema::from_tables` wraps them; `Schema::signatures`, `rbs_type`, the provenance
  line, the `retyped` withdrawal, the ambiguity rule and everything in `analysis/synthesize.rs` are
  shared rather than mirrored. Only one of the two readers decides what a `string` returns.
- **The type table maps a dump's word onto `schema.rb`'s word, not onto a Ruby class.** Rails has no
  such table — `NATIVE_DATABASE_TYPES` is what it *asks* a server for, and pg's read-direction map is
  keyed on catalog names (`int4`, `float8`, `varchar`) where pg_dump writes the standard spellings — so
  the 48 rows are written here. They stop at `string`, `bigint`, `datetime`, and `COLUMN_TYPES`' ten
  does the rest. So `uuid`, `citext` and `time` are `untyped` here because they are `untyped` there,
  and if `COLUMN_TYPES` grows a row both readers gain it at once.
- **A spelling not in the table passes through under its own name.** It lands on `untyped` exactly as
  `t.jsonb` does, and the provenance line quotes what the file said — so a `halfvec` column is a member
  with no type and a card naming the word. Dropping it would be a member that does not exist.
- **One word cannot be shared, and it is `KEY`.** Postgres does not reserve `key`, so pg_dump writes
  `key character varying(50) NOT NULL` bare — a **column**, in several tables of a real dump. MySQL does
  reserve it, so mysqldump writes a column as `` `key` `` and an index as bare `KEY name (cols)`. Same
  token, unquoted, opposite meanings. The dialect is detected once per file from `/*!` or `ENGINE=` and
  decides exactly that.
- **A quoted identifier is always a column, and that rule needs no dialect.** A dumper quotes exactly
  what its own dialect reserves. The first version uppercased the name before it looked at the quotes
  and lost real columns on a Postgres dump.
- **The three dialects must produce the same RBS.** `solid_cache` ships one three-table schema dumped
  by all three adapters — a controlled experiment nobody had to build — and **one type name in six is
  spelled the same in all three**. All three declare the same eleven members returning the same types,
  including `t.binary` as `bytea`, `varbinary(1024)`/`longblob` and `blob(1024)`/`blob(536870912)`. The
  one thing they cannot agree on is the *provenance word*: SQLite has one integer type, so a `bigint`
  is an `integer` there. It costs nothing — both are `Integer` — and both facts are pinned as tests.
- **The scanner needs no primary-key rule, a real simplification.** `create_table` declares an `id`
  without writing one down, which is why `schema.rs` has `primary_key`; every dumper writes the column
  out, so here it is an ordinary column and `id: :uuid`, `primary_key:` and a composite key come free.
- **A table name with a line break in it is declined, and nothing narrower.** The name goes into the
  provenance comment, and a second line there is RBS that does not parse. Nothing narrower is *wanted*:
  a legacy `structure.sql` is exactly where a table called `OldTable` lives, and `self.table_name =` can
  claim it. Column names go through `schema.rs`'s own text rule unchanged.
- **A table two schema *sources* declare is declared by neither**, the same rule two `.rb` schemas get,
  reached by a second road: a repository that switched formats and did not delete the old file has two
  sources for one table.

### The plumbing

- **`capabilities::watched_files` registers two constants, and neither is indexed.**
  `db/*structure.sql` sits beside `ya-lsp.toml`, which is watched and never indexed. It is a
  **constant** and not derived from `index.include`, because the registration is made once at
  `initialize` and never again. `db/*structure.sql` rather than `**/*.sql`, which would sweep up every
  fixture, seed and migration.
- **`Analysis::refresh` has three outcomes and not two.** Re-index a document, forget one, or
  **neither**. A `.sql` is the third: read by the pass and never reaching rubydex, which would index
  SQL as Ruby. That holds on every route the server takes by itself, and it is what `make canary`'s
  file count rests on. A client whose document selector hands a `.sql` over on `didOpen` still indexes
  it as a buffer; the extension's `LANGUAGES` is `ruby` and `erb`, so no shipped client does. The
  branch sits **below** the open-buffer short-circuit and **above** both the `is_file` test and the
  `Workspace::indexes` gate.
- **`Context::dumps` is a path under the root, not a URI out of the graph.** A `.sql` is not a
  document, so no registered row can reach one. It is one `read_dir` of `<root>/db` per settle — the same
  rule the watcher registers, so what is watched and what is read cannot disagree. Bounded to that one
  directory: a dump found deeper would be read and would then never refresh.
- **`Context::is_empty` counts the dumps, and leaving that out would be silent.** An application with a
  `structure.sql`, models that write no macro and no routes file has nothing on any of the six lists,
  and the pass would return before reading it.
- **Deliberately not cached.** What a cache would have to be invalidated by is a file *appearing*.
- **`Workspace::admits` is the one switch, and it is `indexes` without the include half.** "Not indexed
  and not read are the same sentence" cannot be the gate for a file `index.include` can never name —
  but `index.exclude`, `.gitignore` and the hidden-file rule are things the user said, so this is the
  same compiled globs and the same walker. It is strictly wider than `indexes`, asserted over every
  file in the fixture tree.

### What the SQL reader costs and what it found

- **Checked against a live Rails.** One corpus runs `annotaterb`, so its model files carry
  `# == Schema Information` blocks a real Rails wrote against the real database. Over every table both
  sides name: **no column missed, none invented**, nearly every column set matching exactly, **no
  nullability disagreement at all**, and a couple of type disagreements — both Postgres enums, landing
  on `untyped` either way. The columns the annotation lacks are the annotation lagging.
- **The scanner is far cheaper than the parser.** Over the largest dump in the corpus it reads
  hundreds of tables and thousands of columns in a few milliseconds; Prism parsing a `schema.rb` a
  fraction of the size costs comparably, because there is no AST here. Almost every column gets a real
  Ruby type; the rest are `json`/`jsonb`/`inet`/`tsvector`, where Rails' own answer is a `Hash` or an
  `Array`, and a **genuinely unknown** handful — a pgvector `halfvec`, a Postgres enum, an
  `int4range`.
- **Swept over the largest corpus, and the number is a *tier*.** The *answered* total rises, *precise*
  rises far more, and the *list* tier falls — so **most of the value is in the answers it replaces**:
  over half the up-moves are a name-matched list becoming a typed answer. Over the positions asked
  once the bundle was in, **better in bulk and worse nowhere**.
- **Two artefacts look like regressions in a sweep of this size and neither is one.** A tranche of
  positions "moved to nothing" and every one is in the **first decile of the ask order**. More "moved
  to list" because `sweep.py`'s `tier()` tests for `Defined in ` before the footnotes. Behind both
  sits a defect in the harness: a `settle()` that reads a cold server's silence as the end of its
  work, so every shard reported `settled after 0.0s`.
- **`array: true` must be read, and by both readers.** Unread, a Postgres array column answers its
  *element* type — `t.string "languages", array: true` returning `String` — a wrong answer rather than
  an absent one. **Half the corpora write such a column in `schema.rb` alone.** `Column::array` is
  read by both (`array: true` on one side, `integer[]` on the other), `rbs_type` wraps rather than
  replaces, and it wraps an unmapped element too: `Array[untyped]` says the one true thing.

## What the model macros declare

- **Four association macros, and the list is a measurement rather than a taste.** `belongs_to`,
  `has_one`, `has_many`, `scope`. `validates` is the most common macro in a real model by half again
  — more calls than every association macro — and declares no member, type or target.
- **`class_name:` is a correctness requirement, not a refinement.** Well under half of a real
  application's singular associations are a bare name. `belongs_to :parent_comment, class_name:
  "Comment"` camelizes to a `ParentComment` no application has, so a reader that skips the option is
  wrong at dozens of macros in one application.
- **The bound on what may be named is the classes the application itself defines.** A gem that happens
  to define `class Story` is not this application's model. `Declaring::classes` is that set, spelled
  with nesting by walking `Name::parent_scope` and `Name::nesting`, so `class_name: "Admin::Setting"`
  matches exactly what rubydex calls the same class.
- **`polymorphic: true` emits nothing, and `optional: true` is what makes a `belongs_to` nilable.**
  Rails 5 made `belongs_to` non-`nil` by default, so the option's presence is the fact, and about a
  third of real singular associations carry it. `has_one` is optional whatever anyone writes, because nothing in the file says the other
  record exists.
- **A `has_many :through` is believed only when its intermediate is on the same class**, and `source:`
  is read because it is the only thing in the call that says what
  `has_many :voters, through: :votes, source: :user` collects.
- **What one association line installs is Rails' own list**, read out of
  `activerecord/lib/active_record/associations/builder/`. `Association::define_readers`/`define_writers`
  write the pair every macro gets; `SingularAssociation::define_accessors` adds `build_x`, `create_x`,
  `create_x!`, `reload_x`, `reset_x`; `CollectionAssociation` adds the `_ids` pair; and
  `BelongsTo::define_change_tracking_methods` adds `x_changed?` and `x_previously_changed?`.
  `associations.rb`'s own "Auto-generated methods" table is the same list written for a reader. Two
  rows contradict what reading the macro names would suggest: `_changed?` is `belongs_to`'s **alone**,
  and the constructor gate is `unless reflection.polymorphic?` and **not** the older
  `unless constructable?`, which had excluded `has_one through:`. Each carries the macro's own span, so
  `story.create_user` and `story.comment_ids` both jump to the `belongs_to`/`has_many`.
- **An association installs thirteen names; nine are declared and four declined, on a measurement.** A
  call site counts only where it has an explicit receiver or is inside `app/models/**`. Over six
  corpora the writers, the `_ids` pair and the constructors run to **thousands of call sites between
  them**, while `reload_<name>`, `reset_<name>`, `<name>_changed?` and `<name>_previously_changed?`
  are written **barely at all**. A handful of sites in six applications is the wrong side of the
  trade; what ships pays for its declarations many times over.
- **The singular writer is nilable whatever the reader is, and the constructors are not nilable where
  the reader is.** `belongs_to :user` reads a `User` because Rails 5 made the association required, and
  `story.user = nil` is still ordinary Ruby — the validation fails at save — so the writer is
  `(User?) -> User?`. `belongs_to :user, optional: true` reads a `User?` and `create_user` *makes* one,
  so it is a `User`. Assigning a subclass hands the subclass back, which makes the writer's declared
  type a supertype of every value the call can return.
- **A collection writer has no type to state and its `_ids` reader has one it cannot reach.**
  `story.comments = [a, b]` returns an `Array` and `story.comments = other.comments` returns a
  relation, so the writer is `(untyped) -> untyped`. `ids_reader` is `pluck(primary_key)`, and what a
  primary key holds is the **schema's** to say, in a different generated document
  `Association::declare` cannot ask: `Integer` is right for a `bigint` and wrong for every `id: :uuid`
  table. `Array[untyped]` is what is known and what the call sites want.
- **Rails singularizes the association's own name, never the class it resolves to.**
  `has_many :authors, class_name: "User"` installs `author_ids`, and
  `has_many :tags, through: :taggings` installs `tag_ids`. `CollectionAssociation::define_readers`
  reads `name.to_s.singularize`, so this reader reads `self.name` and never `target`.
- **The `def`-shadows-a-macro rule is deliberately not asked here.** A `def` in the same class body
  really overrides what an association's generated module installs, so a model writing `def comments`
  gains a *second place* rather than a wrong answer. `tail.rs`' rule is that a macro keeps its
  declaration when it carries a **type** an unannotated `def` does not, which an association does.
  Measured over six corpora, only a couple of dozen models write a `def` shadowing a name their macro
  installs. The two weakest rows are awkward, because `comments=` returns `untyped` and
  `Types::harvest` drops it; a handful of positions in six applications is not worth a rule.
- **The raw count of `create_<association>` overstates the honest one roughly eightfold.**
  `create_post` in a spec is a *fabrication helper*, and one corpus is most of the difference. A demand
  number for a name Rails **derives** has to be gated on the shape of the call. `<name>=` is the same
  trap an order of magnitude worse, and `<singular>_ids` nearly threefold. An instrument that does not
  strip comments or exclude `create_user:` gets the `create_` pair wrong in the other direction.
- **A polymorphic `belongs_to` declares nothing at all — not the reader, not the writer.** Rails
  installs both. This reader declines the whole macro because `resolved` finds no class, which is a
  *decline of the name on the strength of the type* and is the opposite of what `delegate` and
  `attribute` do. It is the one row left open rather than argued; the demand is a few dozen calls over
  six corpora.
- **A column and an association can name the same member and Rails says the association wins** — "the
  association methods module is included immediately after the generated attributes methods module".
  `Source::Column` is ranked **above** `Source::Association`, the other way round. Left alone because
  it is unreachable: over every corpus with a `db/schema.rb`, checking every name each association
  installs against the columns of the table its own class claims, there is **not one clash** at any
  model whose name lands on a table that exists. A rank change with no measurable effect is
  churn, and the argument the rank was given — the column is what the database *is* — was made
  deliberately.
- **Only statements of a class body are macros.** The body of `included do`, of a `with_options` block,
  of a `def` and of an `if` are Ruby that only runs — except the two hosts named above.
  `Models::associations` iterates a `StatementsNode`'s own children and never descends.
- **A class *is* required to be a plausible host, and a superclass test alone is the wrong one** — see
  *What may host an association macro*.
- **`singularize` is the direction the schema reader refuses, admitted here because there is no other.**
  A `has_many :comments` says its element type in the plural and nowhere else. What makes it safe is
  that a name it gets wrong is a class the application does not define, and such a class declares
  nothing.
- **The relation class is monomorphic, and everything about collections rests on that.** ya-lsp writes
  this RBS, so it never has to write a type parameter: `has_many :comments` returns a
  `Comment::Relation` whose members are already specialised. Generic instantiation would cost 61
  methods, 30 of which need argument inference this crate has no path to.
- **`Comment::Relation` is the name, nested under the model on purpose.** It cannot collide with an
  unrelated top-level constant; it reads correctly on a card saying `Comment::Relation#first`; and a
  project that already has a `Comment::Relation` is exactly the project that meant something by it — so
  a collision makes the item emit **nothing at all** for that element type, and the collection loses
  its type rather than the user losing their class.
- **One relation class per element type, not one per association.** However many collection macros a
  workspace writes, it costs at most one class per model. Which file writes a shared one is the caller's decision — the first
  source in URI order that asked — and that is allowed to be arbitrary *because* nothing in a relation
  class is a place.
- **Nothing in the query interface is mapped, and that is not a shortcut.** No line of anybody's code
  declares `Comment::Relation#first`. A relation shared by four `has_many :comments` could be pointed
  at one of them, and pointing at an arbitrary one of four is the confidently-wrong answer these
  readers exist to avoid. `Declared::at` is an `Option` for exactly this. **A `scope` is the
  exception and the only one**: a file really does declare `Comment::Relation#recent`, on the same
  line it declares `Comment.recent`, so both carry that span.
- **The members are the ones that can be typed without a type parameter, and no others.** `first`,
  `last`, `find`, `find_by`, `to_a`, `each`, and the `where`/`order`/`limit`/`includes` group. `map`,
  `select` and `pluck` are **absent rather than wrong**: their element type is a block's return, which
  is an inference engine and not a substitution.
- **The query interface is written once and declared twice.** `rails::query_interface` returns the
  signatures without their `def`; `relation` writes them as instance methods on `Comment::Relation` and
  `class_side` writes the same ones as `def self.` on `Comment`. `ActiveRecord::Querying` really does
  delegate from the class to `all`, so these are one fact — and generating both from one list makes
  "`Story.where` and `Story.all.where` cannot disagree" something the code enforces. **A name may be
  added only if it can be typed on both sides at once.**
- **The class side goes on classes that own a relation, and nothing wider.** One some `has_many`
  collects, or one a `scope` is written in — both statements a file makes. A model with neither macro
  gains nothing here *even where `db/schema.rb` knows its table*, because the interface hands back a
  `Story::Relation` and nothing would have written one. Widening to "every class the schema claims a
  table for" means generating a relation class per table, which is a different item.
- **Nothing on the class side is mapped either, and the reason is stronger than the relation's.**
  `Story.where` could be pointed nowhere at all.
- **The provenance is per declaration on the class side and per class on the relation.**
  `Comment::Relation` is a class ya-lsp invented, so one comment on the class covers everything in it.
  `class Story` is the *user's own* class, and a note attached to it would read as a claim about their
  file.
- **A `scope` is a class method and its lambda is never read.** The name and the class it is written in
  are enough, because a scope returns a relation of its own class whatever the body does. rubydex files
  `def self.recent` on the singleton exactly as it resolves `Story.recent` there.
- **And it is a relation method too, which is the half that makes a chain work.** Rails delegates
  every scope to the relation — `ActiveRecord::Delegation` builds a module per relation class holding
  them — which is what makes `Story.recent.visible.limit(10)` the ordinary spelling of a query rather
  than a clever one. On the class object alone the **first** hop resolves and every hop after it falls
  to the name-based list: a chain that gets worse the further the code has already got. So one macro
  line is two declarations, `Owner::Singleton(Story)` and `Owner::Instance(Story::Relation)`, carrying
  one span. The `enum`'s class-side pair is a `scope` Rails writes itself and takes the same two.
- **The relation copies are held to the end of the document and sorted by owner.** `Facts` renders in
  the order it was told things and reopens a body every time the owner changes, so declaring the two
  halves side by side writes one `class Story::Relation ... end` per scope. One run per relation class
  costs one body and says the same thing; the sort is stable, so within a class the order is still the
  order the macros were written in, which is what the collision rule reads.
- **A document may write members onto a relation class another document opened.** A concern's `scope`
  lives in the concern's document and the includer's relation class is written wherever it was first
  asked for — so `Poll::Relation` is opened with its superclass in one generated document and reopened
  with a member in another. The two merge exactly as a reopened Ruby class does: measured, the query
  interface still answers after the second scope. This is the one shape where a member and the class
  it hangs on are written by two generators that never see each other, and there is a test for it.
- **Measured over a real application, the macros declare more members than the schema does columns.**
  At every `.member` position outside the schemas, **positions are gained and none is lost** — and the
  result that matters more: **many positions move from a guessed candidate list to a precise answer,
  and none moves the other way**, one association name accounting for a large share on its own.
- **What the class side buys is a tier and not a count.** Over the same positions the *answered* total
  does not move at all, and **many positions change tier, none for the worse**: some to a card with no
  footnote, some from a name-based list or single name match to *derived*, and some from *guessed* to
  *derived*. Over half sit on one of the interface names — `where` most of all — and the rest are
  further along a chain one of them started. Two of them, `limit` and `includes`, are worth barely
  anything on this corpus and **stay anyway**, because the list is bounded by what the relation
  declares and not by what a corpus rewards.

## What a `sig` block or a YARD tag declares

- **The only generator that reads a *claim*.** `db/schema.rb` is what the database is; `belongs_to
  :user` is what ActiveRecord will do. A `sig` and a `@return` are what somebody believed, and nothing
  checks either unless a type checker is run. Both are **derived**, never resolved.
- **Sorbet wins where both are present.** A `sig` is Ruby the parser validates and `srb` checks, and it
  rots when the method changes. A comment rots quietly.
- **Nothing an annotation declares is a place, and here that is forced rather than chosen.** The method
  already exists in the graph at the offset an editor should jump to, so what this adds is a *type*. A
  span would put the same location in a go-to-definition list twice.
- **Because of that, `hover` had to learn the difference between a definition and a place.** A card's
  "Defined in N places" counts definitions that have a `Site`; and prose from a *generated* definition
  is rendered as a footnote rather than the body, because a footnote is already "what ya-lsp knows
  about the answer". That is what lets the card say which annotation was read while still showing the
  user's own RDoc above it.
- **Arity is the half that has to be exact.** A generated signature claiming the wrong arity answers
  nothing, or answers for a call nobody made. Every parameter shape Ruby has is rendered, and
  `def go(...)` — which Prism files under `keyword_rest` — falls back to `(*untyped)`, variadic and so
  right for every arity.
- **Positional parameter *names* are dropped, a risk trade.** They buy a nicer signature-help line; a
  parameter called `type` or `class` is an RBS keyword that would take the whole file's declarations
  down. Keyword names are unavoidable and are kept.
- **A type this cannot spell exactly is a method it says nothing about.** `T.any`, `T.all`, `T.proc`,
  `T.self_type`, `T.type_parameter`, a `T::` constant that is not one of six generics; duck types
  (`[#read]`), `Hash{Symbol=>String}`, and any union that is not `[Foo, nil]`. `Types` keys an answer
  by one declaration, so `String | Integer` has no representation that is not a lie.
- **`Synthesized::record` refuses RBS that does not parse, and says so.** The two consumers fail
  independently and neither complains: `Types::harvest` returns on a parse error and rubydex indexes
  what it can. One file's declarations is the right blast radius for a generator bug, and a warning
  naming the file is what makes it findable — which matters most here, this being the generator whose
  input is least regular.
- **The main corpus has neither, and that is the measurement.** It reports **no annotated methods at
  all**: the corpus every other number in this file comes from cannot show this reader working, so it
  is measured against a second repository.

## The callbacks

- **`before_create` is a `def` in activesupport that `define_model_callbacks` wrote at boot**, so no
  file in the workspace declares it, the graph correctly found nothing, and the name rung answered with
  the only `before_create` anybody's file *does* write:
  `Fabrication::Schematic::Evaluator#before_create`, in a gem, in a fixture library.
- **The four groups are Rails' own four call sites, walked rather than remembered**, which makes this
  **twenty-three** names rather than the thirty a `before`/`around`/`after` × ten events product rule
  would give. Ruby raises on each of the seven that rule invents: `initialize`, `find` and `touch` are
  declared `only: :after`; `ActiveModel::Validations::Callbacks` writes `before_validation` and
  `after_validation` by hand and there is no `around_validation`; and `commit` and `rollback` are not
  `define_model_callbacks` calls at all — `ActiveRecord::Transactions` writes six `def`s.
- **Nothing is mapped**, which is why this cannot make a jump worse: what it takes away is a jump
  *into a gem that has nothing to do with the file*.
- **An abstract class gets these and keeps them**, the one place this parts company with `class_side`:
  `ApplicationRecord.first` raises in Ruby and `ApplicationRecord.before_save` does not. The
  inheritance defect that gate exists for cannot arise here — every subclass is declared its own, and
  `-> void` has no type to propagate wrongly.
- The block is optional and is handed the **record**, which is `ActiveSupport::Callbacks`' own
  behaviour for a proc that takes an argument, so `before_save { |story| … }` types `story`.
- **It replaces a wrong answer, so a tier sweep understates it.** The demand is **over a thousand
  callback call sites across six corpora**, present in every one of them, and the direct evidence is
  one coordinate: a `before_create` in a model going from a fixture gem's
  `Fabrication::Schematic::Evaluator#before_create` to the model's own. Swept with the two lookup
  fixes beside it: **better in bulk and worse nowhere**. A *floor*, because the corpora's bundles are
  partly installed.

## Why the pass does not run on every keystroke

- **`settle` calls this pass before every `resolve`, and a forced settle sits in front of every
  graph-reading request** — so without a gate one keystroke in a file with no macro pays for a
  whole-workspace regeneration.
- **Two questions and both have to be no.** The `Context` is what the generators are handed, so an
  equal one means equal arguments — and that half is what makes a naive "did *this* document declare
  anything" test unsound: `has_many :widgets` declines until some *other* file writes `class Widget`.
  The second half is the files themselves, because a `Context` says which documents a generator opens
  and never what is in them.
- **The gate looks at the disk rather than trusting a notification**, which the suite caught it not
  doing: this pass claims the answer is a function of what is on disk and earns that by re-reading
  everything, so a `git checkout` that deletes a `db/schema.rb` stops the columns answering at the very
  next settle. One `stat` per file read, and on a large workspace that is a few hundred of them.
- **Only the buffer path reports which document moved.** Every bulk route — the workspace walk, a gem
  batch, the file watcher, a rebuild — sets `touched_all`.
- **What is skipped is the reading, parsing, rendering and recording; the walk is not.** One keystroke
  in a file no generator reads costs **nothing at all** for the pass, and completion drops with it, on
  every corpus. Inside the pass that does run, **the `Context` walk is about a third** of a pass that
  reads every listed file and writes nothing, because building the evidence for the gate *is* the
  walk. **Read that denominator carefully**: as a fraction of the pass that actually regenerates the
  walk is under a tenth. `resolve` is identical either way and is not removable: a `didChange`
  genuinely invalidates the graph.
- **A file with no macro in it is not a file no generator reads.** `Wants::calls` matches a macro name
  called **anywhere** in a file, not in a class body, so a `lib/email/sender.rb` lands on
  `rails::MODELS` for a call to something named `helper` inside a `def`. A control for this gate has to
  be chosen by testing every `MACROS` name, the YARD tags, `Struct.new` and `sig`.
- **The tier sweep cannot see this gate, by construction**: `sweep.py` never `didOpen`s anything. Its
  evidence is the two unit tests and the `stat` guarantee, and `Analysis::passes` is the instrument.

### The walk the gate could not skip

- **Two gates, and the cheap one runs first.** Comparing the whole merged `Context` can only be asked
  after paying for the walk. `context_would_be_the_same` asks the same question of **one document**:
  `Analysis::contribution` is the loop body made callable for a single `Document`, `Context::absorb` is
  the only thing that merges one, and one `Contribution` per document goes beside `generated_from`.
  It runs **before** the walk.
- **That map is the walk's memo as well as the gate's evidence, and it is one map because it is one
  fact.** "This document contributes what it contributed last time" is the gate's question asked of
  the document a keystroke named, and the walk's answer for every other document at once — so the
  walk takes the held value instead of projecting it again. Keyed on **"has rubydex re-indexed this
  document"** and never on a file stamp: `contribution` reads the graph and never a file, and
  `touched`/`touched_all` already carry the right question. Measured 2026-09-16, median of three
  keystrokes that add a new top-level class, the walk goes **275 ms → 70** on discourse and
  **52 → 22** on lobsters, and the pass around it **340 → 132** and **64 → 34**. What is left is a
  floor — `absorb` is a fold with no inverse, `includers_of` and `bundle_namespaces` are folds over
  the whole projection — 32 + 9 + 18 ms of the 70.
- **The clone was the one cost nobody had measured, and it is 2 ms.** `absorb` takes a `Contribution`
  by value, so a walk that keeps it has to clone. Measured on the same keystrokes: the merge step
  goes 30 ms → 32, against 205 ms saved. The `&Contribution` shape that would avoid it stays unbuilt.
- **What it costs to hold is 50 MB on discourse** (924 → 977 resident) and nothing measurable on
  lobsters. One `Contribution` per document, 25,900 of them, and most of what is in them is a
  gem's class and module names — which `Context::foreign_classes` already holds a second copy of.
- **`engine_prefixes` is the one input that is not a document, and gem discovery drops the map where
  it writes them.** `is_own_code` and `is_generator_source` read it, so a held `Contribution` projected
  under a previous bundle could admit a former engine's `app/`. The gem batch sets `touched_all` and
  would drop it anyway; saying it at the assignment is what stops that being a fact two files apart.
- **The whole-`Context` comparison stays, behind it, because it is strictly wider.** A document whose
  contribution moved without moving the merged answer is a case a per-document comparison cannot see.
  When it fires the fresh contributions are **kept**, or that document would re-fail the cheap
  comparison for the rest of the session.
- **Three things make one document's `Contribution` enough**, two of which had to be built:
  1. **The merge is a function of the *set* of contributions**, not the order the graph hands them
     over in — `Context::absorb` and `Context::settle`.
  2. **A document nothing re-indexed cannot have contributed anything different.** Only `index_buffer`
     names a document; every bulk route sets `touched_all` and is refused.
  3. **A document the walk does not *visit* is refused outright.** `contribution` returns `None` for a
     gem file or an `.rbs`, and `bundle_namespaces` reads **every** definition in the graph — so an
     `.rbs` in the project's own `sig/` contributes nothing to the walk and can still move a `Context`.
     **Absent means *not visited*, never *contributed nothing*.**
- **Two fields were order-dependent and one was a latent defect.** `claims` is pushed once per
  definition, so `settle` sorts it — no answer moves, because `model_tables` sorts and dedups its own
  copy. And **`superclasses` was last-writer-wins over a `HashMap`'s iteration order**, while
  `defined_in` — filled by the *same* `if let Some(superclass)` two lines away — takes the lowest URI.
  A corpus reopens `class Shop::Product < Shop::Base` in its specs; which line won was whatever the
  walk reached last. Both now take the lowest URI, and `<=` rather than `<` is what keeps one document
  that says it twice on the line Ruby would run.
- **Every bulk route must set `touched_all`, `index_workspace` included.** The cheap gate trusts
  `touched` to be the whole of what moved. In production the two callers happen to cover it —
  `generated_from` is `None` on startup and `rebuild` clears it — which is a property of the callers
  rather than of the method, and **the suite caught it where no reading of the code did.**
- **The measurement, idle machine.** One keystroke in a large-workspace file no generator reads:
  completion **roughly halves**, and the gate itself goes from dominating that cost to a rounding
  error. Same shape on a small workspace.
- **`Analysis::walks` is the instrument, and `passes` cannot be.** The outer gate already stops the
  generators, so a gated-out pass either walks or does not and only a counter says which. What the
  sweep is for here is the two order-independence fixes, which **can** move an answer, and over five
  applications they move **nothing** — not up, not down, not sideways, in any corpus. The corpus that
  reopens a model under two spellings had to be looked at rather than counted.
- **A keystroke in a file that *is* on a list pays the whole pass, and what that costs depends
  entirely on whether a generated document's *text* moved.** Measured 2026-09-16 on discourse,
  release, median of three, 977 generated documents:

| the keystroke | the pass | the walk | `index_source` |
|---|---:|---:|---|
| a macro that names nothing new — every generated document byte-identical | **63 ms** | 0 | 0 documents |
| a new class name — the projection moves, no generated text does | **132 ms** | 70 ms | 0 documents |
| `attribute :x, :string` — one model re-declares | **224 ms** | 0 | 1 document, 18 KB, **148 ms** |
| `attribute :raw, :integer` — re-types a real column, so the schema declines it | **2.49 s** | 0 | 1 document, 450 KB, **2.2 s** |
| the cold pass | 8.5 s | 275 ms | 509 documents, **7.9 s** |

  **Re-indexing generated RBS into a *settled* graph is the pass's one steep cost — 95% of any
  pass that re-indexes anything — and it is charged per *declaration*, not per byte.** The eleven
  generators, the render and both gates are 62 ms of every row above and are edit-independent.
  Fitted and then cross-checked against four independent results: **~32 ms per document plus
  ~0.7 ms per declaration warm, and ~0.46 ms per declaration cold.** Checks: the schema's 3,180
  columns → 32 + 2,194 ms ≈ 2.2 s; `post.rb`'s ~165 declarations → 148 ms; the split keystroke
  below, 0.27 s predicted against 0.31 measured. It is `Graph::consume_document_changes`
  invalidating per declaration against an index that is near-empty cold and holds ~144,000 entries
  warm — rubydex's cost and not this crate's — which is the whole of the 35× cold/warm gap.
- **There is no byte term, and two levers that assumed one are dead.** *Shrink the provenance
  comments*: they really are **1.76 MB of the 2.61 MB** a discourse pass writes (67.7%, universal
  across schema, routes, an engine, a base class and the models), and removing **all** of them moved
  a cold pass 8.31 → 8.24 s and the keystroke 2.46 → 2.44 s. Nothing. *Concatenate every generated
  document into one before indexing*: moves only the per-document term — 0.05–0.5 s of an 8.3 s boot
  — and is actively harmful warm, because it destroys the per-document text comparison that makes
  975 of 977 documents free (0.6 ms in total).

### The document is a body, and that is what the cost model buys

- **Splitting on `Facts::render`'s own key is what makes the per-declaration cost payable.** One
  document per file means a column that changed type re-indexes every column in the project; one
  document per body re-indexes one table. Measured on discourse, release, median of three, with the
  split behind an environment switch before it was written for keeps:

| | one document per file (977) | one per body (1,320) |
|---|---:|---:|
| cold open | 8.44 s | 8.47 s |
| cold open, every table moving | 8.77 s | 9.07 s |
| keystroke re-typing a column | 2.48 s | **0.31 s** |
| every table at once, warm | 2.6 s | **13.7 s** |

- **The last row is the objection and the counter-case is counted rather than imagined.**
  discourse's own 1,007 migrations: **median 1 table, 92.8% exactly one, 98.4% one or two, 0.2% more
  than five.** So the split wins ~8× interactively and ~35× on the shape a real migration has, and
  loses only on a shape this corpus has never produced. The first version of this argument said "a
  migration moves every table"; it was hypothetical and it was wrong.

### Why one changed file does not re-read every other

Broken down by phase, one keystroke in a model file:

| phase | share of the pass |
|---|---|
| the walk | under a tenth |
| reading every listed file | small |
| parsing it with Prism | small |
| `signatures` — the declaring | small |
| phase two's union | small |
| `render` | negligible |
| **`Synthesized::record`** | **almost all of it** |

Reading and parsing every listed file — the obvious culprit — is about a tenth of it. A memo of the
`Facts` alone would buy roughly that.

- **`record`'s short-circuit was never the problem.** Instrumented, one keystroke: **every generated
  document but one skips**, the comparison costs a fraction of a millisecond *for all of them*, and
  `Types::harvest` about as much. Nearly the whole pass is `indexing::index_source` for the **one**
  document that changed.
- **The cost is a function of the graph and not of the document.** rubydex's
  `Graph::consume_document_changes` drops the old document, then invalidates every declaration the old
  and new one touch, cascading to their members, singleton classes and descendants. The same one
  document is **an order of magnitude more expensive** in a workspace an order of magnitude larger,
  which is why no corpus but the largest could have shown it.
- **And the document usually had not changed at all.** A provenance comment names a *file and a macro*
  and never a line number, so pressing return above a `has_many` re-derives RBS that is
  **byte-identical** and mappings that have every one of them shifted. The graph holds the text; the
  side table holds where the text came from; before this gate a move in the second tore down and
  rebuilt the first.
- **So the text decides whether the graph is touched, and the mappings are taken either way.**
  Assigning them costs a pointer and comparing them costs a scan of every span, so they are not
  compared. A mapping left stale is a jump that lands on the line a declaration *used to* be on.
- **`Synthesized::indexed` is the instrument** — the third counter here for the third time the same
  reason applies: a graph rebuilt into exactly the shape it had answers every question the same way.
- **What is deliberately not fixed** is the keystroke that really does change what a file declares.
  `User.` on the line above `belongs_to :x` makes the next line a call *with a receiver*, so the macro
  genuinely leaves the class: on both corpora that last keystroke still costs a full pass, against
  almost nothing for the ones before it. That is rubydex doing work the change requires.

### The memo is of the parse, not of the `Facts`

- **Memoising `Facts` is the obvious seam and the parse is the better one.** The `Context` is an
  argument to every generator, so a `Facts` memo keyed on the text alone is unsound. `read_model` and
  the four beside it take **nothing but the text** — no second input to compare, no invalidation rule to
  get wrong, and a keystroke adding a class elsewhere does not throw the memo away. What the `Context`
  is an argument to is `signatures`, which is a small fraction of the pass.
- **The freshness is the outer gate's `stat` and not a second mechanism.** `Fresh::Disk` *is*
  `stamp_of`. A `git checkout` that rewrites a schema takes effect at the very next settle with no
  watcher in it, and there is a test that writes the file behind the server's back to say so.
- **A buffer is hashed and not versioned.** A client may send `didChange` with no version at all, and
  two versions of one buffer would then compare equal. The two authorities cannot be compared to each
  other, so a file being *opened* costs one read of one file, once.
- **The reads are per document and were per generator.** A model file that also carries a `@return` tag
  and a `self.table_name=` was read three times and is now read once.
- **The memo compares the readers as well as the text, and the reason is a finding.** Every list but
  one is a function of a document's own content. `rails::MODELS` is not: `Analysis::walk` adds to it,
  *after* the walk, every **model** that writes no macro, and whether a class is a model depends on a
  superclass chain running through other files. So `class Widget < Base` joins the model list the moment
  a different file makes `Base` a model, with not one byte of `widget.rb` changed — and a memo keyed on
  the text alone would serve an entry read for a `@return` tag that never ran the model reader, so
  `Widget` would **silently get no relation class**. There is a test, and reverting the comparison is
  what proved it.
- **Two readers are not memoised, one sentence for both**: neither is a function of its own text.
  `structs::read` takes the `Context`'s namespaces and `rails::read_routes` takes a prefix another
  file's parse computed. Together they are under a tenth of the reads.
- **What it costs to hold is a rounding error on resident memory.** The memo keeps one parse per
  listed file, against a graph holding every definition in tens of thousands of documents.

### The two gates ask two questions and must not share a clause

- **One gate must not answer both questions.** "Is a touched file on a generator's list" is about what a
  generator *reads* and says nothing about what the walk *builds*, so a walk gate refusing on it refuses
  far too often.
- Asked on its own it answers yes for the commonest edit in a Rails application: `has_many :comments`
  becoming `has_many :tags` changes what the file says and not one thing a `Contribution` holds. So the
  projection is reused and the generators run — `context_would_be_the_same` and
  `generators_would_repeat_themselves`, the second shared by both gates.
- **`Contribution.declared` is what makes it sound**: `bundle_namespaces` reads every class and module
  definition in the graph, and every one in a *visited* document is pushed to `declared`.
- The instrument is `walks` and `passes` **together**, and the test asserts both in the same breath: a
  pass that did not run would satisfy "it did not walk" for the wrong reason.

### What the three gates are worth, idle machine

One keystroke in a model file, each mechanism added in turn. Measure it by hand against a real
repository, on the largest corpus, where the differences are legible:

| | effect on the pass |
|---|---|
| neither gate | the whole pass, every keystroke |
| the per-document gate | barely moves it, on a file that *is* on a list |
| + `record`'s text test | **cuts it by roughly three quarters** |
| + the parse memo | cuts what is left by a third again |
| + splitting the walk clause | **another fourfold, and the walk reads zero** |

The `walk` line reads **nothing on every keystroke** on both corpora once the last row is in.

- **A file no generator reads is untouched by all of this, and that had to be checked** because the
  gates were rearranged under it. The two skip messages are **one**, because the path that skips
  everything no longer distinguishes "the walk sees nothing" from "the pass reads nothing".
- **`resolve` is unmoved and is now the larger half**, on both corpora. By arithmetic on the measured
  totals, most of what is left of a keystroke is neither the pass nor `resolve` — it is `index_buffer`
  for the edited file itself, paying exactly the invalidation the text test stopped paying for the
  *generated* document, where it is real work.
- **The tier sweep cannot see any of the three gates.** What it *can* see is the memo, which changes
  how every generator gets its source on a cold pass as well as a warm one, and over five applications
  it moves **nothing at all** in any corpus.

## What the framework's own singletons return

- **railties and activesupport ship no `sig/`, so four of the most-written chains in a Rails
  application stopped at their first hop.** `Rails.root.join` resolved its receiver perfectly —
  `Receiver::Constant` reaches `singleton_of`, rubydex has the `def self.root` railties really
  writes — and then died at the `?` on `Types::returns`, falling to the name rung: **30 possible
  `join`s** on lobsters, 59 `fetch`es for `Rails.cache`. The member was never missing. Only what it
  returns was, and that is a sentence this crate can write down in advance, which is why it goes
  through the generator route like everything else here and adds no rung.
- **The bar is not "is the return knowable" — it is "does the class the answer names hold the
  members the call then asks for".** `types.md` records four times a rung that *gained* a type lost
  the name-matched list that had been holding the right word. So every candidate was scored before
  anything was written, against the members the six corpora really call one hop later, asked of a
  real bundle. Four rows passed and four failed, and the four that failed share one mechanism: the
  class Rails really returns answers those calls through `method_missing` or `define_method`, which
  rubydex cannot see.

  | chain | chained calls | return | answered |
  | --- | --- | --- | --- |
  | `Time.zone` | 2,060 | `ActiveSupport::TimeZone` | 100% |
  | `Rails.root` | 895 | `Pathname` | 100% |
  | `Rails.cache` | 362 | `ActiveSupport::Cache::Store` | 99% |
  | `Rails.application` | 682 | the application's own class | 88% as `Rails::Application` alone |
  | `Rails.logger` | 1,395 | `ActiveSupport::BroadcastLogger` | **5%** |
  | `Rails.env` | 455 | `ActiveSupport::EnvironmentInquirer` | **4%** |
  | `Rails.configuration` | 611 | `Rails::Application::Configuration` | **56%** |
  | `Time.current` | 199 | `ActiveSupport::TimeWithZone` | **79%** |

- **`Rails.logger` is the row worth stating twice, because it is the largest population of the
  eight and the temptation is real.** `ActiveSupport::Logger` answers **94%** of those 1,395 calls
  and is **not what Rails returns**: since 7.1 `Rails.logger` is an `ActiveSupport::BroadcastLogger`,
  whose `info`, `warn`, `error` and `debug` are `method_missing`. A rank that is correct-if-true
  cannot be bought by naming the wrong class, so the whole row is declined and the name rung keeps
  it. The same sentence declines `Rails.env`, whose `development?` and its two siblings are
  `define_method` over a constant list.
- **What it was worth, measured the way the rule asks.** Every `Const.method.member` position in six
  corpora, one release binary with the list on and one with it off, nothing else different:
  **6,659 positions, 3,693 moved from a name-based list to one exact answer, 0 moved the other way,
  0 went silent and 0 lost a place.** The four declined chains are byte-identical across the two
  runs, which is the control. By chain: `Time.zone` 2,032 of 2,060, `Rails.root` 880 of 895,
  `Rails.application` 442 of 682, `Rails.cache` 339 of 362.
- **Only the return is declared, never the method**, which is what keeps every one of those jumps
  where it was. railties writes `def self.root` and activesupport writes `def zone` in a
  `class << self`, so the declaration rubydex holds already has a place; the definition written here
  carries `Declared::at` `None` and never becomes a second one. `Source::Interface` for that reason
  — no file says what these return, whatever else it says about them.
- **Both ends of every row are checked against the graph first, and the owner is the half that is
  not obvious.** A `def self.root` declared on a `Rails` nothing else declares would *invent* the
  module: a constant with one member and no place, where before there was an honest miss. So
  `rails::singleton_classes` names both sides, `wanted_namespaces` asks the bundle about all six,
  and a workspace with no railties indexed declares nothing at all. It is
  `rails::framework_classes`' rule for the long tail's three gem classes, read the same way.
- **Which keyword opens each owner's body is read rather than assumed.** railties writes
  `module Rails`; activesupport reopens Ruby's `class Time`. The render key is `(is_module, name)`,
  so guessing wrong declares a second constant RBS refuses to hold beside the first —
  `Namespaces::opens` is the graph's answer and the table deliberately does not carry one.
- **`Rails.application` is the *project's own* class, read from the same document the list is keyed
  by.** `config/application.rb` holds `class Shop::Application < Rails::Application`, and that class
  is where a project writes the `config.domain` and `shortname` it hangs off `Rails.application` —
  12% of every call the corpora make on it, and unreachable through the framework's base. A file
  that declares none falls back to `Rails::Application` and loses only those. The class read here
  needs no second opinion from the graph: it came off a `class` line in the application's own file.
- **The host is `config/application.rb` and that is the honest choice rather than an arbitrary
  one.** These four returns are a property of the framework, not of any document, so the list
  answers *where to write them down*. `config/application.rb` is the file every Rails application
  has and no other kind of project does — the marker `Features::resolve` detects `auto` by — and it
  is the file that holds the one thing the generator actually reads. A full path and not a suffix,
  unlike the schema and routes lists: `application.rb` alone is `app/models/application.rb` in three
  of the six corpora.
- **The eighth list has no key of its own, and that is a decision.** Every other key lets a project
  decline a body of knowledge that does not apply to *it* — its own schema, its own macros, its own
  routes. There is nothing project-specific here to decline: a workspace whose bundle holds railties
  cannot sensibly say `Rails.root` is not a `Pathname`, and one whose bundle does not is already
  declaring nothing. So it is gated by `rails.enabled` alone, it is absent from
  `each_switch_turns_off_its_own_family_and_nothing_else`, and it is the sixth family in
  `rails_off_takes_all_six_rails_families_and_leaves_the_two_that_are_not_rails`.
- **Two bounds the sweep found, both stated rather than fixed.** **solidus gains nothing at all**
  — 0 of 111 — because it is a gem monorepo with no `config/application.rb`, so the list is empty
  and nothing is written. That is the host choice's price, and a second host for a project that has
  none is a change with its own measurement rather than a line added here. And of
  `Rails.application`'s 682 calls, 240 do not move: **50 already answered exactly** before the
  table existed, by a name rung whose match happened to be unique (`Rails::Application#reloader`);
  **91 answered nothing before and nothing after**, most of them `Rails.application.routes` in
  chatwoot; and **99 stay on a name-based list**. The largest single group inside the 240 is a
  method the application writes in `class << Rails.application` — a singleton reopened on the
  *object*, which belongs to no class rubydex can name. No class-based type reaches those, and
  none should claim to.

## The rest of ActiveRecord's vocabulary

- **The bound is Rails' own list, and that is the whole of it.** A table chosen for being *typeable* is
  a bound nobody can check. `query_interface` is `ActiveRecord::Querying::QUERYING_METHODS` — **all
  113** — plus the two places Rails puts a class method that is not in it:
  `Persistence::ClassMethods`, where `create!` lives, and `Querying#with`. **119 on both sides, 6 on the
  relation alone, 1 on the class alone.** A name added without a line of Rails behind it moves a counted
  assertion — and one moved sides without a line behind it moves the same number, which catches five of
  the six `Persistence::ClassMethods` names if shipped class-only: `relation.rb` defines `new` at 126,
  `build` as an alias at 134, `create` at 155, `create!` at 170, `update` at 640 and `update!` at 664,
  so `story.comments.create!` really is a call. `instantiate` is the one `Relation` does not define.
- **`new` is the relation's and only the relation's.** `relation.rb:134` is `alias build new` — the
  *same method* — so declaring `build` and not `new` was an incoherence, and `story.comments.new` had no
  answer. It is `Side::Relation` because a model already gets `new` from `Class`. Measured on a receiver
  naming a declared `has_many`: **the two spellings are written about equally often**.
- **What makes the widening safe is that a name may still refuse a *type*.** `pick`, `calculate`,
  `minimum`, `maximum`, the eight `async_*` and the six bulk writers return `untyped`: the name
  resolves, the chain stops, and `Types::harvest` drops the claim. **Decline the type, never the name**
  — otherwise "I cannot type this" and "Rails does not have this" are the same answer.
- **`Query::side` is a three-way answer and not a flag.** `delegated: bool` could only ever *subtract*
  from the relation's list; `instantiate` is on the model and on **no relation**. `Side::Relation` is
  five names — `size`, `length`, `empty?`, `to_a`, `each` — every one of which raises on the model.
- **`Declared::overloads` is the fact table learning to say what RBS can say.** Four names answer two
  things depending on how the call was written, which `Types` reads off the *call site*: `Story.first`
  is a `Story?` and `Story.first(3)` an `Array`; `Story.select(:id)` is a relation and
  `Story.select { }` an `Array`. It renders **on one line** — `def first: () -> Story? | (Integer) ->
  Array[Story]` — because a `Span` is a byte range and a declaration crossing a newline is one more
  thing every consumer of `spans` would have to be right about.
- **An overloaded member has no `Facts::returns`.** What `delegate :first` hands back is a question
  about the call the delegator writes, which phase two cannot see, so it answers `None` and the
  `delegate` declines the type and keeps the name.
- **The relation arm of an overload must not claim arity 0 as well.** `settle` fills `by_arity[n]` with
  what *every* arm accepting `n` agrees on, and a `(*untyped)` rest arm accepts every arity including
  zero — so pairing `() -> A` with `(*untyped) -> B` makes the zero-argument call answer **nothing**.
  `first`'s second arm is `(Integer)` and `select`'s is told apart by a **required block**.
- **`where` is declined, and it is a bound rather than a gap.** `where` with no argument returns a
  `QueryMethods::WhereChain`, where `not`, `missing` and `associated` live — hundreds of `not` call
  sites across six corpora. An arity split beside `first`'s is **not expressible**: `arity_of` deliberately does not
  count a keyword hash as a positional argument, so that `3.7.round(half: :up)` reaches the
  zero-argument arm — which makes `where()` and `where(title: "x")` the same call to this machinery. An
  arm answering `WhereChain` at arity 0 would answer it for the commonest call in Rails. Putting `not`
  on the relation instead is the sin the side table exists to prevent: `Story.all.not` raises.
- **Three approximations, stated rather than hidden.**
  - *A scalar or an array in one argument.* `find`, `create`, `create!`, `build`, `instantiate` and
    `destroy` hand back a record for a scalar and an `Array` for an array, both writing **one**
    positional, so `Arity` cannot tell them apart. Each types the singular. `update` and `update!`
    return `untyped` for a different reason: their first parameter *defaults to `:all`*.
  - *`count` after a `group` is a `Hash`* and this says `Integer` unconditionally.
  - *`pluck` and `ids` are `Array[untyped]`*, not `Array[Element]`.
- **`find_each`, `find_in_batches` and `in_batches` declare a *required* block.** Without one each
  returns an enumerator, so an optional-block arm would claim `void` for a call that chains off it. The
  required arm also puts an element into `Types::yielded`.
- **`sum` declares no block arm at all.** `Enumerable#sum` with a block hands back whatever the block
  summed; stating only the blockless arm is what makes `Story.sum { ... }` answer nothing rather than
  wrongly. `count` needs no such care.
- **The provenance sentence is written once per *base* rather than 120 times per model.** Spelling it
  per declaration measures **more than half of all the generated RBS in the workspace**, on every
  corpus. What it may **not** do is disappear: `class ApplicationRecord` is the
  user's own class and a generated `def self.pluck` with nothing above it reads as something their file
  declared. A relation class carries none because the *class* is what ya-lsp invented.
- **The cheap version was built, measured and refused, and the reason is a finding.** Rails writes the
  shared half once, so a generated `module Story::Querying`, `include`d into `Story::Relation` and
  `extend`ed onto `Story`, is the framework's own arrangement and cuts output to roughly half: half
  the declarations per element type, and one note on a generated module instead of one sentence per
  declaration on the user's class. It resolves correctly in a graph indexed cold. It does **not** survive
  an edit: an `extend` arriving in a document indexed *after* its class was resolved is **never
  linearized**, and no repair reaches it. `class Author`, new in the same document, gets its class side;
  `class Story`, already resolved, does not — a full re-index does not repair it, and neither
  `locator`'s ancestor search nor `extends_written_on` finds the member. A generated document is
  *always* that shape, which is why nothing else in the crate meets the bug. The `include` half has no
  such problem, which is why the route helpers' module works.
- **The whole interface is written once for the project**, made possible by two receiver-relative return
  types. `Return::Element` means *the model this receiver is about* and `Return::Collection` *that
  model's relation*, so `def first: () -> ActiveRecordElement?` written once answers `Story` on
  `Story::Relation` and `Comment` on `Comment::Relation`, and **no signature in `query_interface` names
  a class of the application's at all** — asserted as a property of every row.
- **The two spellings are sentinel class names and RBS has no keyword for either.** `self` is the
  receiver and `instance` is the *declaring* class; this needs neither. `types::ELEMENT` and
  `types::COLLECTION` are read in `class_of`, the one place an RBS type becomes a `Return`. Neither
  reaches a reader and neither reaches the graph: nothing declares a class of that name, so a lookup
  escaping `class_of` answers `None` and stops the chain.
- **`model_of` answers `None` for anything that is neither a relation nor a class object**, and that is
  the safety. `relation_of`/`element_of` are one invertible mapping and a test says so.
- **The relation half is a superclass, safe because nothing else declares a relation class.** Every
  `X::Relation` is one line — `class Story::Relation < ActiveRecordRelation` — and `rails::RELATION_BASE`
  holds all of it. **A generated superclass on a class the user's own file already gives one is silently
  ignored**: measured, a `class Widget < SpikeBase` in RBS beside a `class Widget < ApplicationRecord` in
  Ruby leaves `SpikeBase`'s members unreachable, with no error and no diagnostic. That is why the class
  side may not use a base this crate invents.
- **The class side is inherited, so it goes on the model's own base.** Ruby follows a class object's
  singleton chain up the class chain, so one copy on `ApplicationRecord` answers for every model under
  it. `synthesize::base_of` is `models_of`'s walk with a different stopping rule — that one climbs to
  `ActiveRecord::Base` and answers *whether*, this one climbs while the next class up is a model the
  application declares and answers *which*. A model whose chain leaves the application — a
  `Tag < ActsAsTaggableOn::Tag` — is **its own base**.
- **One hop past the application, onto `ActiveRecord::Base` itself, and only when the bundle declares
  it.** The largest corpus has no `ApplicationRecord` at all — every one of its models sits directly
  on `ActiveRecord::Base`.
  Reopening somebody else's class is safe for the reason the `MessageDelivery` stub is (nothing declared
  here is mapped), and the gate is the conjured-namespace rule: the **bundle** has to declare the name.
  It is therefore **self-regulating** — an unindexed bundle falls back to per-model, visible as two
  different member counts across the cold-start boundary.
- **`ActiveRecord::Base` exactly, and not `is_record_base`'s other spelling.** An `ApplicationRecord` the
  application declares is reached by the walk; one it does *not* declare is a name this pass may not
  write on at all. Reading the two as one question put the interface on a bare
  `class ApplicationRecord` that inherits nothing.
- **An abstract class keeps the class side, and the return types are why that is safe.** Taking it off
  is the right rule while the interface names a concrete class, because it is inherited.
  `Return::Collection` and `Return::Element` fix that at its source — `Category.order` is a
  `Category::Relation` and `Captain::Assistant.find` a `Captain::Assistant` — so inheritance is the
  mechanism rather than the defect. What is left of the trade: `ApplicationRecord.where` resolves and
  raises in Ruby, which is the callbacks' kind of wrongness, on a receiver nobody writes.
- **The end-to-end test asserts on the place and not on the card**, which is sharper either way: `order`
  is declared once on the base, so the card names `ApplicationRecord.order` whatever it returns. What has
  to be right is the chain — `Category.order(:id).first.things`.
- **`include Enumerable` on the relation base is Rails' own line, and leaving it out costs measurable
  down-moves.** `select` correctly answers a relation, and a relation with no `Enumerable` is a **dead
  end** for the `.index_by` or `.map` that habitually follows.
- **`Facts::inherits` is a list and not a `Declared`**, for `Facts::mixins`' reason: a superclass names
  no member, has no return type and cannot collide. Said twice for one body it keeps the first, because
  a class has one superclass and two `<` clauses are RBS that does not parse. A `Facts` holding nothing
  but a superclass is **not empty**.
- **A body with a superclass and no member is opened by the trailing loop**, the same one that opens a
  body with an `include` and no member.
- **Exactly one document writes `ActiveRecordRelation`** — the first that emits a relation at all — for
  the reason exactly one writes the `MessageDelivery` stub. A workspace with no relation writes none.
- **A project that declares `ActiveRecordRelation` itself keeps it, and what is withdrawn is the
  relations.** It is asked of `relations` rather than of the base alone, because the two failures are
  different and both real: writing into their class would answer with their members *and* ours, and
  letting a relation inherit it would give every collection whatever they meant by the name. With no
  relation class there is nothing for a `has_many` to return, so the whole half declines together.
- **The cost is the hover card**, stated rather than hidden: `story.comments.where(...)` printed
  `Comment::Relation#where` and prints `ActiveRecordRelation#where`; a class-side call prints
  `ActiveRecord::Base.where`. The element is gone from the card and is still in the *answer*.
