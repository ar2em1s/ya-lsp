---
paths:
  - "src/analysis/locator.rs"
  - "src/analysis/cursor.rs"
  - "src/analysis/symbols.rs"
  - "src/analysis/hover.rs"
  - "src/analysis/render.rs"
  - "src/analysis/requires.rs"
  - "src/analysis/search.rs"
---

# Navigation, outlines and rendering

## Tiers and confidence

- **Hover and go-to-definition answer three tiers, and the card says which.** Between "rubydex
  named the receiver" and "matched on the method name alone" is a rung for a receiver ya-lsp typed
  itself, from an RBS return type or an assignment. Both requests go through
  `locator::resolve_typed`, which is why they cannot disagree; `references`, the type hierarchy and
  `rename` deliberately do not. `types.md` has the whole of it, including why a *local* assigned a
  literal carries no footnote while an instance variable does.
- **An instance variable is the scope walk's, and all three requests ask the walk before the
  graph.** rubydex files an instance variable's assignment as a declaration and files no
  reference to one, so `locate` finds nothing at a read and finds the declaration itself at a
  write — neither of which is what `@title` means. `locator::variable_at` hands back the span
  the cursor is in and every write sharing that `self`; `resolve_variable` types it through the
  same `cursor::Receiver` `@title.upcase` goes through, so a card on the variable and a card on
  a call on it cannot disagree about the type or the rung. **The order is the invariant, not a
  preference**: `@name = 1` is the one span both halves can answer for, `highlight.rs` settled
  years-of-reading ago that the walk wins there, and three requests asking in two orders is the
  bug lane 2 of the audit is built to see. `hover` alone falls through when nothing could type
  the variable — at a write the graph still holds `Shelf::Book#@title`, and a card naming the
  variable is better than none.
- **The *places* are not rebased and may not be; the *type* is, and must be.** The writes were
  read out of the buffer the client is holding, in the file the cursor is already in, so they
  arrive in the coordinates the reply is sent in; `documentHighlight` answers from the buffer for
  the same reason and it is the same walk. Two things in the same answer do go in through the
  map, both because they read the graph: the nesting a guessed constant is resolved in, and the
  `Receiver` the assignment produced — `types.md` has that one. The line the card names for the
  assignment stays the buffer's, because it is a place a reader goes to rather than a key. **A
  template's writes are that same rule with a second text**: they are read out of the
  renderer's buffer and measured against the renderer's buffer, so nothing crosses the map
  there either.
- **A template's instance variable is written in another file, and `definition` says so.** A
  template has no enclosing class, so `locator::variable_at` searching the buffer the cursor is
  in finds nothing by construction — measured over the six corpora at their pins, the card
  answered **759 of 1,301** template reads while the jump answered **4**. It answers **870** of
  that same draw now, and the card **788** — the second half of the convention, two bullets
  down, is what moved both again.
  `requests::template_variable_links` is the walk continued into the one document those writes
  can be in, and it takes the class from `types::renderer_documents`, which is the function the
  card's rung takes it from too: one convention read once, so a card citing `StoriesController`
  and a jump landing somewhere else is not a state this can reach. **Every write, not the typed
  ones** — `cursor::assignments_in` drops `@stories = Story.where(...)` because nothing declares
  what that returns, and that line is exactly the place a reader asked for, so the jump reads
  `scopes::writes_to` one filter earlier. **The tier does not move**: the class a path implies is
  the fourth rung and a Derived answer, and a jump built on it is not promoted for having a
  `Location`.
- **A partial no single controller renders answers nothing, and that is the ruling.**
  `rails::controller_of` reads the directory and not the file name, so `stories/_story.html.erb`
  names `StoriesController` exactly as `stories/show.html.erb` does, and `shared/_header.html.erb`
  names a `SharedController` no file declares — **542 of those 1,301** reads are in templates of
  the second kind. *Several possible writes in several files* is not a wider answer than *one
  write in one file*, it is a different one, and nothing in a path ranks the controllers that
  render a shared partial. So the path names one class or it names none, and the refusal is
  `Views::rendered_by` coming back empty — the same line the card already refuses on. **129 of
  those 542 do answer now**, and none of them is a partial: they are a mailer's views, which the
  bullet below reaches by a second convention rather than by ranking anything. What is left is
  layouts, whose directory names nothing, a gem's own mailer, and partials under a directory no
  one class is named after.
- **A mailer's views hang off the mailer, and the jump follows it there.** `ActionMailer::Base`
  derives its view path from the class it renders for, so `app/views/user_mailer/welcome.html.erb`
  is `UserMailer` and there is no `UserMailerController` anywhere — mastodon answered **0 of its
  101** template reads before this and **97** after, because every `.erb` that application ships
  is a mailer view or a layout. The controller is tried first and the mailer only where there is
  no controller, which is Rails' own order; the mailer half is gated on the classes the
  application defines that `rails::is_mailer` recognises, because a directory spells whatever it
  spells and only a controller's name is unmistakable. Both halves are one function —
  `views::Views::rendered_by` — read by the card, by the jump and by the view context, so one
  path cannot resolve to three classes. `views.md` has the gate and `types.md` the rung.
- **A local is not this question and both requests say so.** `person = Person.new` is a line the
  reader can see from where they are standing. That leaves a local as the one cursor in an
  ordinary file where `documentHighlight` answers and `definition` does not, which is a gap
  rather than a decision — it is pinned in `a_local_is_not_this_question_and_both_requests_say_so`
  as a lowercase `w` with no jump on it, so closing it is a choice somebody makes rather than
  something that drifts shut.
- **A macro's `:symbol` is a member of the class it is written in, and that is the whole rule.**
  rubydex records a call and never its arguments, so a symbol is a cursor the graph holds nothing
  for at all. `cursor::macro_symbol` finds it by syntax and no vocabulary — *a receiverless call
  written straight into a class or module body*, which is what a macro is in Ruby — and
  `locator::resolve_symbol` looks the name up in the enclosing class's ancestors, instance side
  first and the class object second. That one lookup answers all twenty-three macros the audit
  draws from and every DSL nobody has taught this crate about: `before_action :authenticate` is a
  `def` above it, `validates :title` is the column `db/schema.rb` declared, `belongs_to :user` is
  the reader `workspace/rails/associations.rs` wrote. **No macro table lives in `analysis/`**, and
  adding one would be the first Rails word outside `workspace/rails/`.
- **Positional arguments only, and a name no ancestor declares is declined rather than guessed.**
  `dependent: :destroy`, `on: :create` and `only: [:index]` configure a macro rather than naming
  its subject, and telling `to:` from `dependent:` is exactly the Rails knowledge `cursor` does
  not have. `:desc` inside a `scope`'s block and `:draft` inside an `enum`'s array are the same
  exclusion one level down. What is left over — `validates :absent` — answers **nothing**: the
  only rung below an ancestor walk is a name match, and a jump out of a `validates` line into
  somebody else's `def` is a wrong answer the reader cannot see is wrong.
