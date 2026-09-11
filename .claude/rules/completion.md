---
paths:
  - "src/analysis/completion.rs"
  - "src/analysis/cursor.rs"
  - "src/analysis/signature_help.rs"
---

# Completion and signature help

## The cursor, and what a receiver is

- **A literal's class is read off the parse, never from the text.** `Receiver::Literal` maps a Prism
  node kind to a core class name, so `4.2.` is a `Float`, not an `Integer` with a message.
  `Foo.new.` becomes `Receiver::Instance`, resolving to the class rather than its singleton. A local
  takes the type of the nearest preceding assignment whose *value* ends before the cursor — using the
  name instead would let `x = x.` type `x` from the statement it is part of. That local rule is the
  one place in the module that can be confidently wrong, and it covers two further shapes on the same
  terms: a local assigned a *call*, and an instance variable. See `types.md`.
- **`Receiver` is not `Copy`, and classification happens after the walk, not during it.** A chain
  owns the method names in it, and a receiver that is a local or an instance variable takes its type
  from an assignment the walk may not have reached. `Finder` holds the receiver *node* (`Pending`)
  and classifies once the file is walked. Resolving on the spot cannot answer `b = a.foo` whichever
  order the assignments are written in.
- **A literal receiver must degrade to the name-based list, not to silence.** With `[rbs]` off there
  is no `String` declaration, and `completion::declared` returning `None` is what lets
  `Context::MethodCall` fall through to `by_name`. An id built from a name never indexed is a
  well-formed id that answers nothing.
- **`cursor.rs` never sees a graph and `completion.rs` never sees syntax.** That is the only reason
  the awkward classifications are cheap to test: a trailing `.` on the line above an `end`, a cursor
  in the whitespace after a comma, `#{}` inside a string. Resolving what a receiver *is* belongs on
  the other side of the line, in `types::method_receiver`, shared with navigation so that what
  `person.` is cannot depend on whether the user pressed a key or hovered.

  **Sharing the function is not the same as sharing the answer, and one gap has been found.** Both
  surfaces typed a captured `self` as the anonymous class rubydex records for a `Class.new do … end`
  body; they disagreed about how to *say* so, because completion can list an anonymous class's
  members and `locator::missed` will not print its name. Fixed in `cursor.rs` by giving
  `Receiver::SelfObject` the offset it was written at — see `types.md`. What caught it was the
  audit's check 6, holding a card against the completion list at the same byte, and that check
  exists because a sentence and a list are two answers to one question.
- **Completion reads Prism's error recovery, not the text.** `Foo::` is a syntax error recovering
  into a `ConstantPathNode` with an empty `name_loc` at the cursor; `foo.` into a `CallNode` with an
  empty `message_loc`. A backwards text scan is used for one thing — where the half-typed word starts,
  which decides what an accepted item *replaces* — and only after Prism has said the cursor is
  somewhere Ruby can be written. Without that, completion fires inside comments and strings.
- **A class or module body completes against the class, not an instance.** `self` in a class body is
  the class object, so `Scope::at` sets `self_id` to the nesting's singleton whenever the innermost
  body holding the cursor is a namespace. Getting it wrong offers `valid?` where only `validates` is
  callable — measured, dozens of suggestions that would all raise `NoMethodError`. The top level is *not* one
  of these: `self` is `main`, an ordinary `Object`. **A block written straight into that body is the
  one exception and it adds rather than replaces** — see the closure rows below.
- **Innermost is the whole of that rule, and the namespace can be the inner one.** Ruby refuses a
  `class` keyword in a method body, so `Class.new(base) do … end` is how one gets written there —
  and rubydex records the block as a class all the same. It is `class_eval`'d, so `self` inside it
  is the new class *object*, and the `def` around it says nothing about it. `types::self_of` read
  the innermost method unconditionally until 2026-09-15 and answered the base's **instance** side:
  at a `define_method` inside one the list was a single row, `define_singleton_method`, arriving
  from `Object`, while the card beside it said `Module#define_method` and was right. Bodies nest and
  never overlap, so the body that starts later is the inner one and it is the one that decides —
  which also keeps a `def` written *inside* the block on the instance side.
  `a_class_new_block_inside_a_def_completes_against_the_class_it_opens` pins both directions.
- **The cursor carries whether it sits in such a block, and that is a field rather than a question.**
  `cursor::at` has already parsed the buffer, so `Cursor::in_a_closure` is a second walk over the
  same tree. Asked where it is needed it was a second parse of the whole file for `locator` and would
  have been a third for this module, on every keystroke in a class body.
- **`self_decl_id` must be stated for `MethodCall` and `NamespaceAccess`.** rubydex derives it only
  for `Expression`; the receiver contexts use it purely as the caller in their visibility check, and
  `None` there means "outsider" — a class would stop seeing its own private class methods.
  `Scope::caller` is the one place that decides.
