---
paths:
  - "src/analysis/completion.rs"
  - "src/analysis/cursor.rs"
---

# Completion

- **A literal's class is read off the parse, never from the text.** `Receiver::Literal` maps a
  Prism node kind to a core class name, so `4.2.` is a `Float` rather than an `Integer` with a
  message. `Foo.new.` becomes `Receiver::Instance`, which resolves to the class rather than its
  singleton. A local takes the type of the nearest preceding assignment whose *value* ends before
  the cursor — using the name instead would let `x = x.` type `x` from the statement it is part
  of. That local rule is the one place in the module that can be confidently wrong.
- **A literal receiver must degrade to the name-based list, not to silence.** With `[rbs]` off
  there is no `String` declaration, and `completion::declared` returning `None` is what lets
  `Context::MethodCall` fall through to `by_name`. An id built from a name that was never indexed
  is a well-formed id that answers nothing.
- **`cursor.rs` never sees a graph and `completion.rs` never sees syntax.** That is the only
  reason the awkward classifications are cheap to test: a trailing `.` on the line above an
  `end`, a cursor in the whitespace after a comma, `#{}` inside a string. Resolving what a
  receiver *is* belongs on the other side of the line.
- **Completion reads Prism's error recovery, not the text.** `Foo::` is a syntax error that
  recovers into a `ConstantPathNode` with an empty `name_loc` at the cursor; `foo.` into a
  `CallNode` with an empty `message_loc`. A backwards text scan is used for one thing — where the
  half-typed word starts, which decides what an accepted item *replaces* — and only after Prism
  has said the cursor is somewhere Ruby can be written. Without that, completion fires inside
  comments and strings.
- **A class or module body completes against the class, not an instance of it.** `self` in a
  class body is the class object, so `Scope::at` sets `self_id` to the nesting's singleton
  whenever the cursor is in a namespace with no enclosing method. Getting it wrong offers
  `valid?` where only `validates` is callable — measured, 49 suggestions that would all raise
  `NoMethodError`. The top level is *not* one of these: `self` is `main`, an ordinary `Object`.
- **`self_decl_id` must be stated for `MethodCall` and `NamespaceAccess`.** rubydex derives it
  only for `Expression`; the receiver contexts use it purely as the caller in their visibility
  check, and `None` there means "outsider" — a class would stop seeing its own private class
  methods. `Scope::caller` is the one place that decides.
- **Ranking is `(group, internal, tier, distance, locality, length, sequence, label)` and every
  field earns its place.** `internal` sinks names starting with punctuation or `_`, without which a Rails app
  opens `User.` on `__send` and `_fork`. `sequence` is *only* a keyword argument's position in
  its signature: rubydex's emission order within a namespace is a hash map's and reshuffles when
  a member is added.
- **`distance` is the only term for relevance, and without it a list nothing has been typed into
  is not ranked at all.** `tier` is 1 for every row at an empty prefix and `length` is 0, so what
  was left was the label, alphabetically — which is why `"hello".` shipped opening on
  `DelegateClass, Digest, append_as_bytes`. It sits below `tier` (what was typed is what was
  asked for) and above everything else. Because `take_best` uses `select_nth_unstable_by`, the
  cap keeps the nearest rows rather than the alphabetically first ones.
- **`Locality` measures nearness by directory, not by namespace, and that was decided by trying
  both.** A walk outwards through the cursor's lexical nesting is the more principled-looking
  answer and works on namespaced code, but a Rails model is `class Message < ApplicationRecord` at
  the top level — empty nesting, no signal, half a real app unranked. Ruby projects put related
  code in the same directory whether or not they nest it, and Zeitwerk makes the directory *be*
  the namespace, so the path carries everything the nesting carried and answers for flat code too.
  It also cost 8 ms less: the namespace version walked `owner_id` per candidate.
- **`group` is derived from `Locality`, not measured separately.** It is built from exactly the
  documents `is_own_code` accepts, so "is this theirs" is "did it score at all" — one walk over a
  declaration's definitions instead of two, on a path that runs over every method declaration in
  the graph. Changing what `Locality` is built from changes what `group` means.