- **The symbol is asked after the graph, and the instance variable before it.** Opposite orders,
  one reason: at `@name = 1` the graph answers and answers *wrongly*, while at a symbol it does
  not answer at all. Where it does — `attr_reader :count` files a definition whose name span is
  the symbol — its answer is the declaration the buffer walk would have found anyway, already
  carrying the reference machinery. `documentHighlight` asks in the same order and adds the
  symbol's own span, so `definition` never lands somewhere nothing lit.
- **What a symbol names is rebased and what a variable names is not.** A symbol resolves to a
  declaration in the graph, which may be in another file entirely — a column is in
  `db/schema.rb` — so it goes out through `locator::sites` and `link` like every other target. An
  instance variable's writes never leave a buffer; the template bullet above is the one case
  where the buffer is not the one the cursor is in.
- **A hover footnote is about ya-lsp's confidence, never about the code.** Three kinds: nothing (the
  code names the type), what was derived and where from, and the guess. The offset-to-line
  conversion for an assignment happens in `analysis::mod`, where the document is; `hover::markdown`
  renders the line it is handed and knows nothing about encodings.
- **"The receiver has no type" and "the receiver has no such member" are two footnotes, not one.**
  `locator::typed` falls through to the name rung in both cases and for two releases said the first
  for both — while `completion` at the identical cursor went on offering that class's members,
  because it is built *from* the receiver and never reaches a member lookup at all. The class is
  carried back on `Resolution::missed` and `hover::why_guessed` picks the sentence. **The tier does
  not move**: a list matched on a name is a guess whichever of the two put it there, and
  `precise` is still `false` — this only says which, so that the half with a class in it can be
  checked. Measured before the split, over six corpora, counting cards that said the type was
  unknown while the completion list at the same cursor was a class's own members: **207 of
  1,506** at an instance variable and **180 of 209** at a class object.
- **The class named in that sentence has to be one a reader can open.** `Namespace::Todo` is
  refused — it is what rubydex invents for a constant no file defines, so every lookup on one
  misses by construction — and so is any name `render::is_nameable` rejects, which is the same
  test the symbol picker applies. A singleton is kept rather than refused, because the class it
  hangs off *is* spellable: `render::class_object_of` is the third caller of `singleton_parts`
  rather than a second copy of the rule, and `Missed::class_object` is what keeps the sentence
  from calling a class object an instance of itself.
- **A hover card is the answer, then what ya-lsp knows about the answer.** Fence, rule, prose, then
  footnotes — one italic line each, at the bottom. `hover::footnote` is the one place the shape
  lives, because it was two: a guessed single match carried the caveat as a trailing italic and a
  guessed *list* carried the same sentence inline, in bold, at the top, after an em dash. Four cards
  are pinned whole in `analysis::tests` — a core method, a stdlib method, a reopened class, a
  name-only match — the way `ANCESTRY` pins the first ten completions, because composition drifts
  while every part still passes a `contains`.
- **Hover cards are pinned side by side, in one file, whole.**
  `every_hover_card_in_one_file_drawn_side_by_side` renders every construct in `GALLERY` with its
  card under it. Before it existed, each construct had a `contains` somewhere and no two cards were
  read next to each other — which is how `class << Book` came to sit above a
  `private Shelf::Book#hide`: `attached_name` read the attached class out of rubydex's
  `Shelf::Book::<Book>`, where it is spelled unqualified, and the one test covering it used a
  *top-level* module, where qualified and unqualified are the same string. **Take the part before
  the `::<`, never the part inside it.** A new card or construct goes in that fixture.

## rubydex's spellings

- **rubydex invents constant references you must not follow.** Every call with an implicit or
  constant receiver gets a fabricated reference to `<Foo>` so the singleton class can be resolved.
  `Person.new` therefore has two references over the same six bytes, and for an *implicit* receiver
  the invented one spans the **whole call** — so `alias_method :a, :b` carries a reference resolving
  to `class << self`. Angle brackets are rubydex's only spelling for singleton names;
  `analysis::locator` drops references starting with `<`.
- **rubydex records an anonymous `*`, `**` or `&` under the sigil itself**, not under an empty name,
  so `render`'s `sigil` writes it once. Prepending unconditionally spelled `def f(*, **, &)` as
  `(**, ****, &&)`, which `.new` hovers made visible on every constructor taking one.
- **A qualified name has to be put back together to be looked up.** `constant_path` walks the chain
  of `ParentScope`s rubydex interned the reference as, because the graph holds only the last
  segment's string on each `Name`. `resolve_outwards` is the lexical half of Ruby's constant lookup,
  the same pair `types`' guess rung uses.
- **rubydex is what makes "is `self` a class object here?" one question.** A receiverless call
  written as a statement of a class or module body records the *singleton* as its receiver; the same
  call inside a `def` records the class itself. Both halves below turn on `attached_class`
  answering, so neither needs a syntactic test and neither can disagree with the other.

## `Foo.new` and concerns

- **`Foo.new` is answered with `Foo#initialize`, and the redirect is marked.** `Class#new` is the
  exact answer and a useless one — on a Rails app **every** `.new` site hovered as
  `Class#new(*args, **kwargs, &block)` in `core/class.rbs`. `locator::constructor` steps off it
  through the *attached* class, which is the singleton's `owner_id` and not a name taken apart. It
  stands aside twice: a hand-written `def self.new` is what the call really reaches (rbs declares one
  for `Struct`), and `BasicObject#initialize` is what every object inherits rather than a constructor
  anyone wrote, so a class with neither keeps `Class#new`. Both guards ask the *owner* of what was
  found.
- **`Resolution::redirected` exists because `references` must not take the redirect.**
  `def initialize` is not a declaration of `new`, and with `includeDeclaration` it was listed as one
  for every class whose constructor is in the user's code. Navigation and
  `completion::precise_call` want the redirect — a constructor's keyword arguments really are what
  `Foo.new(` takes. Anything new reading `Resolution` has to decide which it is.
- **The concern edge left this module on 2026-09-15, and what replaced it is a declaration.** Until
  then `locator` held `const CLASS_METHODS: &str = "ClassMethods"` and a walk built around it —
  the one Rails word in the crate outside `workspace/rails/`. `ActiveSupport::Concern
  #append_features` ends with `base.extend const_get(:ClassMethods)`, Rails writes the `include`
  statically and never writes that `extend`, so the singleton lookup correctly found nothing and
  `validates`, `scope`, `belongs_to` and `has_many` in a model body answered on the name rung.
  `workspace/rails/concerns.rs` now writes each of those `def`s onto the singleton of every class
  that includes the concern, so an **ordinary ancestor walk** finds them. `synthesized.md` holds the
  generator's rules and the measurement that forced its shape.