- **`self` for an expression is ya-lsp's to state.** rubydex used to derive it from the lexical
  nesting when `CompletionReceiver::Expression` got `self_decl_id: None`, and upstream's *"Make self
  type required for completion"* deleted that branch while leaving the doc comment that promises it. A
  `None` there now collects **no methods and no instance variables at all** — the commonest completion
  there is. `receiver_for` therefore passes `Scope::caller`, which is `self_id.or(nesting_id)` plus
  the alias unwrapping its own doc describes. Six tests in `analysis/mod.rs` caught it; the completion
  sweep says it is restored at scale. **A dependency can delete a default without deleting the
  sentence that documents it** — a port has to be swept, not only compiled.

## Ranking

- **`(tier, internal, group, distance, length, generated, locality, sequence, label)`, and every
  field earns its place.** `internal` sinks names starting with punctuation or `_`, without which a Rails app opens
  `User.` on `__send` and `_fork`. `sequence` is *only* a keyword argument's position in its
  signature: rubydex's emission order within a namespace is a hash map's and reshuffles when a member
  is added.
- **A sigil is not a letter, and `tier` refuses before it scores.** `@foo`, `@@foo` and `$foo` are
  three namespaces, so the leading run of `@`/`$` a label carries must start with the one the prefix
  asked for. `starts_with` rather than equality, and deliberately asymmetric: `@` admits `@@count`
  because the second `@` may be the next keystroke, and `@@` admits no `@name`. Without it the
  subsequence match reads the sigil as one more character and `@` offers `$@` — shipped through two
  releases, being a row at the bottom rather than a wrong answer at the top. `significant` (which
  decides `internal`) is defined in terms of the same `sigils`. The no-sigil case is **left alone on
  purpose and pinned**: `entr` still reaches all five namespaces, because somebody who typed no sigil
  has not said which they meant.
- **`distance` is the only term for relevance; without it a list nothing has been typed into is not
  ranked at all.** `tier` is 1 for every row at an empty prefix and `length` is 0, so what was left
  was the label, alphabetically — which is why `"hello".` shipped opening on
  `DelegateClass, Digest, append_as_bytes`. It sits below the three terms that say what the row *is*
  — matched, callable, whose — and above every term that only breaks a tie. Because
  `take_best` uses `select_nth_unstable_by`, the cap keeps the nearest rows rather than the
  alphabetically first.
- **`length` sits above `generated` and `locality`, and is silent exactly where they work.**
  `sort_length` is 0 when the prefix is empty, so at a bare cursor the term cannot speak at all and
  the two below it decide that list unchanged; from the first keystroke it is the strongest tiebreak
  left among rows that have all matched, and `render` beats `rendered_format`. Below them it was a
  long generated name from the nearer file sitting above the short one the user was spelling.
  Measured over the audit's 1,452 typed cursors, lifting it two places takes rank 1 from 543 to
  **586** at one character, 727 to **759** at two and 887 to **919** at three, and the top ten from
  1,096 to **1,122**, 1,164 to **1,169** and 1,154 to **1,157**; 108 and 392 at a bare cursor are
  untouched, and no test moves. **Being inert at k=0 is the whole of why it may rank this high** —
  the two neighbouring keys both pay for the same gain, dropping `generated` as well costs a test
  and dropping `locality` with it costs six.
- **The name-based list is ranked by `tier` and by nothing below it, measured.** Over 385 untyped
  cursors on six corpora, nineteen orderings of the eight terms behind `tier` — including every one
  that deletes a term outright — return the same lists: median rank 1 from the third character on,
  and 474 of the 492 lists that came back hold the word in the first 128. The twentieth, `Locality`
  lifted *above* `tier`, takes the median to 10, which is this arm's only ranking risk. The reason is
  the decline: a guess still broad enough for the lower terms to matter is over `admitted` and is not
  offered at all, so every list this arm returns has already been narrowed by what the user typed —
  nothing at a bare `.`, 30% of cursors at the fourth character. `make audit-prefix` is that probe,
  and `make audit-rank` does not answer for this path however its shapes are named. **A split
  comparator for the two paths was built and measured and buys nothing**: `take_best` taking the
  comparator as a parameter works, and there is no reason to carry it.
- **`Locality` measures nearness by directory, not by namespace, decided by trying both.** A walk
  outwards through the cursor's lexical nesting looks more principled and works on namespaced code,
  but a Rails model is `class Message < ApplicationRecord` at the top level — empty nesting, no
  signal, half a real app unranked. Ruby projects put related code in the same directory whether or
  not they nest it, and Zeitwerk makes the directory *be* the namespace. It is also cheaper: the
  namespace version walked `owner_id` per candidate.
