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
- **Completion reads Prism's error recovery, not the text.** `Foo::` is a syntax error recovering
  into a `ConstantPathNode` with an empty `name_loc` at the cursor; `foo.` into a `CallNode` with an
  empty `message_loc`. A backwards text scan is used for one thing — where the half-typed word starts,
  which decides what an accepted item *replaces* — and only after Prism has said the cursor is
  somewhere Ruby can be written. Without that, completion fires inside comments and strings.
- **A class or module body completes against the class, not an instance.** `self` in a class body is
  the class object, so `Scope::at` sets `self_id` to the nesting's singleton whenever the cursor is in
  a namespace with no enclosing method. Getting it wrong offers `valid?` where only `validates` is
  callable — measured, dozens of suggestions that would all raise `NoMethodError`. The top level is *not* one
  of these: `self` is `main`, an ordinary `Object`.
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

- **`(group, internal, tier, distance, locality, length, sequence, label)`, and every field earns its
  place.** `internal` sinks names starting with punctuation or `_`, without which a Rails app opens
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
  `DelegateClass, Digest, append_as_bytes`. It sits below `tier` and above everything else. Because
  `take_best` uses `select_nth_unstable_by`, the cap keeps the nearest rows rather than the
  alphabetically first.
- **`Locality` measures nearness by directory, not by namespace, decided by trying both.** A walk
  outwards through the cursor's lexical nesting looks more principled and works on namespaced code,
  but a Rails model is `class Message < ApplicationRecord` at the top level — empty nesting, no
  signal, half a real app unranked. Ruby projects put related code in the same directory whether or
  not they nest it, and Zeitwerk makes the directory *be* the namespace. It is also cheaper: the
  namespace version walked `owner_id` per candidate.
- **`group` is derived from `Locality`, not measured separately.** Built from exactly the documents
  `is_own_code` accepts, so "is this theirs" is "did it score at all" — one walk over a declaration's
  definitions instead of two, on a path running over every method declaration in the graph. Changing
  what `Locality` is built from changes what `group` means.
- **`Distance` is seeded from the same walks rubydex is about to make, each numbered from zero, and
  the lexical walk only fills what the ancestor chains never reached.** Numbering the walks end to end
  would put every constant above every method, or the reverse. `Object` is both the last rung of every
  ancestor chain and the outermost lexical scope: score it as the latter and everything rubydex could
  not attribute lands one step from the cursor. It keys on `Declaration::owner_id`, so nothing parses
  a name — `Foo::<Foo>` and a top-level constant need no special case.
  - **The one seed that is not rubydex's** is the concern edge: a `ClassMethods` module is on none of
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
  `completion.rs` is the single place the two are reconciled.
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
- **`render::is_nameable` is the one test for "rubydex invented this name".** Angle brackets are its
  only punctuation for them — `Foo::<Foo>`, `<uri>:<offset><anonymous>` — and both completion and
  `workspace/symbol` go through it. A very large workspace answered `::` with a page of anonymous
  classes before it existed.

## Rows that come from outside rubydex's walk

- **A class object is offered what a concern extends onto it, and rubydex's walk cannot see any of
  it.** `ActiveSupport::Concern` ends `append_features` with `base.extend const_get(:ClassMethods)`,
  which no file writes, so `query::completion_candidates` correctly offers nothing and `valid` in a
  model body would complete to an empty list. Resolution can ask a second question after the first
  fails because it takes one answer; completion **collects**, so the rows are walked here and joined
  *before* `take_best` — appending after the cap would answer with `limit` plus however many a
  concern holds. The walk is `locator::extended_class_methods`, shared so the gate is stated once.
- **`class_object` is rubydex's answer, not a syntactic test.** A bare call in a class body and an
  explicit `Foo.` both arrive as the singleton class; the same call inside an instance `def` arrives
  as the class. So three of the four receivers hand over what they hold and `extended_class_methods`
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
  which bounds the item: its one under-cap list is followed immediately by a
  `.`, and a trigger character starts a fresh request whatever the flag said. A `null` is what tells
  the client to fall back to its own word list inside a comment.
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

## Signature help

- **`locator::precise_call` is the one gate on every answer that shows a *signature*, and both callers
  go through it.** Keyword-argument completion and `textDocument/signatureHelp` ask the same question
  and must get the same answer: only a receiver rubydex could name. A name-based match while the user
  types into it is not a wrong navigation they can see is wrong, it is a syntactically valid parameter
  list belonging to another class. It keeps the `Foo.new` → `Foo#initialize` redirect;
  `references` is the caller that must not, and reads `Resolution` itself.
- **Keyword arguments are never guessed from a name.** `Context::Argument` only becomes
  `MethodArgument` when `locator::resolve` was *precise*; otherwise it degrades to a plain expression.
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