- **The `extend` could not be declared, and that is a rubydex defect rather than a preference.** The
  obvious shape is one `module <concern>::ClassMethods` and one `extend` per includer. A mixin that
  arrives in a document indexed *after* its class was resolved is **never linearized**, and a
  generated document is always that shape — so the member stays unreachable. Measured twice: as the
  refused half of the query interface (`synthesized.md`), and again here across every spelling, in
  RBS and in Ruby, including `class << self; include M; end` and `singleton_class.include M`. A late
  **`include`** *is* linearized, which is why the route-helper module works and why this is an
  asymmetry rather than a limit. The two code sites are `resolution.rs::handle_definition_unit`,
  which sets `needs_linearization` only when the declaration is *created*, and
  `get_or_create_singleton_class`, which returns an existing singleton without scheduling its
  ancestors. Re-indexing the class's own file afterwards does not repair it.
- **What stayed is a repair with no Rails in it.** `locator::extended_member` and
  `locator::extended_modules` walk the modules a class object's own bodies `extend`, resolved by
  name, for the one shape rubydex does not linearize — `extends_written_on`'s four-row table, whose
  commonest cost is `SecureRandom.hex` through `stdlib/securerandom`'s `extend Random::Formatter`.
  It is reached only after the ordinary ancestor search came back empty, so it can only ever add.
- **What is taken from such a module is what it declares itself, never what its ancestors declare.**
  `extend M` really installs the instance methods of `M`'s ancestors, so an ancestor walk is the
  right reading of Ruby and the wrong reading of the graph: rubydex records an `include` written
  **inside a `def`** as a mixin of the enclosing namespace, which is exactly how Rails writes the
  ones reaching a `ClassMethods` module — `def has_secure_password; include ActiveModel::Validations;
  end` means the *record's* ancestors at the moment the macro is called. Over Rails' five core gems
  and the six applications, **those modules hold a couple of mixins in the module body and rather
  more inside a `def`**, and every one of the latter means the class the macro was called on. The
  applications write none of either. Following them made `Story.valid?` — which raises in Ruby —
  resolve precisely to `ActiveModel::Validations#valid?`. **The rule survived the move as a
  rendering rule**: the generator writes each module's own members and never an `include` of it,
  which is what a declared `extend` would have lost.
- **It is asked after the ordinary ancestor search and only when that found nothing** — the rule
  `resolve_typed` already holds for a derived receiver: a worse answer may never displace a better
  one. A class writing its own `def self.validates` keeps it, and so does a class whose own file
  declares a `def self.` a concern also installs: the two merge into one declaration and
  `locator::places` prefers the `.rb`.
- **`textDocument/completion` follows the same walk, and `locator::extended_modules` makes it one
  gate rather than two.** Resolution takes the first module answering one member; completion
  collects every member of all of them. Both read the same list in the same order, so a change to
  what the repair covers cannot land on one half only. `completion.md` has the collecting side.

## Narrowing the name-based fallback

- **A private declaration is not an answer where Ruby would not let the call be written, and the
  gate reads the syntax because nothing else can.** `locator::Privacy` is the value, and it is a
  parameter rather than a lookup: rubydex's `MethodRef` carries `Option<NameId>` for the receiver
  and fills that name *both* for `Foo.bar` and for a bare call in `Foo`'s own body, which are
  opposite answers to this question. `cursor::Context::allows_private` is the one thing that knows
  — an implicit receiver, or one spelled `self`, through `.` and `::` alike — and it was
  `completion`'s alone for two releases while `hover` and `definition` resolved through a rung that
  never asked. The cost was measured from the outside: **90 `absent-resolved` rows over six
  corpora**, a *Resolved* card followed by a completion list that refused the same member, and 88
  of them one cursor — `RSpec.describe` on the first line of a spec file, answered with minitest's
  `private Kernel#describe` out of `vendor/rbs/stdlib/minitest/0/kernel.rbs`.
- **All three rungs, or the refusal has a side door.** The resolved member, the derived one and the
  name list. A declaration the ancestor walk declines is matched by its own name like any other, so
  gating only the rung that found it hands the same `def` back one tier down as a *Guessed* card —
  which is the identical wrong answer wearing a weaker tier. Where that leaves nothing, **nothing
  is the answer**: rspec-core defines `RSpec.describe` dynamically, there is no `def describe`
  anywhere in the gem, so the correct card is no card and not a better one.
- **The refusal is a re-resolve and never a strike-through.** What the gate changes is which rung
  answers: refusing the ancestor hit sends the call on to the extend repair and then to the name
  rung, either of which may hold a public answer the private one was standing in front of. Emptying
  the finished `Resolution` instead would leave `precise` true with no declarations, which reads to
  every caller as a resolved *has no such method* rather than as the fall-through it is.
- **`Foo.new` is exempt and that exemption is the whole risk in the change.** It resolves to
  `Foo#initialize`, which Ruby privatises by name at the point of definition, at a receiver that is
  written and is never `self` — so a gate reading visibility off the redirect would refuse every
  constructor in every workspace. `holds_private` skips a redirected resolution, and Ruby's
  five always-private names stay `completion`'s list for the same reason.
- **What the footnote may not say.** A lookup that came back empty means the class has no such
  method; a lookup the gate refused means it has one and Ruby will not let it be called here. Those
  are different facts, so `locator::Missed` carries which — printing *has no such method* for the
  second would be introducing, on the way out, the exact shape of false sentence the gate exists to
  remove.
- **rubydex's record is wrong for one ordinary shape, and the gate repairs it rather than working
  around it.** A bare `private` is a *statement*, and rubydex applies it to the body it is written
  in until that body ends — a block is not a body to it. `class_methods do … private … end` therefore
  sets the *module's* default visibility, and every `def` written below the block is recorded
  private: discourse's `HasCustomFields#upsert_custom_fields` is public, is called on explicit
  receivers in shipped code, and the gate refused all of them the first sweep after it landed.
  `locator::Modifiers` rereads the declaring document and overturns the record where a bare
  modifier is the whole reason for it.
- **Ruby agrees with rubydex for one kind of block and disagrees for the other, and nothing in the
  syntax says which.** A bare `private` sets the visibility on the *cref*; a plain iterator block
  shares the cref it was written in, so `[1].each { private }` really does privatise the `def`
  below it, while `module_eval` and `class_eval` give the block a cref of its own, so
  `class_methods do`, `concerning`, `included do`, `Class.new do` and every RSpec example group do
  not. Telling them apart means knowing what the method holding the block does with it, which is a
  fact about a gem. **So the rule is the one that does not refuse: a bare modifier governs only the
  body it is written in, and a block body is a body.** The census says the shape it is wrong for
  does not occur — over 24,054 Ruby files in the six corpora, 49 `def`s in 9 files are recorded
  private by a modifier that escaped a block, and **every one of those blocks is an eval form**:
  `class_methods do` at 20, an RSpec group or a `Conversion::Step` DSL at the other 29. A refusal
  is an assertion, and an assertion is not a thing to make from the reading that cannot be checked.
