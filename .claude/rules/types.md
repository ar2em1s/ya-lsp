---
paths:
  - "src/analysis/types.rs"
  - "src/analysis/cursor.rs"
  - "src/analysis/hover.rs"
  - "src/workspace/rails/**"
  - "src/analysis/annotations.rs"
---

# The types ya-lsp derives

## The table

- **rubydex models no types, so every type ya-lsp knows it owns.** `return_type` is zero matches in
  rubydex's source, published *and* at `origin/main`; its graph is a record of "which declaration is
  this name" by design. `analysis::types` is a table beside the graph, not inside it, which keeps the
  pin cheap to reverse. Re-open the question the moment `return_type` appears upstream — this whole
  module would become a version bump.
- **The table is keyed by `DeclarationId`, and the lookup happens *after* rubydex found the member.**
  A table keyed by `(receiver name, method name)` is wrong in a way nothing shows: `[].tap` is owned
  by `Kernel`, not `Array`, so asking for `Array#tap()` misses. The ancestor walk is
  `query::find_member_in_ancestors`'s; this reads the answer off the declaration it found.
  `DeclarationId::from("String#upcase()")` is a pure hash, so the harvest builds keys with no graph
  and the two meet at lookup. `rubydex_spells_a_signature_the_way_this_keys_it` pins that spelling
  against a real graph, including the singleton form `Shelf::Book::<Book>#open()`, because a rename
  upstream would make every lookup miss in silence.
- **`self` is resolved at lookup and everything else at harvest.** RBS's `self` means *the
  receiver's* type. `Kernel#tap` is declared `() { (self) -> void } -> self`, and `"x".tap` returns a
  `String`: filing it as `Kernel` at harvest — which the first version did — answers `"x".tap.` with
  `Kernel`'s three methods and none of `String`'s. `instance` and `class` *do* name the declaring
  class, so they are resolved where written. `Return::Same` carries the difference.