- **`group` is derived from `Locality`, not measured separately.** Built from exactly the documents
  `is_own_code` accepts, so "is this theirs" is "did it score at all" — one walk over a declaration's
  definitions instead of two, on a path running over every method declaration in the graph. Changing
  what `Locality` is built from changes what `group` means. `Locality::of` now answers three things
  from that one walk — nearness, `generated`, and whether the application loads it — and returns a
  named `Written` rather than a tuple, because three unlabelled fields is not a return type.
- **So a generated document is scored beside the file that implied it, and that is a correction
  rather than a convenience.** `group` sits above `distance`, so anything `Locality` does
  not score cannot be rescued by `distance` however near its owner is — and a generated document has
  no path to score. On a Rails application that is every column, association, enum, scope and
  delegate the receiver's own table implies, and the visible cost is the far end of the chain
  winning: measured on a real application, two methods the project patched onto `Object` took ranks
  **1 and 2** of `story.`, above `body`, `score` and `title`. `Locality::at` files every
  generated document at its source's step — picked out of the pass over the graph it already makes
  and mapped back with `synthesized::source_of`, because one source writes one document per body and
  the set is no longer a function of the source's URI alone. `own_documents` itself is untouched,
  which is what keeps a generated definition out of `rename`'s reach.
- **A declaration whose every definition is under a test tree is not offered, and that is a drop
  rather than a rank.** `environment::in_a_test_tree` is the tag — four directory names matched as
  path segments, a deny-list because six Ruby repositories agree on where tests live and not on
  where source lives — and `Locality` reads it in the same walk it makes for nearness. The tag, the
  rule and the cursor gate all live in `analysis/environment.rs`, which `environment.md` covers;
  what is specific to this module is that the verdict here is a **drop**. Ruby will not find
  the name from an application file at all, so offering it is offering a suggestion that cannot
  run: the judgement `reachable` already makes about a private method. Sinking it instead leaves it
  holding a slot under `MAX_COMPLETION_ITEMS` and leaves the name in `by_name`'s candidate count,
  which decides whether a guess is offered at all.
- **The same drop reaches a generator's template tree, and that tag is read of the whole graph.**
  `environment::in_a_generator_template` — a `templates` segment after a `generators` one — is
  what a gem ships in order to *copy* it into somebody else's project, so nobody loads it and the
  row cannot run. It is read of `graph.documents()` rather than of `own`, which is the opposite of
  the test tag beside it: a gem's `lib/rack/test/` is a published library, but a gem's generator
  template is exactly the common case. One extra pass over the documents, which is the pass `own`
  itself is built with, and a `NO_LOCALITY` entry that changes no ranking because `of` falls back
  to that value anyway. Measured at a typed prefix in the project's own code over six corpora:
  template rows **22 -> 7**, every one of the fifteen removed `Object`-owned and so offered for
  *every* receiver — discourse's `id` out of active_model_serializers' template, chatwoot's
  `update` out of jbuilder's, whose card still carried an unexpanded `<%= route_url %>`.
- **The mechanism is `Object`, which is why this is worth a term at all.** rubydex files a `def`
  with no enclosing `class` or `module` — the top of a spec file, or anything inside an
  `RSpec.describe … do`, because a block body is not a namespace — on `Object`, the end of every
  ancestor chain. Measured over the six corpora with Prism, `Object`'s instance surface is **78% to
  99.8% spec code**: 18 spec-declared methods against 5 of the application's on lobsters, 477
  against 1 on mastodon, 1,359 against 118 on discourse. Before the fence, test-only rows were
  **0.6% of what lobsters offers at a bare word and 13.6% of what mastodon does**, 96% of mastodon's
  lists carried one and 205 of them sat in the top ten. Test code is *not* generally a large share
  of a declaration graph — RSpec's `describe`, `it`, `let`, `double` and `factory` declare nothing —
  so the harm is concentration, not volume.
- **The fence is off wherever the cursor is itself in a test tree, and a declaration survives if
  *any* definition is loadable.** Both halves are `environment`'s — `fenced_from` and
  `Tally::loadable` — and the name rung reads the same two functions, which is what keeps the list
  and the jump from fencing the same name differently. A developer editing a spec is exactly who
  the helper in the next spec file over is the answer for, and a class the suite reopens is still
  the application's class. `Locality::fenced` caches the first because the question is about the
  cursor and the point of use runs once per method declaration in the graph.
- **A document `Locality` never scored is loadable, not suspicious.** The table is built from the
  workspace's own documents, so a gem, Ruby's own signatures and anything else outside it miss the
  lookup — and a miss must read as *not the project's test tree* rather than as *unknown*. A gem
  with a `test/` directory inside its `lib/` would otherwise be fenced by a rule that has no
  business reading its paths.