- **It narrows a refusal and never widens one.** A `public` written inside a block escapes it just
  as far, so a genuinely private `def` below one is recorded public — and nothing in `Modifiers`
  touches that. A missing refusal is not the same defect as a false one, and this type exists to
  stop the gate asserting what the source does not say rather than to assert more than rubydex did.
- **One wrong record, two surfaces, one repair — `completion::reachable` reads it too.** A jump
  that lands on a member the list at the same cursor will not offer is the disagreement the tier
  system cannot survive, and it is the shape the audit's `absent-resolved` key measures. The
  repair is bounded there: the receiver path gets it, `completion::by_name` does not, because that
  one walks every method in the graph before deciding whether to answer and the repair costs a read
  of each declaring document. The two paths are alternatives at any one cursor, and the name path's
  output is the *Guessed* tier.
- **`signatureHelp` is gated and the three surfaces below are not**, and the difference is whether
  the caller has a cursor to read. Signature help holds a `cursor::Call` from its own parse and
  passes `Privacy::written(call.allows_private)`, because the card and the jump answer one cursor
  and may not disagree — `a_private_signature_is_not_drawn_where_the_jump_has_nowhere_to_go` is the
  assertion. The ungated three: completion's keyword arguments are at an implicit receiver by
  construction, so `Allowed` is the right answer rather than a missing one; the outgoing call
  hierarchy walks a body's call sites with no cursor at all; and `references`, `rename` and both
  hierarchies' preparation go through `resolve`, which answers *where is this used*, where a use of
  a private method is a use.
- **The fallback is filtered when the receiver is a class object, and the argument is the ancestor
  chain, not a heuristic.** `locator::reachable_on_a_class_object` drops a candidate whose owner is a
  `class`: what a class object answers is its own singleton chain — the singleton classes of its
  ancestors, plus whatever is `extend`ed onto it, plus the instance methods of `Class`, `Module`,
  `Object` and `Kernel` — and rubydex puts every one of those in the singleton's *own* ancestors,
  which the search above already walked without finding the member. So a surviving class-owned
  candidate is provably unreachable, while a module-owned one is exactly what must stay: a concern's
  `ClassMethods` is a module's **instance** method. The list is narrowed, never emptied.
- **Swept twice, because the demand sits in two kinds of position: better in bulk, worse nowhere.**
  Most of the gain is at receiverless class-body calls, the rest at `.member`. Every lateral move is
  `list -> name`, never the reverse: the rank cannot see a long candidate list collapsing to the
  right single answer, because that is the same rung. **Every per-corpus zero is a bundle that is
  not installed** — a corpus pinning an `activerecord` the measuring machine does not have moves
  nothing — so every number is a floor, and the corpora with installed bundles are the shape of it.
  **What still moves under a missing bundle is the application's own concern**: a corpus with no
  `Gemfile.lock` still moves on its own `scope`s and its own `ClassMethods` modules.
- **A receiverless call in a class body resolves precisely, and the Rails macros are the known
  exception — the *fallback* is what is wrong there, not the scoping.** Measured against a real
  application's own models with its bundle indexed: a `def self.` on the same class, one on a
  superclass, and a
  method of a module the class statically `extend`s all answer *resolved*, and completion offers
  exactly those. `validates`, `has_many`, `belongs_to` and `scope` do not, because the edge that
  would put them on the singleton is runtime reflection no file states. So the name rung firing there
  is the **fallback working as designed**, and "the name search is not scoped" describes a symptom of
  a missing edge. The half belonging here is the second route: the candidate list is not filtered by
  what such a call could reach, so `scope` offers a long list headed by
  `ActionDispatch::Routing::Mapper::Scoping#scope`, and an **instance** method of an unrelated class
  can never be what a class body's bare call reaches.
- **The same filter is wrong at a cursor inside a *block*, and there it was throwing the right
  answer away.** A block is a value: whoever receives it may run it against something else, and
  every DSL that takes one does — `rule(:colon) { str(':') }` against a parser instance,
  `scope :recent, -> { where(...) }` against a relation. rubydex has no notion of a block, so such a
  call carries the same singleton receiver a statement of the body carries, the singleton walk finds
  nothing, and `reachable_on_a_class_object` then drops every candidate a `class` owns — a correct
  rule about a class object, applied to a cursor whose `self` is not one. An inherited instance
  method was discarded and a same-named module method kept in its place, which is a **wrong** answer
  rather than a vague one.
- **`locator::in_a_closure` is the rung, and it needs both halves of its evidence.** The graph
  half: the attached class has the member on its instance side. The syntax half:
  `cursor::closure_in_a_body` says a block or lambda stands between the cursor and the class or
  module body it is written in. Both have to answer because either alone is a guess — together they
  say the name is absent from the class object and present on an instance, so a file that meant the
  class object there would not run. It fires **only where the answer was already a name-matched
  list**, so the rung it replaces is the one rung allowed to be wrong.
- **The syntax half rides on the `Cursor` and is a walk rather than a parse.** It used to be
  ordered last, because it read the file a second time and the graph question costs a walk that was
  already half done; a completion list built on the same evidence (`completion.md`) would have made
  that a third parse of the buffer on a keystroke. So `cursor::at` runs `BodyClosure` over the tree
  it has already parsed and `Cursor::in_a_closure` carries the answer to both callers. The ordering
  argument is gone with the cost that produced it, and what is left is a field that is free where it
  is read.
- **A `def` ends the question and a block inside one never starts it.** `self` in
  `def self.run; [1].each { … }; end` is the class object whatever `each` does with the block,
  because the block closes over a method's `self` and that is not up for rebinding; a statement
  written straight into the body is the class object for the same reason. Both keep the guess they
  had. `cursor::closure_in_a_body` is `MacroSymbols::body`'s question asked one step further — a
  macro is a receiverless call in a body and *a block does not stop it being one*, which is the
  same rule read from the other end.
- **A class, never a module, and the reason is the sentence the card prints.** The claim is *the
  block is run against an instance of this*, and a module has no instances: the blocks a module
  body holds are `included do` and `class_methods do`, whose `self` is the **including class** and
  not anything reachable from the module. The name rung keeps the module's own instance method
  anyway — it is module-owned, which is exactly what `reachable_on_a_class_object` does not drop —
  so declining costs a reader nothing and keeps the tier's claim true.
- **Where both scopes hold the name the class object keeps it, resolved, with no footnote.** The
  singleton side is searched first and this rung only ever runs after it found nothing, so there is
  no hedge to make: the file as written would run, and manufacturing a second candidate there would
  put doubt on the one answer the code actually states.