- **`Distance` is seeded from the same walks rubydex is about to make, each numbered from zero,
  and the lexical walk only fills what the ancestor chains never reached.** Numbering the walks
  end to end would put every constant above every method, or the reverse. And `Object` is both
  the last rung of every ancestor chain and the outermost lexical scope: score it as the latter
  and everything rubydex could not attribute lands one step from the cursor. It keys on
  `Declaration::owner_id`, rubydex's own back-pointer, so nothing parses a name — `Foo::<Foo>`
  and a top-level constant need no special case.
- **`Context::allows_private` is the one test for "may a private method be written here", and it
  is pure syntax.** Ruby permits one with an implicit receiver and, since 2.7, with a receiver
  spelled `self` — through `.` and `::` alike, both checked against a real interpreter. rubydex
  asks a different question: its filter passes a private method whenever the caller's `self` is
  the same *class* as the receiver, so `Vault.new.secret` was offered from inside `Vault`, where
  a real interpreter raises `NoMethodError`. ya-lsp is deliberately stricter, and `reachable` in
  `completion.rs` is the single place the two are reconciled.
- **`ALWAYS_PRIVATE` is Ruby's list of five, not a heuristic.** `rb_add_method` privatises
  `initialize`, `initialize_clone`, `initialize_copy`, `initialize_dup` and `respond_to_missing?`
  at the point of definition, and neither rbs nor rubydex records it — rbs is not even
  self-consistent, marking `Kernel#initialize_copy` private and `String#initialize_copy` public.
  **Instance methods only**: the same code leaves `def self.initialize` public and
  `Bar.initialize` really does call it, which is what `singleton_owned` checks. `method_missing`
  and `singleton_method_added` are *not* on the list — they are private on `BasicObject` because
  that is how those copies were written, which the graph already knows.
- **`reachable` runs after `tier`, not before.** It costs a visibility lookup where the prefix
  test costs a string compare, and `by_name` runs the pair over every method declaration in the
  graph. `private_ok` short-circuits it entirely wherever no receiver was written.
- **`analysis::mod`'s `ANCESTRY` fixture pins order and reachability together.** Every other
  completion test asks whether a name is offered; the ranking bug shipped through all of them and
  through a benchmark that only ever measured milliseconds. One method per rung, and `first_rows`
  prints the owner beside the label so the assertion reads as a ranking. `Item#initialize` and
  its `private def stash` are in it so the pinned receiver lists prove their own absence — they
  appear only in the two lists reached without an explicit receiver. Add to it rather than
  starting a second one.
- **Keyword arguments are never guessed from a name.** `Context::Argument` only becomes
  `MethodArgument` when `locator::resolve` was *precise*; otherwise it degrades to a plain
  expression. Another class's parameters would be a syntactically valid wrong answer.
- **Every completion list is `isIncomplete`, and nothing found is `null` rather than `[]`.** The
  list was filtered against one prefix, so the client must re-ask rather than narrow it; and a
  `null` is what tells the client to fall back to its own word list inside a comment.
- **`MAX_COMPLETION_ITEMS` is not a latency control** — unlike `MAX_WORKSPACE_SYMBOLS`. Measured
  over 128/256/512/1024 it moves the worst request by about a millisecond, because the cost is
  the graph work in front of it. It bounds the response size. Re-measure before assuming
  otherwise.
- **`completionItem/resolve` carries the `DeclarationId` as a string.** It is a 64-bit hash and
  JSON numbers are doubles, so a round trip through a client silently corrupts a number.
- **`render::is_nameable` is the one test for "rubydex invented this name".** Angle brackets are
  its only punctuation for them — `Foo::<Foo>` and `<uri>:<offset><anonymous>` — and both
  completion and `workspace/symbol` go through it. A 17,557-file workspace answered `::` with a
  page of anonymous classes before it existed.