- **`generated` is a term of its own and not a side effect of `locality`.** *What the class was
  written to do, then what its table says it holds* is a property of the **kind** of declaration: the
  column is in `db/schema.rb` and the method beside it in `app/models/story.rb`, so leaving the two
  to `locality` decides them by which directory the cursor is nearer — the pinned order held from
  `app/` and inverted from `db/`, which a test now fails without. It sits below `distance` (a near
  owner still wins) and above `locality`, and `Locality::of` returns it out of the walk it was
  already making rather than a second one. **It is here for the invariant and it costs something**,
  and it costs the same thing under both keys it has shipped under. Against the `Locality` half alone
  it moved **18 words out of rank 1** into the top ten, 124 to 106, while tightening everything
  below. Against the key above — deleting the term outright, every other term held — it is 18 again,
  108 to **126** at a bare cursor, for 3 lost from the top ten there and 1 rank-1 each at two and
  three characters. So the corpora say a developer reaches a receiver's *generated* members slightly
  more often than its hand-written ones, and a run scored on rank 1 alone reads the term's removal as
  an improvement. The order is kept anyway, because without it the same receiver ranks differently
  depending on which directory the cursor is in, and an order nobody can learn is worse than an order
  somebody disagrees with. **Deleting `.then(a.generated.cmp(&b.generated))` is the whole of the
  other choice**, 18 rank-1 answers is what it buys, and
  `what_the_class_was_written_to_do_leads_what_its_table_says_it_holds_from_anywhere` is the one
  thing that fails when it goes.
- **`Distance` is seeded from the same walks rubydex is about to make, each numbered from zero, and
  the lexical walk only fills what the ancestor chains never reached.** Numbering the walks end to end
  would put every constant above every method, or the reverse. `Object` is both the last rung of every
  ancestor chain and the outermost lexical scope: score it as the latter and everything rubydex could
  not attribute lands one step from the cursor. It keys on `Declaration::owner_id`, so nothing parses
  a name — `Foo::<Foo>` and a top-level constant need no special case.
  - **The one seed that is not rubydex's** is the extend repair: such a module is on none of
    the chains rubydex walks, so every member would arrive at `NO_DISTANCE`, last and equally last,
    *behind `Object`'s own methods* — backwards for the name a model body is most likely typing.
    `Distance::extended` records the module, not a chain, at `locator::Extends::step`, which counts
    the **classes** the chain passes through rather than the ancestors: a Rails model's instance
    ancestors are forty rungs of concerns and its singleton chain is five classes, so counting
    ancestors would score `validates` below `Object`'s methods.
- **`analysis::mod`'s `ANCESTRY` fixture pins order and reachability together.** Every other
  completion test asks whether a name is offered; the ranking bug shipped through all of them and
  through a benchmark that only measured milliseconds. One method per rung, and `first_rows` prints
  the owner beside the label so the assertion reads as a ranking. `Item#initialize` and its
  `private def stash` are in it so the pinned receiver lists prove their own absence. Add to it rather
  than starting a second one.

## Visibility and reachability

- **`Context::allows_private` is the one test for "may a private method be written here", and it is
  pure syntax.** Ruby permits one with an implicit receiver and, since 2.7, with a receiver spelled
  `self` — through `.` and `::` alike, both checked against a real interpreter. rubydex asks a
  different question: its filter passes a private method whenever the caller's `self` is the same
  *class* as the receiver, so `Vault.new.secret` was offered from inside `Vault`, where a real
  interpreter raises `NoMethodError`. ya-lsp is deliberately stricter, and `reachable` in
  `completion.rs` is where the two are reconciled **for the list** — since 2026-09-16 it is no
  longer the only surface that asks. `locator::Privacy` carries the same answer to navigation and
  to the signature card, which for two releases resolved through a rung that never asked: the
  head-to-head counted **90** *Resolved* cards naming a member this list had just refused, 88 of
  them `RSpec.describe` answered with minitest's `private Kernel#describe`. `navigation.md` has the
  rung table, and which surfaces apply the rule against which deliberately do not.
- **`ALWAYS_PRIVATE` is Ruby's list of five, not a heuristic.** `rb_add_method` privatises
  `initialize`, `initialize_clone`, `initialize_copy`, `initialize_dup` and `respond_to_missing?` at
  the point of definition, and neither rbs nor rubydex records it — rbs is not even self-consistent,
  marking `Kernel#initialize_copy` private and `String#initialize_copy` public. **Instance methods
  only**: the same code leaves `def self.initialize` public and `Bar.initialize` really does call it,
  which is what `singleton_owned` checks. `method_missing` and `singleton_method_added` are *not* on
  the list — they are private on `BasicObject` because that is how those copies were written, which
  the graph already knows.