- **The tier is `Derived` and the card names the class.** Nothing in the file says the block is
  re-bound, so `Derivation::closure` carries the class the member was found on and the footnote
  states the evidence rather than the conclusion — a reader who thinks the DSL does something else
  can go and look. Calling it *Resolved* would claim the code named the type, which is the one thing
  it did not do.
- **And the list beside that card holds the same names, which it did not until 2026-09-14.** This
  rung answered one name from a scope `completion` never reached, so a card named a member the list
  at the same byte did not carry — the same self-contradiction check 6 of the audit reads one shape
  over. `completion::InClosure` is the collecting half, and `completion.md` holds its rules; what
  matters here is that the two agree **per member and by construction**, because the list adds a row
  only where the class object answered nothing and this rung is reached only when `resolve_call`
  came back imprecise.

- **The name rung does not leave the application for a test tree, and a cursor already in one keeps
  everything.** `locator::loadable_from` runs once, on the way out of `resolve_typed`, over an
  answer that is not precise: a declaration every one of whose definitions sits under a `spec`,
  `test`, `tests` or `features` segment is dropped. A `def` that only RSpec loads is a `def` the
  application never sees, so a cursor in a model sent to one has been sent where the running
  program has never been — and where the filter empties the list the answer is silence, which is
  the trade this carries rather than hides.
- **A generator's template tree is the second thing the name rung and the root arm drop, and it
  is stricter than a spec.** A spec at least loads under RSpec; a tree `rails generate` copies
  **out** of loads nowhere ever, and the load-path clause cannot say so because the tree sits
  under the gem's own `lib/`. `environment::in_a_generator_template` is the tag —
  a `templates` segment after a `generators` one — and `Fence::unloadable` is where it joins
  `only_the_suite`, so every rung that already fenced gained it without a new call site.
  `loadable_on_a_root` reads it too, unlike the layout: AMS' `serializer.rb` puts a top-level
  `def id` on `Object` and answered **506 of 1,500** drawn `.id` cursors on discourse as one
  place. Measured over 5,859 cursors: 731 lists shrank, 0 emptied, and all 767 places dropped
  were a template.
- **The tag, the rule and the cursor gate are `analysis/environment.rs`, not this module.** A
  deny-list of four directory names matched as path segments, a `Tally` that decides *any
  definition is loadable* and *no definitions is loadable*, and `fenced_from` for the cursor —
  five surfaces read them and a rule answered in five places is a rule with four holes in it.
  `environment.md` carries the whole table of which surface drops, which ranks and which must
  never ask. What stays here is that `loadable_from` is the one call site on the *name rung*, and
  that it runs on the way out of `resolve_typed` rather than inside any rung: the question is
  about the target, not about how the target was found.
- **One precise rung is filtered too, and it is the root arm.** `resolve_call` returns
  `Resolution::precise` for a member found on `Object`, `Module` or `Class` — which is every
  receiver in the workspace, and where rubydex files a `def` written at the top of a spec or inside
  an `RSpec.describe` block. The gate travels in as a parameter (`resolve` passes `false`, in
  writing), the arm asks `environment::loadable`, and a fenced answer **falls through to
  `by_name`** rather than to silence, so the reader gets what the workspace would have answered if
  that `def` had not been written. Measured at 800 bare calls per corpus outside the test trees:
  **31 of 4,251 answered cursors resolved onto a spec-only root member, all 31 wrong, 0 after.**
  `precise_call` takes the same gate, so a signature card cannot keep an answer the jump beside it
  has refused — but it has no name rung, so there the fenced case draws nothing.
- **Nothing else precise is filtered.** A *resolved* answer inside a spec is the code saying so,
  and the answer to that is to read the code. `references` is not
  filtered either, for the reason it does not follow a derived receiver: a work list of places to
  edit that quietly omitted the specs is a rename that breaks the suite.
- **What the dropping surfaces do with it differs from what the ranking ones do, and the
  difference is what a wrong answer costs.** Navigation fences only the imprecise rung and answers
  silence; completion **drops** the row outright, because a suggestion that cannot run is not a
  weaker answer but a wrong one, and because a sunk row still spends a slot under the response
  cap. `workspace/symbol` and the type hierarchy only **sink** it — nothing there is capped by
  the fence and a name has to stay findable. `completion.md` and `environment.md` carry those.
- **`preferred_definition` picks the loadable copy when a name is written in both.** It is a
  tie-break inside `locator` rather than a fence: the row exists either way, and this only decides
  which of a declaration's places it points at. It matters most where the application's path sorts
  after the spec's, which `definitions_of`' own doc already names — an rspec helper first and the
  gem's own file fourth.

## Spans, outlines and rendering

- **`selectionRange` must sit inside `range`, and Prism's error recovery hands out pairs that do
  not.** `locator::spans` is the one place reconciling them, for `DocumentSymbol` and `LocationLink`
  alike — VS Code enforces the first by *throwing*, dropping the entire outline rather than the one
  bad symbol. A bare `def` at the end of a line recovers into a node whose location is the three
  keyword bytes and whose name location is the whitespace after them; a sweep over five files typed
  character by character, at every intermediate state, found that shape and no other. Never build the pair by hand
  from `offset()` and `name_offset()`.
- **A definition with no name in it is not an outline entry** — the visible half of the same
  recovery, a blank row while `def` is being typed. `is_outline_worthy` checks it because
  `document_symbols` already builds every name; `locator::locate` deliberately does not, since a
  string lookup per definition would land on every hover and completion for a transient that only
  shows when the cursor is parked on the whitespace.
- **A definition matches only its name span, never its body.** `Definition::name_offset` is `None`
  for constants, `attr_*` and aliases — there `offset()` is already just the name. Widening this to
  the body makes hover fire over whitespace.
- **`locator::site` is the single point a declaration becomes a place**, which is why generated
  declarations are translated back there. Goto-definition, `references`, the symbol picker and the
  type hierarchy all reach a file through it, so one lookup covers all of them — and a generated
  definition with nothing recorded about where it came from answers `None` rather than handing out a
  link into a document the editor cannot open. `synthesized.md` has the rest.
- **A *place* is narrower than a definition, and `locator::places` is the only function that says
  how.** Four things are definitions and are not somewhere to send a reader: an `.rbs`, which
  declares a method rather than defining it and lands the jump in a stub with no body; a second
  copy of a file the project would never load, which is what a bundle pinning `cgi` makes of the
  copy inside Ruby; and a document whose URI is not a file at all, which is how rubydex spells its
  own built-in `Object` and `Kernel`. Measured over the five corpora: 661 positions offered a
  signature beside the source that defines the same method, 350 carried a shadowed copy, and
  7,205 places went away with **nothing dropped from any project's own tree and no position
  silenced**.