- **Two more returns are resolved at lookup, and they are `self` one step further.**
  `Return::Element` means *the model this receiver is about* (`Story::Relation` and `Story`'s class
  object both mean `Story`); `Return::Collection` means *that model's relation*. They let
  ActiveRecord's whole query interface be declared **once for the project** instead of once per model
  — an order of magnitude fewer generated declarations on a real application (`synthesized.md` has
  the generator's half). RBS has no keyword for either, so they are the sentinel class names `types::ELEMENT` and
  `types::COLLECTION`, read in `class_of` — the one place an RBS type becomes a `Return`, so a
  spelling recognised anywhere else would be a second table. Nothing declares a class of either name,
  so a lookup escaping `class_of` answers `None` and stops the chain, and **no hover card prints a
  return type**, so neither reaches a reader.
- **`model_of` answers `None` for a receiver that is neither a relation nor a class object.** The two
  returns are declared on exactly two kinds of body, so a receiver that is neither reached one
  through an ancestor nobody meant. It reads the receiver's **name** — `Story::Relation` is
  `relation_of`'s spelling and `Story::<Story>` is rubydex's for a singleton — because the name is
  the only thing the two shapes have in common.
- **A generic's head is not an approximation, it is the answer to a different question.** Method
  lookup on `Array[Integer]` and `Array[String]` reaches the same declarations, so offering `Array`'s
  members is exactly right for what a `.` asks. The element type is a different question this module
  does not answer.
- **Optional takes the inner type, and it is the one inexact entry.** `String?` becomes `String`,
  because `nil` has almost no members anyone completes on. A method that returned `nil` this time
  really has no `upcase`, and provenance is what makes that visible rather than silent.
- **One `Unknown` ends a chain rather than being carried up it.** `cursor::Receiver::Returned`
  wrapping an `Unknown` would only make the graph side rediscover there is nothing to look up on. The
  same rule bounds the walk: `MAX_CHAIN` is 8, a bound rather than a budget — this runs on the
  analysis thread on a keystroke, and `x = x.foo` is legal Ruby.

## Reading an RBS signature

- **Overloads are a union — except where a block tells them apart, which is worth a real slice of core.**
  Several arms naming several classes is a union and is dropped. But the commonest multi-arm shape in
  Ruby's core is not one: `def bytes: () -> Array[Integer] | () { (Integer) -> void } -> self`, where
  **which arm applies is decided by whether the caller wrote a block** — syntax the cursor already
  read. So the table holds an answer per block-ness: `"x".bytes.` is `Array`, `"x".bytes { }.` is
  `String`, neither a guess. Measured over `vendor/rbs/core`: most multi-arm definitions agree
  outright, **a substantial minority are decided by the block**, and the rest either disagree
  blocklessly (dropped), have no blockless arm, or declare nothing usable. The first version filed
  the block-decided ones as unions and threw away `bytes`, `chars`, `lines` and `split`. **Taking the
  first arm instead would have been the wrong fix** — it reads as exact and is wrong wherever a block
  was written.
- **A required block is not optional, and `?{ ... }` is.** An arm with a required block answers only
  calls that wrote one — `"x".tap.` reaches nothing, which is right, because Ruby raises
  `LocalJumpError`. An optional block's arm applies both ways and is counted in both.
- **The arity is the second partition, and it pays out on plain Ruby.** How many positional arguments the call
  wrote tells arms apart exactly as the block does. `Float#round` is the shape:
  `(?half: ...) -> Integer` beside `(int digits, ?half: ...) -> (Integer | Float)`, which as one
  partition is a union and as two is `3.7.round.` answering `Integer`. Measured over `vendor/rbs`,
  it adds a further tranche of methods that answer. The gain is the chained half of core —
  `Array#first(n)`, `#last(n)`, `#pop(n)`, `Float#ceil`, `#floor`, `#truncate`,
  `Hash#transform_keys` — the one partition that pays out on Ruby that is not Rails. Arms an
  *optional* positional already covers are not counted separately, which is why a naive count of
  distinct arities overshoots.
- **An optional positional is to the arity what `?{ }` is to the block, and a rest parameter has no
  most.** `(?String chars) -> String` answers both a zero- and a one-argument call from one arm.
  `(Integer, *String) -> String` answers one and every number above, which is what `Arms::beyond`
  holds — the buckets run only as far as the widest arity an arm names exactly.
- **A call no arm accepts is answered for by none of them, and that removes zero-argument answers
  in bulk.** This is the one place the split *removes* an answer: `"hi".scan.` used to be an `Array`
  because `scan` has one arm and nothing looked at how it was called. It requires a pattern, so a
  call writing none reaches no arm. Over a thousand methods in `vendor/rbs` are in that position,
  every one requiring an argument — provable rather than observed, since a side whose arms all agree
  agrees on
  every non-empty subset, so an empty bucket is the only way to lose one. Falling to the nearest arm
  is a guess wearing an exact answer's clothes. `request.format.` shows the removal is a *fix*:
  `Kernel#format` is reachable through the ancestors and requires a format string, and answering
  `String` for a `Mime::Type` was wrong before anybody counted.
- **A call whose arguments cannot be counted gets what every arm agrees on.** `foo.sub(*args).` and
  `foo.g(...)` reach `Arity::Unknown`, and `Arms::any` holds the unpartitioned answer — exactly what
  such a call got before arity was read. That field makes the split a partition rather than a filter;
  guessing a low count would pick the arm with the fewest parameters, the "nearest arm" rule renamed.
- **`Array#first` was dropped twice over; arity fixed one, and neither half was the generics one
  people expect.** It declares `() -> E` and `(int count) -> Array[E]`. As one partition they name
  two things and both go — the half arity fixed, so **`[1, 2].first(3).` answers `Array` now**. The
  zero-argument arm is still refused because a type variable is not a class, which generic
  instantiation would fix. **Even with both, `[1, 2].first` answers nothing**, because `[1, 2]` never
  carried `Integer`: `cursor::Receiver::Literal` is a `&'static str` with nowhere to put one.
- **What a block is *handed* is in the same signature as what the call gives back, and both are
  read.** RBS writes `def each: () { (Story) -> void } -> Relation`. Reading only the first types a
  block parameter **only where its own name camelizes onto a class**: `.each do |story|` looked as
  though it worked and `.each do |instance|` answered nothing. `Types::yields` is the second table
  and `Receiver::Yielded` is the shape reaching it.
- **It is a second map rather than a field of `Overloads`, because it is not partitioned the same
  way.** `returns` is a function of the *call site*; a block parameter is not — it is what the
  signature says the block receives, and any call reaching the lookup already wrote a block. So one
  entry per method, and `Receiver::Yielded` carries no `arity` and no `block` field.
- **Agreement across arms is the whole of the safety, the same rule the return side uses.**
  `Enumerable#each_entry` is declared one way yielding an element and another yielding an array of
  them, so `declared_yield` answers nothing where two arms disagree. An optional `?{ ... }` is read
  alongside a required `{ ... }`, because the call that reaches this wrote a block whatever the
  signature permitted.
- **The two tables are filled independently.** A method whose *return* the policy refuses can still
  say exactly what its block receives — `each` is declared `() { (Story) -> void } -> void` all over
  Ruby's signatures — and gating the block on the return would throw that away for the commonest
  yielding method there is.
- **A block parameter is asked *after* every assignment and wrapped in `Receiver::Spelled`, so it
  adds no rung and can displace no answer.** Assignment first, block signature second, name last. The
  crate's stance on shadowing is unchanged: `type_the_local` treats a block's `x` and the outer `x`
  as one variable, so a typed assignment above the block wins over the parameter that would shadow it
  in Ruby. That is a **wrong answer** rather than an absent one — the same trade documented for a
  reassignment inside a branch.
- **The innermost enclosing block wins, and a call with no receiver hands over nothing.**
  `a.each { |x| b.map { |x| x } }` is legal, so the shortest block span containing the read is taken.
  A receiverless `each { |x| }` is an implicit `self`, and what a `yield` inside that method hands
  over is the method's own body, so it stays on the name rung.
- **Typing a block parameter faithfully is only an improvement if the signature it reads is right.**
  Measured against a tree where a `scope` in `ApplicationRecord` put the query interface on a
  singleton every model inherits, it read **better in bulk but with real down-moves** — every one
  `@categories.each do |x|` typing `x` faithfully as `ApplicationRecord`, where guessing from the
  word had been accidentally right. Once every model owned its own relation, the same code measured
  **better still and worse nowhere**. Reading one more fact out of a signature *removes* the guess that was covering for
  a wrong signature, so the wrong signature has to go first.

## The five rungs, in order

rubydex naming the receiver → a signature or an assignment → the controller a template's path names
→ the receiver's own spelling → the name-based list.

- **The order is the whole safety argument.** `locator::resolve_typed` only reaches past the first
  when rubydex came back imprecise; `types::named` asks the convention before the guess; and `cursor`
  never lets a bare name count as an assignment's answer — `type_the_local` and
  `type_the_instance_variable` both skip a `Receiver::Named` candidate, so `x = Person.new` above
  `x = whatever` keeps answering `Person`. Remove any one of those three and a guess starts
  displacing something checkable.
- **The three tiers are said out loud.** *Resolved* — the code names the type, no footnote.
  *Derived* — a signature or an assignment was followed, and the footnote names which. *Guessed* —
  matched on the method name alone. A user who cannot tell them apart has lost the property that
  makes this server different from one that guesses well.
  `the_three_tiers_of_answer_drawn_side_by_side` pins all three together, in `GALLERY` shape.
- **The derived tier costs a parse, and it is only paid where it can help.** `typed_receiver` runs
  `cursor::at`, which parses the file, and the instance-variable path parses it again through
  `scopes`. `resolve_typed` reaches it **only after `resolve_call` came back imprecise**, so a call
  rubydex resolved pays nothing — and hover and go-to-definition are user-triggered rather than
  per-keystroke. Completion pays it on `@foo.` and nowhere else. Moving this rung above the rubydex
  one would cost a parse on every hover in the project and buy a worse answer.
- **`resolve_typed` classifies the cursor once and dispatches, and the two rungs below it can never
  both answer.** A call with a receiver written reaches `types::method_receiver`; a call with none
  reaches `views::Reachable::member`, which answers a **member** rather than a receiver's type —
  there is no type for a template's implicit `self` to have, which is why the view context is not
  declared in RBS. `cursor::at` runs here rather than inside `typed_receiver` (which is why that
  function is gone): asking the same classification twice was a second parse on a path that already
  pays for one. `Derivation::view` is how the card says which convention answered.
- **`resolve_typed` is for callers that have the document's text; `resolve` is for the rest, and that
  is not a gap to close.** `hover` and `definition` go through the first, so a jump and a card cannot
  disagree about what `person.` is. `references`, the type hierarchy and `rename` stay on the second
  **deliberately**: a work list is a list of places to *edit*, and a derived receiver is the one
  thing in it that could be wrong. `signature_help` and keyword-argument completion likewise go
  through `locator::precise_call` — extending them to the derived tier is a real option, not taken,
  because a parameter list is believed.
- **An instance variable is wrapped in `Receiver::Assigned` and a local is not.** `person =
  Person.new` is a line the reader can see from where they stand, and it is exact. `@user` is typed
  from an assignment that can be in another method, twenty lines away, in a branch that never runs —
  so it carries the offset, and the card names the line.
- **Which `@foo` this is is `scopes`'s question, asked rather than re-derived.** `@v` in `def a` and
  `@v` in `def self.b` are two variables; a `def c` inside `class << self` shares the second.
  `cursor` pays a second parse to ask `scopes::variable` rather than keeping its own copy of the
  algebra — a highlight and a completion disagreeing about which `@foo` is which would be invisible
  in both.
- **The half-typed operator is blanked before the scope question is asked.** For `@name.` on the line
  above an `end`, Prism reads the `end` as the method name, consumes it, and **reparents everything
  below**: the `@name = "ada"` in an `initialize` below lands in a nested `def`, one singleton step
  away, correctly reported as a different variable. `Finder::without_the_half_typed_call` replaces
  those bytes with spaces, keeping every offset and line, as `signatures::without_interfaces` does.
  Removing it does not break a test that looks like it is about scopes; it breaks the common case of
  completing on an ivar.
- **What the signature and assignment rung buys, measured over a real application.** Thousands of
  receivers gain a *shape* and only a small fraction can be looked up — chains and instance variables
  in roughly equal measure — because return types exist for core and stdlib and nothing else. Every
  instance variable assigned something nameable (`Foo.new`, a literal) resolves; every failure is an
  ivar or chain through an application method that nothing declares. **The ceiling on this tier is
  the annotations, not the machinery.**
- **`hover`'s derivation footnote deliberately does not say where a signature came from.** A
  signature may be `vendor/rbs`, a gem's `sig/`, or text this crate wrote — `User::<User>#where()` is
  in the commonest chain in a Rails application. It says "from what those methods declare, not from
  this expression", which it can assert without checking anything. Where a *declaration* came from is
  answered by hovering it, through the comment its generator wrote.

## The guess

- **`Receiver::Named` is a spelling, not a type, which is why it lives in `cursor`.** The module with
  no graph cannot decide whether `person` is a `Person`; what it can do is not throw the six letters
  away. Everything that treated `Unknown` as "nothing exact can be said" answers the same when the
  rungs below come back empty.
- **The guess is bounded to a *bare* name**: an instance variable, a local nothing typed, and a
  receiverless call with no arguments and no block. `find(id).title` and `each { }.first` stay
  `Unknown` — the name of a method is not the name of a thing.
- **The member lookup is a second filter, and it does most of the work of making a guess safe.**
  Measured over a real application: most receivers that resolve to a class from their name never
  find the method the call names, so **the large majority of wrong guesses disappear before anybody
  sees one**. `request` in a controller guesses `Request`, which one corpus really declares in a
  script, and every one of those sites answers nothing because that class has no `xhr?`. **The residual risk is a wrong class that happens to have a method of the right name**,
  and provenance is the only defence — which is why the footnote is not optional.
- **A guessed class is looked up lexically, never through the ancestors.** `constant_named` walks the
  nesting outwards — `Admin::UsersController::User`, `Admin::User`, `User` — the half of Ruby's
  constant lookup that needs no reference to have been written. Reaching through an inheritance chain
  would give a guess more ways to be wrong and no more ways to be right.
- **A guessed *receiver* is not the name-based list, and a completion row must distinguish them.**
  The rows really are one class's members, a better list than every method in the project matched by
  name — so `Completion::precise` stays true and a second field carries the doubt.
  `completionItem/resolve` gets both on `data`. Saying nothing would present six letters of inference
  as a resolved type.
- **`Sources` exists so that two more parameters do not become six.** The type side reads exactly two
  things that are not the graph — another document's text, and whether the guess may answer — and
  both had to reach `completion`, the locator and `types` alike. The closure keeps the reading in
  `analysis::mod`, where the open buffers are.

## The view↔controller convention

- **It is the only Rails knowledge in the crate, and it is one directory.** `workspace/rails/` is
  pure text with no I/O and no graph, like `bundler.rs`, and it is on the 100% list for one reason:
  being framework-aware is a surface with no natural edge, so the edge is drawn where it can be read
  in forty lines. A reviewer asking how much Rails is in ya-lsp reads one list — the convention
  tables in `rails/mod.rs`, the directory's only public surface.
- **Bounded to the controller the path names; answers nothing when that class does not exist.** Not
  its ancestors — `@user` is set in `ApplicationController` in a real application, and walking up
  would find it and then fail to type it anyway, because what it is assigned is an ActiveRecord
  chain. Not something similarly named either: `declared` is an exact lookup, and a template under
  `app/views/comments/` with no `CommentsController` falls to the rung below rather than reaching for
  `StoriesController`.
- **`cursor::assignments_in` returns every assignment and lets the graph pick.** The same-file path
  takes "the textually last that produced a shape", which it can, because a shape is all `cursor` can
  see. Across a file boundary "produced a type" is a question only the graph can answer — an
  assignment naming a class nothing declares has to fall through to the one above it — so the list
  comes back in file order and `from_controller` walks it in reverse.
- **Which `@story` the controller means is still `scopes`'s question.** `scopes::writes_to` is
  `variable` entered by name instead of by offset, because a template has no cursor in the file it
  needs to ask about. A `def self.` and a `class << self` hold a different variable of the same name
  and are excluded there, exactly as for a cursor.
- **The controller's text is read through the buffer, not off disk.** `Analysis::text_of` goes
  through `with_text`, so a controller being edited types the template it renders before it is saved.
  It is the only accessor in `analysis` that clones, and it is reached only when a receiver in a
  *template* was nothing but a name.
- **What the last two rungs buy, over a real application.** The view↔controller convention types
  receivers only in templates, and most of those find the method the call names; the rest are
  ActiveRecord attributes with no `def`. The name guess types many more receivers than it changes
  answers for. In a hand audit of the changed ones, **every one named the class the code means**
  (`@user`, `@story`, `comment`, `exception`, `time`, `object`, `tag`) — as much a result about the
  member lookup as about the guess.

## What the repository declares about itself

- **Generated RBS feeds the same table by the same route, and that is the only route.** The schema,
  the model macros and the annotations all end at `Types::harvest`, where Ruby's own signatures and
  every gem's `sig/` also end. There is no rung for "a column", none for "a `belongs_to`", none for
  "a `sig` block" — `types.rs` cannot tell them apart and must not learn to. `synthesized.md` has
  each generator's rules.
- **A generated answer is `derived`, never `resolved`.** The schema is ground truth about the
  database and an inference about the Ruby; a macro is what ActiveRecord will do at run time; a `sig`
  or a tag is what somebody believed. All three are correct-if-true, and the card says which — through
  the *generated comment*, so no module here learns a Rails word.
- **`Types::harvest` answers whether the document parsed, and exactly one caller cares.**
  `synthesized::Synthesized::record` is the one place this crate feeds the table text it wrote itself,
  and a rejection there is this crate's bug rather than somebody's malformed file.
- **Arity is what makes a generated signature safe to write.** An answer is partitioned by how many
  positional arguments the *call* wrote, so a generated `def` claiming the wrong arity does not
  display wrongly — it answers nothing, or answers for a call nobody made. That is why the
  annotations reader renders every parameter shape Ruby has, and why anything it cannot render
  exactly falls back to `(*untyped)`, which is variadic and fits every arity.
- **A chain can start on the model as well as be followed off it.** `Story.first`, `Story.where(...)`
  and `Story.find(1)` type, because `rails::class_side` writes the relation's own names onto the
  model's singleton from the same list `rails::relation` reads. No rung is added, and `types.rs`
  cannot tell the text from `vendor/rbs` — `synthesized.md` has the bound, which is that a name may
  be declared on the class side only if the relation already declares it.
- **Two conventions with no macro at all declare on the class side.** `UserMailer.welcome(user)` and
  `DigestJob.perform_later(id)` are members because Rails installs them, read from a `def` and a
  superclass rather than from any call. More generated RBS through `Types::harvest`; the *only* thing
  in it `types.rs` could notice is that `ActionMailer::MessageDelivery` is a class this crate wrote
  when the gem is not indexed — `synthesized.md` says why that is safe and what it costs.
- **What the schema buys — and the metric is not a receiver count.** The schema types no new
  receivers; it converts existing misses into hits. Swept over every `.member` position of a real
  application outside `db/schema.rb`: **positions are gained and none is lost.** Rather more answers
  come from the schema than were gained, so the difference **was already answering and was wrong to
  be trusted**: `user.username` used to return a name-based candidate list
  (`ActionMailbox::Relayer#username`, `Faker::Internet.username`, a run of `Net::IMAP`
  authenticators) and now resolves to the column. **The value is as much in the answers it replaces as in the ones it
  adds.**
- **A variable whose assignment produced a shape carries its own spelling, so a failed chain falls to
  the last rung instead of to nothing.** Without it, `story = Story.published.first` then
  `story.comments` answers nothing while a bare parameter named `story` reaches the name rung and
  answers `Story` — writing the assignment makes the answer *worse*. `cursor::type_the_local` and
  `type_the_instance_variable` wrap what they found in `Receiver::Spelled { was, name }`, and
  `types::method_receiver` asks `was` first and `name` only when it came back empty.
- **The fall-through is a step, not a sixth rung, and three things follow.** The `or_else` is
  one-directional, so a chain that resolves can never be displaced by a guess. What it reaches is
  `types::named`, the same pair of rungs a bare name reaches, so the answer wears the *guessed* label
  and `[types] guess_from_names = false` turns it off with the rung it belongs to. And an instance
  variable wraps the whole `Assigned`, so a chain that types keeps the note naming the line.
- **What those two buy is a tier rather than a count.** Swept over a real application's `.member`
  positions the *answered* total does not move at all, while **many positions change tier and none
  for the worse**: the fall-through moves positions from a name-based list or single name match to
  *guessed*, and the model's class side moves more again, a portion of those from *guessed* to
  *derived*. The two overlap in part and are independent in part — the ordering argument measured
  rather than asserted.
- **Neither reaches a position that was silent, and a sixth of the corpus stays silent.** That
  residue is a receiver whose chain runs through a method that nothing in the project, in a gem, in
  Ruby's signatures or in anything this crate generates declares a return type for. No rung in the
  five reaches those; that is the honest statement of where this tier's ceiling is.

## The chain that starts at `self`

- **A receiverless call is an implicit `self` and must not be read as a *name*.** Read as a name it
  is rung four of five, so `api_key_scopes.first` on a model guesses at a class called
  `ApiKeyScopes`, finds none, and offers a page of possible definitions — while `self.api_key_scopes.first`
  one word longer resolves to `ApiKeyScope::Relation#first` exactly. `cursor::returned_by`'s
  no-receiver branch returns `Returned { on: SelfObject, .. }`; `Receiver::SelfObject` is the variant
  a *written* `self` produces, and `types::method_receiver` types it as the enclosing class. No rung
  is added and nothing new is declared.
- **The name rung is kept below the lookup rather than beside it**, which is what
  `Receiver::Spelled` already existed for: a bare name becomes
  `Spelled { was: Returned { on: SelfObject, … }, name: method }`, so `types` asks the graph first
  and reaches `named` only where that answered nothing.
- **The `arguments().is_none() && block().is_none()` guard was a bound on a guess and is not a bound
  on a lookup.** `find(id).title` was excluded because the *word* `find` says nothing about what it
  returns — true of the guess, false of the graph. A call with arguments now produces the bare
  `Returned` with **no name under it**, so the guess stays as bounded as it was and the chain runs on.
- **A write whose value may answer nothing must not displace one that produced a type, and a call on
  `self` is such a write.** `Foo.bar` names a class the file names and a receiver every reader can
  check; `self` in an RSpec block, a rake task or a top-level script is `Object`, where
  `create(:story, title: "…")` resolves to nothing. Read as solid it displaced
  `s = Story.find(s.id)` written ten lines above, and **dozens of positions went down with it**,
  every one in a spec.
- **So the assignment loops keep three slots, and every clause of the third came from a corpus rather
  than from reasoning**: `solid`, then a write relaying a **block parameter**, then one **rooted in a
  call on `self`**. Each clause of the third is paid for by a position it fixed:
  - it walks **down the chain** rather than reading its last link, because one corpus writes
    `tokens = user_tokens(account, …) + contact_tokens(…)`, which at the top looks as solid as
    `Foo.bar.baz` and displaced the `tokens = [user.pubsub_token]` above it;
  - it **stops at a variable**, because a corpus writes `link = c.links.last` where `c` is itself a
    call on `self` — that chain is *`c`'s* problem and `c` carries its own name rung, so reading
    `link` as weak let a String literal in another `it` block take it, two positions the other way;
  - it sits **below `yielded_to`**, because a corpus writes `uploader = upload_image(a, b)` in
    one method and `Uploader.new.tap do |uploader|` in the next, and `type_the_local` deliberately
    treats the two names as one variable — so a write in another method was taking `uploader` away
    from the block the cursor stands inside.

  The `Spelled` a *bare* receiverless call carries is unwrapped once at the top and never followed
  again, because that wrapper is the same call rather than a variable mid-chain.
- **`type_the_instance_variable` keeps the same three slots**, so a local and an instance variable
  cannot disagree about which write speaks for the name.
- **The static bound counts one thing and a sweep moves more.** The bound counts positions whose
  *receiver* is a receiverless call; what moves is everything downstream of one, since a chain that
  starts resolving types every link above it. Swept over five applications it moves **more than the
  bound, worse nowhere, and laterally almost nowhere** — and the largest corpus, which holds much of
  the bound, is not in that sweep at all.
- **The collection predicates carry which side each name is on, and `QUERYING_METHODS` decides it.**
  `size`, `length` and `empty?` live on `ActiveRecord::Relation` and are delegated to nothing, so
  `Story.size` raises — as do `to_a` and `each`, which are likewise not on the singleton. Nine names:
  `size`, `length`, `empty?`, `count`, `exists?`, `any?`, `none?`, `many?`, `one?`. The `?{ }` blocks
  are Rails' own: `posts.any? { |post| … }` reaches `Enumerable#any?` through `super`, so the block
  is handed an element, and being optional the arm applies whether or not one was written.
- **One approximation is known and stated**: `count` after a `group` returns a `Hash` and this says
  `Integer` unconditionally — the same order of inexactness as `where` always returning a relation.
  Measured alone over five corpora: **better in bulk and worse nowhere**, with the *answered* total
  barely moving. What moves is a name-matched candidate list becoming a typed answer.

## When a better-known receiver makes the answer worse

- **A `Namespace::Todo` is not a class object, and that has to be said explicitly.** rubydex spells a
  namespace it never saw a definition of `Todo`, and upstream **promotes a constant used as a
  receiver into one** — so `ENV` and `URI::RFC2396_PARSER`, which hold an *object* rather than a
  class, stopped being `Declaration::Constant` and acquired a singleton class. Its ancestors are
  `Class`, `Module` and `Object`, so `ENV.` answered `alias_method` and `attr_accessor`: **precise,
  wrong, and enough to displace the name-based list** that used to answer.
  `method_receiver`'s `Receiver::Constant` arm asks `is_todo` before `singleton_of` — the same test
  `hierarchy` and `search` apply. Deliberately **not** folded into `singleton_of`, which
  `completion`'s `Distance` also calls to *rank* rather than to decide what a receiver is.
- **It has happened four times.** A corpus' `# @return [Object]` types a receiver as a class with no
  members where the guess had been right; the `Todo` above is the second; and a run of positions in
  one corpus **gain a type and thereby lose the name-matched list that held the word**. The third is
  not a defect to fix here: most of those are columns on a corpus with **no `db/schema.rb`**, so the
  type is known and nearly empty, and a corpus that has one loses none of that shape. **When a rung
  is added, ask what it *displaces* as well as what it answers, and ask it on the completion sweep**:
  `sweep.tier` scores every one of them as an improvement, because `list -> derived` is a rank it
  counts upward.
- **The remaining two have no repair available.** A variable with **two assignments naming two
  classes** picks one: a corpus writes a `Status` or a `ScheduledStatus` into one `@status`
  depending on a branch, and once `Relation#new` and `Relation#create!` existed the assignments
  resolved, the one that speaks names `ScheduledStatus`, and `poll`, `quote`, `reply?` and
  `in_reply_to_account_id` — all `Status`'s — fell from a right guess to a name-based list at a
  handful of hover and completion positions. And a receiver typed **correctly** to a class this crate knows
  almost nothing about does the same: `Doorkeeper::Application.find_by(…)` and `Audited::Audit.last`
  answer precisely, a gem's model has no columns here because its table is not the one the table-name
  rule would claim, and `redirect_uris`, `uid`, `secret` and `action` stop being offered. YARD's
  `Object` can be declined because it means "anything"; `ScheduledStatus` and
  `Doorkeeper::Application` are real classes really named by the file. The repair for the first is to
  decline the assignment rung where the assignments disagree about the class — a corpus-wide change
  needing its own sweep. **The second is the standing price of a class side that reaches into the
  bundle**, and its only repair is "know the gem's columns".