- **`reachable` runs after `tier`, not before.** It costs a visibility lookup where the prefix test
  costs a string compare, and `by_name` runs the pair over every method declaration in the graph.
  `private_ok` short-circuits it entirely wherever no receiver was written.
- **`reachable` does not take rubydex's visibility record at face value, and the exception is
  `locator::Modifiers`.** A bare `private` written inside a block is recorded against every `def`
  *below the block*, so an ordinary Rails concern's public methods — `class_methods do … private …
  end`, then a `def` in the module body — were never offered on any receiver. The jump reads the
  same record, so leaving one surface unrepaired would mean a jump landing on a member the list at
  the identical cursor refuses, which is exactly what the audit's `absent-resolved` key counts.
  `navigation.md` has the rule and why it is the reading that does not refuse.
- **The repair is the receiver path's and deliberately not `by_name`'s.** It costs a read of the
  declaring document, memoised per document per request: an ancestry's members are tens of
  documents, and `by_name` walks every method in the graph before it decides whether to answer at
  all. The two paths are alternatives at one cursor — the name-based one runs only where no
  receiver resolved — and what it produces is the *Guessed* tier, where a list one name short is
  what the tier already warns about.
- **`render::is_nameable` is the one test for "rubydex invented this name".** Angle brackets are its
  only punctuation for them — `Foo::<Foo>`, `<uri>:<offset><anonymous>` — and both completion and
  `workspace/symbol` go through it. A very large workspace answered `::` with a page of anonymous
  classes before it existed.

## Rows that come from outside rubydex's walk

- **A concern's class methods arrive as ordinary rows now, and the walk that used to collect them
  is gone.** `ActiveSupport::Concern` ends `append_features` with
  `base.extend const_get(:ClassMethods)`, which no file writes, so `query::completion_candidates`
  offered nothing and `valid` in a model body completed to an empty list. Since 2026-09-15
  `workspace/rails/concerns.rs` declares each of those `def`s onto the singleton of every class that
  includes the concern, so rubydex's own walk carries them and this module reads them like any other
  member of the receiver — label, container, ranking and dedup all unchanged, because there is
  nothing special left to do. `synthesized.md` has the generator's rules and the measurement that
  forced its shape.
- **What is still collected here is a repair with no Rails in it.** `Extended` reads
  `locator::extended_modules`: the modules a class object's own bodies `extend` that rubydex did not
  linearize — `extends_written_on`'s four-row table, `SecureRandom.hex` its commonest cost. The rows
  are joined *before* `take_best`, because appending after the cap would answer with `limit` plus
  however many the modules hold. Resolution can ask a second question after the first fails because
  it takes one answer; completion **collects**, which is why the two halves share the walk rather
  than each writing one.
- **`class_object` is rubydex's answer, not a syntactic test.** A bare call in a class body and an
  explicit `Foo.` both arrive as the singleton class; the same call inside an instance `def` arrives
  as the class. So three of the four receivers hand over what they hold and `extended_modules`
  declines anything that is not a singleton; `NamespaceAccess` is the one arm that has to look,
  because `Foo::` names the class while rubydex collects its *singleton's* methods.
- **A name the ordinary walk already offers is not offered twice, and the prefix is tested before
  that question.** The dedup is the resolution's own rule applied per member: a class writing its own
  `def self.validates` keeps it. Asking costs a walk of the singleton chain — a hundred hash lookups
  on a Rails model — so `tier` runs first, and a keystroke pays for the handful of rows it could
  offer rather than for every class-macro name Rails installs. Measured at a real model's
  `validates`, the extra walk is a fraction of a millisecond with a few characters typed, and no
  worse with none (where the list is capped anyway).
- **These rows pass ya-lsp's `reachable`, not rubydex's visibility filter, and the difference is
  `protected`.** Every row off rubydex's own walk is already filtered against the caller; these are
  not, so a `protected` class method is offered where an explicit receiver is written. Measured over
  every `module ClassMethods` block in Rails' core gems and the six applications: the methods they
  declare are overwhelmingly public, a minority private, and **`protected` is vanishingly rare**.
  Left alone rather than filtered,
  because `private_ok` is false for `Foo.bar` written *inside* `class Foo`, where Ruby permits a
  protected call — filtering would trade a rare wrong offer for a rare wrong refusal, and the
  name-based list does not filter either.
- **A bare word in a block written straight into a class body is offered the instance side too, and
  the card beside it said so first.** `locator`'s closure rung answers one such name from a scope
  this module never reached — `self` in `rule(:colon) { … }` is the class object until whoever takes
  the block re-binds it, which is what every DSL that takes one does — so `hover` named a member the
  list at the same byte did not carry. `InClosure` is the collecting half: rubydex is asked the
  *instance* question about the attached class, which is an ancestor walk, so a superclass's `def` is
  reached — the half `reachable_on_a_class_object` was throwing away. Rows are filtered to **methods**
  and nothing else: the constants it also collects are the class object's from the same nesting and
  are already on the list, and `@x` in a class body is the class object's whatever the block does
  with `self`, because re-binding a receiver does not re-bind a lexical scope. Visibility is
  `ranked_declaration`'s as for every row, and `private_ok` is already true — no receiver was
  written, so a private method is exactly what may be called.
- **`add_closure` fills gaps and never shadows, which is the opposite of `add_view` and is what
  makes the two surfaces agree.** `locator` reaches its closure rung only once `resolve_call` has
  come back imprecise, so the class object's own answer — the concern edge included — always wins
  there. The list holds that order by adding a row only where it does not already carry the name,
  which is why `add_closure` runs **after** the concern edge and not before it. A helper module
  really does replace `Kernel#format` for a template and `add_view` takes the row away; a block's
  `self` is an inference about somebody's DSL and the class object is what Ruby uses if the
  inference is wrong, so here the existing row stands.