- **And a copy only the suite loads is not a place a reader in application code asked for.** The
  fourth, and the first time `environment.rs`'s rule reaches a *place list* rather than a name.
  solidus offers **539** places for `Spree` and **76** are under `spec/`; the reader in a
  controller loads none of them. Same two safety clauses as the signature rule: kept where no
  loadable place survives, and off entirely where the cursor is itself in a test tree or a
  `testing_support` tree. Measured over 4,501 constant cursors outside the test trees: **413
  positions carried one, 24,143 places dropped, 0 emptied, 0 lists grew**, and the audit's
  *Resolved cards land in a test tree* went **44 to 0**. `environment.md` holds the rule.
- **What `require` can name is loaded, whatever the directory is called.** The tag is four
  directory names written against *the project's* trees, and a gem shipping `lib/rack/test/` is
  publishing a library — `railties` puts `rails/commands/test/` there too. Without that clause the
  fence took **81 lists off lobsters**, a corpus with no project test tree in any place list at
  all. The clause belonged to `places` alone for an afternoon and that gap was its own defect: the
  name rung answered `null` for a method rack-test really declares. Both halves — the cursor gate
  and the `Layout` — are one `environment::Fence` now, so `places` and the name rung cannot answer
  this differently. **The root arm is the one rung that reads the directory name and nothing
  else**, because a hit on `Object` answers for every receiver and a fence loosened there is
  loosened backwards. `environment.md` holds both.
- **A signature stays where it is the whole answer.** 216 positions have an `.rbs` and nothing
  else, and a signature is a better answer there than silence — so the filter is "drop the
  signatures *if* source survives", never "drop the signatures".
- **Which copy of a file wins is the load path's question, already answered.** `Workspace::load_paths`
  is ordered the way `require` searches — the project's own entries, then the bundle's gems, then
  Ruby's own library last — so `places` keys each candidate by its path *under* a load path and
  keeps the earliest. A candidate under no load path has no such key and is never shadowed, which
  is what keeps two files of the same name in `db/` and `script/` two real places.
- **`references` deliberately asks `locator::sites` and not `places`.** Every mention is every
  mention: a rename that skipped the project's own `sig/` would leave a signature naming a method
  that no longer exists. The narrowing belongs to the request that sends somebody somewhere.
- **`hover`'s *Defined in N places* is `places().len()`, not a second count.** The footnote and the
  jump are the same number computed once, so they cannot come apart — a card claiming a place no
  jump offers is exactly what a second arithmetic produces, and the audit's fourth check is built
  to see it.
- **One `def` several declarations name is one place, and `locator::all_places` is where that is
  said.** `places` is asked about one declaration at a time and cannot see a repeat across two,
  which cost nothing while a generated declaration had nowhere to point. It costs as soon as one
  does: ActiveRecord's query interface is declared once per relation class and once per base, so
  a name rung answering `find_each` offered mastodon's five times, four of them the same line —
  `Defined in N places` overstating N, from a new direction. First occurrence wins, so the rank
  `definitions_of` and `places` just applied is the order a reader sees. Over the audit's own
  draw: **32,140 repeated URIs inside a place list down to 29,488**, most of them predating the
  change that made it visible.
- **A wide namespace's places are ranked by file name, and alphabetical by path is the bug.** Ruby
  reopens a namespace freely, so a wide one collects a definition per file that ever touched it —
  69 files write `module Sidekiq` in one corpus' bundle and 145 write `module Rails` in another —
  and the path order put an rspec helper above the gem's own `sidekiq.rb`. `definitions_of` sorts a
  second time, **stably**, and the key has **two tiers that ask different questions**. Both the
  jump and the card read the first entry, so one sort fixes both.
- **Tier one: is the file named after the constant?** Squash both the file's stem and the
  constant's unqualified name to letters and digits; the name has to be *in* the stem — so
  `sidekiq_adapter.rb` counts for `Sidekiq` and `ruby-progressbar.rb` counts for `ProgressBar`,
  which is why it is containment and not a prefix — **and at least half of it**. Without that
  proportion a long file name swallows a short constant: discourse writes `Jobs` in 372 files and
  `app/jobs/onceoff/remove_old_auto_close_jobs.rb` took first place over every file in `app/jobs/`.
  Within the tier the smallest excess wins, so an exact match scores zero.
- **Tier two: where nothing is named after it, the file name says nothing at all.** This is the
  common case, not the edge — solidus writes `module Spree` in 539 files and not one is `spree.rb`,
  so every candidate tied at *unnamed* and the tie fell through to how close the stem's **length**
  was to the constant's. `setup` is five letters and so is `spree`; the jump opened
  `api/lib/spree/api/testing_support/setup.rb`. Comparing two unrelated words' lengths is noise.
  What is left that means anything is the path: **a directory the constant names, then the file
  nearest the top of that tree** — `plugins/discourse-ai/plugin.rb` for `DiscourseAi`,
  `app/jobs/base.rb` for `Jobs`.
- **Tier two is a weak rule and is written down as one.** A namespace reopened in hundreds of files
  has no definition site — every entry is a `module` keyword wrapping something else, and ruby-lsp
  answers the same solidus cursor with **463** of the same places in plain path order. Measured
  over six corpora: **592 cursors** change first place, **92** distinct (corpus, constant) lists,
  and about ten of those land somewhere a reader would have chosen while the rest move from one
  arbitrary reopening to another. The list is the right shape; the order is the only thing there
  was to get right.
- **Strip the scheme before reading directories.** `file:` squashes to the four letters `file`, so
  a scheme left on puts every document in a directory named after `File` — lobsters' two places for
  `class File` swapped on it, for the ten minutes it shipped that way.
  `the_uri_scheme_is_not_a_directory_named_file` keeps it found.
- **It is a namespace's rule and must stay one.** A file is conventionally named after the class in
  it; a method's file is named after its class too, so ranking a method's places this way reorders
  answers on nothing. `a_methods_places_are_not_reordered_by_a_file_name` pins that.
- **A document that is not a file sorts behind every one that is, before any name is compared.**
  `rubydex:built-in` and a generated URI are deliberately not `file:` URIs so that the editor can
  never be handed one — and `rubydex:built-in` squashes to a fourteen-letter word that outscored the
  `.rbs` really declaring `BasicObject`, which made `preferred_definition` pick a place no request
  may point at and dropped the class out of a supertype chain. The same fact `locator::places` uses
  to drop it from a place list, reached from the other side.
- **Which definition of a declaration a *list* points at is `locator::preferred_definition`, one
  decision.** A class reopened two hundred times has two hundred definitions, and every list
  mentioning it once must pick the same one, or the class opens in `app/models` from the outline and
  in whichever gem reopened it from the type hierarchy. `locator::declared_in` is the other half —
  whether *any* of them is the user's — which both the symbol picker and the subtype list rank by.
  Neither belongs to the feature that wanted it first.
- **How a construct is spelled for a human lives in `render`, and is shared.** `split_qualified`
  (symbol lists) and `qualified_name` (hover) both go through `singleton_parts`, and
  `symbols::kind_of` is shared with `search`. A symbol reading differently in the outline and the
  picker is a bug, not a style choice.
- **A name rubydex keyed by a number is spelled as the call that built it, on every surface.**
  `Class.new` and `Module.new` are expressions, so what they build has no name until something
  binds it to a constant — and where nothing does, rubydex files it under
  `<document id>:<offset><anonymous>`. Five surfaces printed that key at the user: the hover card
  over the `def`, the card over a call that name-matched it, the outline row, the picker's
  container column and a completion's `detail`. `render::spelled` is the one place it is
  replaced, which is why `qualified_name` and `split_qualified` take a graph — the key says
  nothing about *which* call, and across the five corpora **365 of the 571** anonymous namespaces
  that own a method are modules, so a spelling that assumed would be wrong more often than right.
- **There is nothing better to print, and that is a measurement rather than a preference.** rubydex
  already names `Foo = Class.new do … end` `Foo` — in a method body, in a block, inside
  `class << self`, under a constant-path superclass, with braces instead of `do`. Of the 571 that
  own a method and can therefore reach a card or a row, **not one** is bound to a constant that
  names it. The 31 written `Foo = Class.new { … }.new` are not the exception they look like: there
  the constant is an *instance* of the class, so lending its name to the class would print
  something untrue.
- **Every occurrence in a name, not the first.** A singleton method of one is
  `<key><anonymous>::<<key><anonymous>>#call`, and putting the same spelling on both halves is what
  lets `singleton_parts` recognise the shape and spell the whole of it `Class.new.call`. The key is
  matched as digits, one colon, digits — never walking back over a `::`, which would swallow a
  namespace that is really in front of it.
- **And an anonymous namespace is a bad *receiver*, not only a bad name to print.** A cursor
  inside a `Class.new(base) do … end` body has that namespace as its `self`, which is Ruby's own
  answer — but a `self` captured into a local *outside* the block does not, and that distinction
  cost two chatwoot cursors both surfaces' answers until `Receiver::SelfObject` learned the offset
  it was written at (`types.md`). `locator::missed` declining to name one is what turned the wrong
  answer into the sentence *the receiver's type is unknown*, and that decline was the second half
  of the same defect: a **known and unnameable** type is not an unknown one.
- **So `missed` spells the name instead of refusing it, and it was the only surface not already
  doing so.** Five printed `Class.new` through `render::spelled`; the sixth read the raw key, failed
  `is_nameable` and said *unknown* — including on cards whose own candidate list, two lines above
  the footnote, was rendered `Class.new#…`. It now reads *the receiver is the class object `Class.new`,
  which has no such method*, which is true, and which the completion list at that cursor agrees
  with rather than contradicts.
- **Spelled *before* it is taken apart, and the order is the whole of it.** `class_object_of`
  cannot recognise a raw anonymous singleton — `prefix.ends_with(singleton)` fails against digits —
  and recognises the spelled `Class.new::<Class.new>` exactly. Raw-first would have printed *a
  `Class.new`* for a class object; spelled-first gets both facts. `Namespace::Todo` is still
  refused and still by kind rather than by spelling, because a `Todo` has no members at all and
  *has no such method* would be vacuously true of it.
- **A name nobody can look up sorts below every name that can.** The candidate list under a guessed
  card has ten rows and sorted them alphabetically, so a digit took all ten. The pair
  `(is_anonymous, spelled)` is what ranks *and* what deduplicates, so two anonymous owners of one
  method collapse into a single row and the count above the list keeps counting rows. Over the five
  corpora 593 method names have an anonymous owner and **164** have more than one, so a card asking
  about one of those states a smaller number than it used to.
- **`render::signature_label` writes the label and the spans in one pass, and the offsets are UTF-16
  code units.** `signatureHelp` highlights a parameter by handing the client a pair of offsets into
  the label, so the function that writes the string says where it wrote each piece. LSP's other
  spelling — the parameter as a substring to search for — mis-highlights the moment a label holds the
  same token twice, and `def each(key, value = key)` already does. UTF-16 because that is what a
  client indexes the label by; the protocol ties `Position` to the negotiated encoding and says
  nothing about these, and `def приветствие(имя)` is legal Ruby.

## `require` paths

- **One walk answers both callers, and the offset is the only difference.** `requires::at` asks
  which require the cursor is in and `requires::all` asks for every one in the file; they are the
  same Prism visitor with `Finder::offset` set or not, so what counts as a require — a bare
  `Kernel#require`, never `Foo.require`, never an interpolated path, never one inside a comment or
  a string — is decided once. A second visitor is a second answer that would one day disagree.
- **A require the graph cannot place produces no link at all.** `documentLink` drops it rather
  than returning a link with no `target` for the client to resolve later: nothing is learned by
  the round trip, the target is settled when the link is made, and an underline that opens nothing
  is worse than a path left plain. `require "json"` in a project with no indexed stdlib is that
  case and it is common.
- **The buffer is parsed and the buffer is measured, so `documentLink` needs no rebase.** Both
  halves of every link come out of the same text, and what the graph is asked is a path *string*,
  which no edit can move — unlike every request that hands the graph an offset. In a template the
  parse reads the Ruby view and `TextDocument::range_at` answers in the markup the editor holds,
  which is the blanked document doing what it does for every other request.

## RDoc and markdown

- **Nothing raw-HTML may reach a `MarkupContent`, and nothing inside code may be touched.** RDoc
  lifted Ruby's documentation out of the C source and left the HTML in — `<code>` spans throughout
  the vendored signatures, plus `<em>`, `<strong>`, `<tt>`, `<b>`, `<i>` — and a client renders a hover
  as markdown, which strips them: `<code><=></code>` arrived as a bare `<=>`, and prose merely
  looking like a tag (`<vowel>`, `<rhs>`, `<main>`, a whole `<html>` example) vanished with
  everything up to the next `>`. `render::to_markdown` converts what is markup and escapes what is
  not; RDoc's `rdoc-ref:` links — `core/` alone is full of them, every one pointing into a tree the
  editor has never seen — keep their words and lose the link. It skips fenced blocks, four-space and
  tab-indented blocks, and backtick spans: what is in them is Ruby, and a backslash in front of
  `Hash<Symbol, untyped>` is a visible bug where the old behaviour was a silent one.
- **RDoc is a markup, and `to_markdown` was only reading the HTML RDoc had already turned into.**
  Ruby's vendored signatures carry generated markup; a *gem's own source* is RDoc, and none of it was
  read. Six spellings ship, and the cheapest matters most: **RDoc's verbatim block is two spaces and
  markdown's is four**, which is the whole of "examples are plain text".