- **Measured, two binaries from one tree, six corpora: 90 cursors, 0 of them answered before and
  90 after.** The probe scans for a macro call opening a block at two-space indentation in a file
  whose first construct is a `class` or `module`, takes the first bare word inside it, and keeps
  only the cursors whose card carries the closure footnote — the server's own statement that the
  rung fired, which by construction means the class object holds no such name. 1,674 candidates
  reach 90 such cursors; **the old binary put the member on the list at 0 of them and the new one
  at 73**, with the other **17 lost to the 512 ceiling and not to the change**: every one of those
  lists came back at exactly 512 rows with `isIncomplete`, and every one holds the member once the
  word is typed. They are Rails' controller and view helpers, which sit far up an instance chain,
  so this is the ranking behaving as `Distance` already says it does rather than a new gap — one
  keystroke is what buys them, the same sentence the 2026-09-13 ranking measurement ends on. The
  shape is **rare**: 5 of the six corpora hold fewer than 15 of these cursors each.
- **The audit cannot see any of this, and the sweep saying so is not evidence that nothing
  happened.** Its completion key poses only `member` cursors, so a bare word has no list for a
  check to hold a card against; the sweep for this change read *0 findings new, 0 gone, 0 counters
  moved* across all six corpora while the probe above counted 90 cursors changing. `audit.md`
  states the gap where the key's limits are collected.
- **Three refusals, each a different reason.** A `def` fixes `self` and a block inside one cannot
  unfix it, which `Cursor::in_a_closure` already answers. A module has no instances for the claim to
  be about, and `included do` / `class_methods do` re-bind `self` to the **including** class, which
  is reachable from neither side of the file — the same decline `locator` makes and for the same
  sentence. And a receiver somebody wrote down says what `self` is not, whatever block it sits in:
  `MethodCall` and `NamespaceAccess` decline outright, and the `Expression` arm declines on a missing
  `self_decl_id` — which is `::Foo`, the one receiver that arrives carrying `Only::Constants`, so
  that `?` is also what keeps a method off a list that may hold none.
- **A template's bare word completes from the view context, and the rows join before the cap.** Same
  shape as the concern edge, reached by a second road: `in_view` runs beside `Extended` and `add_view`
  extends `ranked` before `take_best`. Both read `views::Reachable`, so the gate is stated once
  (`views.md`). Two things it does that `Extended` does not: its rows carry a **step** of their own,
  because `_helpers` is not one of rubydex's chains and every row would otherwise land behind
  `Object`'s methods; and a name the graph already offers is **taken away** rather than skipped,
  because a helper module is `include`d into the view class and really has replaced `Kernel#format`
  for every template. Swept over five applications: the word the file itself wrote goes from being
  offered almost nowhere to almost everywhere, **with nothing lost.**

## The response

- **A list is `isIncomplete` when the cap dropped rows; nothing found is `null` rather than `[]`.**
  Dropping a row is the only way the answer can fail to hold something a longer prefix would reach,
  because every filter on the way is a *subsequence* match — `tier`, and rubydex's `MatchMode::Fuzzy`
  under `by_name`. A longer prefix admits a subset of what a shorter one admitted, so an untruncated
  list is a **superset** of what a fresh query would return and the client may narrow it itself. The
  *ranking* is the half the client does not keep — it scores against the longer prefix and uses
  `sort_text` as the tiebreak, which is the right way round, since that score knows what has been
  typed since the list was built. The flag costs a whole request per keystroke: measured on a large
  corpus at typing speed, a word typed against an over-cap list costs **noticeably fewer round trips
  and proportionally less waiting** once the flag is honest. **`self.` gains nothing either way**,
  which bounds the change: its one under-cap list is followed immediately by a `.`, and a trigger
  character starts a fresh request whatever the flag said. A `null` is what tells the client to
  fall back to its own word list inside a comment.