- **The same two spaces mean two different things, and what opened above them decides which.** A line
  under a labelled list item (`[+:autosave+]`, or `autosave::`) is the item's description and must
  stay prose; a line under a paragraph is verbatim. Read as verbatim, **every** option description
  in `has_many`'s comment became a code block. `list_item` is therefore recognised *before* the
  indentation is read.
- **`+word+` and `<tt>word</tt>` are the same markup.** RDoc's rule is that the `+` opens at a
  non-word boundary and closes before one, and nothing inside is whitespace — which is what keeps
  `1 + 2` and `a+b` prose.
- **`:nodoc:` is RDoc saying there is nothing here**, and a card with the word in it is worse than no
  card: a reader takes something in a card as an answer. It is `is_directive`'s, not
  `to_markdown`'s, so it only leads — prose above a `:nodoc:` is still documentation.
- Measured on `ActiveRecord::Associations::ClassMethods#has_many` and its neighbours: **most probed
  cards change**, examples become indented code, the option list becomes a markdown list, and every
  `+:autosave+` becomes a code span.

## Where a lookup goes wrong

- **A `def` that names its class instead of spelling it `self` is not attributed back to the method
  it declares.** `def Foo.bar` and `def Foo::bar` record a *constant* receiver;
  `definition_to_declaration_id` has an arm for `self` — enclosing namespace, its singleton class,
  the member — and none for a named one, so such a definition falls through to the arm meant for a
  plain `def` and `bar` is looked for as an **instance** member of whatever namespace the `def`
  happens to sit lexically inside. Usually that misses and the `def` line answers nothing at all:
  **85 of the 126 `def` places that answered nothing over the six corpora** are this one spelling,
  13 to 16 in every corpus, because rexml writes its whole XPath surface as `def XPath::first`,
  `def Functions::floor`, `def QuickPath::match`. Where the namespace *does* declare an instance
  method of that name it does not miss — it lands on a different method and the card says so with no
  hedge. So `locator::declaration_of` answers a named receiver **outright rather than as a
  fallback**: falling back would mean falling back onto the answer that is wrong. What it may do
  after its own lookup finds nothing is retry it through the alias below.
- **The other 41 were two further misses, and both are closed.** 34 were a plain `def` in a class
  written with a compact constant path — `class User::Policy::NotAlreadySilenced` — of which 25 were
  discourse's service objects; *every* `def` in such a class was silent, not only the one the sample
  happened to draw. The cause was not a resolver bug but **Zeitwerk**: a directory with no matching
  `.rb` beside it *is* the declaration of the middle segment, so `workspace/rails/` writes one and
  discourse went 39 to 1.
- **A name can stand for a namespace without being one, and then every member the body declares is
  lost.** Ruby ships `Gem::URI = Bundler::URI`, so `module Gem::URI` reopens Bundler's module
  exactly as Ruby would. `definition_to_declaration_id` attributes a member by the **name of the
  body holding it** — the lexical walk for a plain `def`, the owner definition for a `def self.`,
  the written receiver for `def Foo.bar` — and then asks that name for a namespace to take the
  member off. The alias has no members and no singleton class, so the walk ends in `None` and a real
  `def` line answers nothing. The graph itself is right: `workspace/symbol` reports
  `Bundler::URI#self.split`, a call on **either** spelling resolves to it, and a `module` nested in
  the same body answers `Bundler::URI::Schemes` — a nested namespace and a constant carry a name of
  their own and never ask the question. So `declaration_of` retries the lookup against the name the
  alias stands for, reading the target off a `ConstantAlias` *definition* (the only kind that
  records one), walking a chain because Ruby permits an alias of an alias and capping it at eight
  hops because Ruby also permits `A = B` beside `B = A`. **A fallback and never an override** —
  rubydex first, the named receiver's own lookup first, and only silence is answered, which is what
  keeps `Thing = 1` / `def Thing.bar` answering nothing rather than something near it. It was never
  only `def self.`: a plain `def`, an `alias` and an `attr_reader` in the same body were silent too,
  and five definition kinds are retried for that reason. **The last 2 of the 16 were `def
  Ripper.parse` and `def Ripper.slice`**, which the row had filed under the compact-path mechanism
  and which are this one — Ruby 4 ships `Ripper = Prism::Translation::Ripper`, reached through the
  named receiver rather than through the body. On the 0.2.5 pin this lookup did not return `None`,
  it **panicked**; upstream's `ab88ef1` made it fall out instead, which is the silence this closes.
- **A `def` written inside a block is recorded as a private method of `Object`, with nothing to tell
  it from a true top-level one.** rubydex's nesting stack holds lexical scopes, `Class.new` /
  `Module.new` owners and methods; a `describe "x" do` pushes none of the three. No flag, no variant,
  no nesting id — so declining to treat such a `def` as `Object`'s is **closed at the graph level**,
  and would be wrong anyway, since a top-level `def` really *is* a private method of `Object` and
  really is reachable from every class body.
- **What was left is the order, and it was wrong rather than approximate.** A module `extend`ed onto
  a class object sits above `Class`, `Module` and `Object` in the singleton chain, so the concern
  edge was always meant to be reached first. `resolve_call` now asks it **before** keeping a hit on
  one of `Object`, `Kernel` or `BasicObject`, and keeps the root answer wherever the edge has
  nothing. On a real corpus that is `validate` in a model body going from
  `private Object#validate(config)` — whose definitions are all RSpec spec files — to
  `ActiveModel::Validations::ClassMethods#validate`. **A hit on a root carries almost no
  information**, which is why it is the one hit worth asking a second question about.
- **`extend` is not unread; what it loses is one narrow case.** rubydex indexes `extend` and attaches
  it to the singleton class exactly as Ruby does. Four probes over one workspace isolated it:

  | written | resolves |
  | --- | --- |
  | Ruby: `extend Flat`, `extend Ns::Fmt`, `include Ns::Fmt` | yes |
  | RBS: `extend Flat`, `include Ns::Fmt` | yes |
  | **RBS: `extend Ns::Fmt`** | **no** |

  `stdlib/securerandom/0/securerandom.rbs` writes `extend Random::Formatter` — **most `extend`s in
  the vendored signatures are qualified**, `CGI::Util`, `Minitest::Spec::DSL` and a run of
  `OpenSSL::Marshal::ClassMethods` among them.
- **So `extends_written_on` is a repair, not a second walk, and it cannot double-count.** It is
  reached from `extended_member`, which resolution asks only after the ordinary ancestor search
  came back empty, and from completion's `Extended`, which drops every member the ordinary walk
  already offers. An `extend` rubydex did linearize is found by the search and never gets there. Only
  where the receiver's singleton chain really passes through that declaration's singleton — the
  attached declaration and the classes above it. An `extend` written in an *included module* lands on
  that module's own singleton and never on the includer's.