- **An empty list is `isIncomplete` too, and that is not the previous rule read twice.** Every route
  answering with no rows does so because there was nothing to say — a receiver that turned out not to
  be a namespace, a `::` on something that cannot hold one — and "the complete answer is nothing"
  would have the client stop asking as the word grows. So the flag is set by `take_best` having
  truncated, *and* by every path returning before it.
- **Which tier a list came from travels on `data`**, because by the time `completionItem/resolve`
  arrives that is all either side knows. It is a property of the *list*, not a row: every row was
  offered for the same receiver. `resolve_completion` built its card with `precise: true`
  unconditionally for two releases, so every row off the name-based list was presented as certain. The
  shape is an object rather than the bare id string the type hierarchy sends, and both halves are
  strings because a `DeclarationId` is a 64-bit hash and JSON numbers are doubles — a round trip
  through a client silently corrupts a number.
- **`MAX_COMPLETION_ITEMS` is not a latency control**, unlike `MAX_WORKSPACE_SYMBOLS`. Measured
  across a range of caps it barely moves the worst request, because the cost is the graph work in
  front of it. It bounds the response size. Re-measure before assuming otherwise.
- **The name-based list has two ceilings of its own, and the split is the measurement.**
  `MAX_COMPLETION_ITEMS` bounds a *response*; `MAX_UNTYPED_CANDIDATES` decides whether a **guess**
  is worth making, and `MAX_UNTYPED_COMPLETION_ITEMS` how many of its rows are worth reading. At an
  empty prefix the candidate set is the project's entire name universe — 26,073 to 45,953 over five
  corpora — with the word the file actually wrote at **rank 4,070** at the median and `hover`
  naming no type at 112 of 112 of those cursors. That is far above any admission ceiling, so it
  declines, which is what this exists for. **Raising `MAX_COMPLETION_ITEMS` is not the fix and was
  measured**: at a cap of 100,000 the member is present 436 times of 500 instead of 304, still at
  rank 4,070, for ~200 KB of JSON per keystroke.
- **Where those two ceilings sit is measured by `make audit-prefix` and by nothing else.** Every
  cursor the audit draws is at a word's start, where the candidate set is the whole universe and
  any bound in the plausible range declines identically — so the values only bite mid-word and only
  that probe asks there. It classifies each cursor at an empty prefix (a typed receiver answers,
  an untyped one is declined) and follows the untyped ones outwards, against a binary built with
  the ceiling raised. Over 335 of them:

  | candidates | lists | word in the first 128 rows |
  |---|---|---|
  | 1–128 | 159 | 159 |
  | 129–256 | 131 | 131 |
  | 257–512 | 140 | 140 |
  | 513–1,024 | 157 | 152 |
  | 1,025+ | 156 | 141 |

  So **the ranking is trustworthy and the row count is not**, which is why they are two numbers:
  admit at 512, the largest value with no measured loss, and send 128, which is what a person
  reads. Nothing is offered for the first three keystrokes at any of these values. Truncating is
  honest here only because a prefix exists — with nothing typed `tier` is 1 for every row and
  `length` is 0, leaving `Locality` as the only live term, so the rows kept would be chosen by
  which directory they live in. Whether an admission ceiling *above* 512 is better stays censored:
  a declined list reports "over" and never by how much.
- **`MAX_COMPLETION_ITEMS` was measured for the typed list too, and it does not move.**
  `make audit-rank` is that probe — `audit-prefix`'s question asked of the other path, at the
  cursor an editor sends the instant `.` is typed. Over 1,452 typed member cursors on six corpora,
  against a second binary built with the ceiling raised to 100,000:

  | | at 512 | raised |
  |---|---|---|
  | lists that came back whole | 1,103 of 1,452 | 1,452 |
  | the word is on the list, nothing typed | 1,218 | 1,236 |
  | the word is on the list, one character typed | 1,236 | 1,236 |
  | its rank at one character | median 3, p90 32 | median 3, p90 32 |
  | list size | median 288, p90 512 | median 288, p90 653, p99 1,539, max 7,191 |

  **Below 512 it loses answers and buys nothing.** A ceiling of 128 drops **217 of the 1,218** words
  that are on the list today with nothing typed, and turns 1,333 of 1,452 lists into truncated ones —
  a re-ask on every keystroke where 1,103 are filtered by the client today. **Above 512 it buys 18
  words at the trigger that one keystroke already returns**, and trades the server's ranking for the
  client's on the 310 lists between 512 and 1,024. Neither direction is an improvement, and the
  number that would change the answer is the one in the fourth row: the day a typed list holds its
  word past rank 128 *after* a character is typed — 14 of 1,236 today — the display half of this is
  worth splitting off the response half, exactly as it was for the name-based list.
- **The rank a typed list gives a word with nothing typed is the size of one owner's band, not a
  ranking fault.** Median 32 over those cursors, and **51% of the rows above an answer are its own
  owner's** — the bands sort correctly and the alphabet decides inside them, because `tier` is 1 for
  every row and `length` is 0 until something is typed. Measured at forem's worst position, 488 of
  the 505 rows above the answer belonged to the same class. So the lever here is which *band* a row
  lands in — see `group` under Ranking — and not a sixth term in the key.
- **The decline is a decision, and `isIncomplete` is what keeps it from being a refusal.** An empty
  list marked complete would have the client filter it locally and never ask again, so the list
  could not come back inside the word it was declined for — and coming back as the prefix narrows
  is the whole reason a bound is admissible instead of a flat refusal. This is the previous rule
  about empty lists doing the work it was written for.
- **`[types] guess_from_names = false` reaches this list too.** `by_name` is the name guess wearing
  a different hat — matched on the word alone, belonging to no class — so the setting that silences
  the tier on a *card* has to silence it in a *list*, or it only half exists. It gates that arm and
  nothing else: a receiver the graph can name is not a guess, and a receiver guessed from its own
  spelling still offers a real class's members, which is a different and better answer.

## Signature help

- **`locator::precise_call` is the one gate on every answer that shows a *signature*, and both callers
  go through it.** Keyword-argument completion and `textDocument/signatureHelp` ask the same question
  and must get the same answer: only a receiver rubydex could name. A name-based match while the user
  types into it is not a wrong navigation they can see is wrong, it is a syntactically valid parameter
  list belonging to another class. It keeps the `Foo.new` → `Foo#initialize` redirect;
  `references` is the caller that must not, and reads `Resolution` itself.
- **Keyword arguments are never guessed from a name.** `Context::Argument` only becomes
  `MethodArgument` when `locator::resolve` was *precise*; otherwise it degrades to a plain expression.
- **`Call` carries `allows_private`, read off the same `CallNode` the parse already produced.**
  `precise_call` takes the privacy gate as a parameter rather than deciding it, because its three
  callers know three different amounts: the signature card has that node's receiver; keyword-argument
  completion is at an implicit receiver by construction and passes `Allowed`; the outgoing call
  hierarchy has no cursor at all and passes `Allowed` too. Deciding inside `precise_call` would mean
  guessing on behalf of the two that cannot answer. **The card and the jump answer one cursor and
  may not disagree about it** — the same sentence that puts the environment fence on this rung — so
  a signature is not drawn for a call `definition` has already refused to follow.
- **`cursor::at` and `cursor::call_at` answer different questions, deliberately.** Three places have
  nothing to complete and a call still being written: a `.` inside the parentheses (`puts(person.`), a
  string argument (`puts("hel`), and a comment between two arguments. `at` gives up on all three —
  that is what stops completion firing inside a string — and `call_at` does not, because an editor
  keeps the signature popup up through every one and a `null` makes it flicker per keystroke.
- **The active argument is `Active`, a three-way answer, never a bare number.** `Nth` is the count of
  arguments ending before the cursor, with a keyword hash spread into its own elements first
  (`f(1, a: 2, ` is the third parameter, not the second). `Keyword` is by *name*, because Ruby writes
  them in any order — both spellings, since `f(a: 1)` and `f(:a => 1)` satisfy the same `def f(a:)`.
  `AnyKeyword` is the gap after a finished keyword: which one comes next is unknowable, that it is a
  keyword is not, because Ruby forbids a positional argument after one — and counting there answers
  with a parameter the call can no longer reach.
- **An argument's claim on the cursor runs past its own span, to the comma.** `create(name: ` has
  written the keyword and not its value, and Prism recovers the pair as ending at the colon, so the
  cursor is outside every node. The comma says the user has moved on.
- **The highlighted parameter is ceilinged at `*rest`, not at the end of the list.** A splat absorbs
  every positional argument after it, so the fifth argument to `def new(name, age = 18, *nicknames)`
  is still `*nicknames`; counting straight through walks one parameter further per argument typed.
  With no splat the ceiling is the last parameter, because **LSP 3.17 cannot say that no parameter is
  active** — an index outside the list and an omitted one both mean zero, so running off the end would
  silently point at the first.
- **Overloads stay overloads.** `Signatures::Overloaded` is real (RBS declares three arms for
  `String#gsub`) and LSP has `activeSignature` for exactly this. The arm chosen is the first one that
  *has* the parameter being written; flattening to the first would choose to know less than the
  signatures do.
