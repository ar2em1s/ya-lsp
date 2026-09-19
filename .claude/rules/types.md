---
paths:
  - "src/analysis/types.rs"
  - "src/analysis/cursor.rs"
  - "src/analysis/hover.rs"
  - "src/workspace/rails/**"
  - "src/analysis/annotations.rs"
---

# The types ya-lsp derives

rubydex models no types (`return_type` does not exist upstream). `analysis::types` is a table
*beside* the graph. If upstream ever adds return types, revisit this module.

## The five rungs, in order

1. **rubydex names the receiver** → *Resolved*.
2. **A signature or an assignment** → *Derived*.
3. **The class a template's path names** (a controller, then a mailer) → *Derived*.
4. **The receiver's own spelling** (`Receiver::Named`) → *Guessed*.
5. **The name-based list** → *Guessed*.

Below all five sits the `loadable_from` fence (`navigation.md`), which can only remove answers.

## Must

1. **The order is the safety argument.** `resolve_typed` only reaches past rung 1 when rubydex
   came back imprecise. `types::named` asks the convention before the guess. `cursor` never lets a
   `Receiver::Named` count as an assignment's answer. Remove any one of these and a guess starts
   displacing a checkable answer.
2. **When adding a rung, ask what it *displaces*, not just what it answers**, on both the navigation
   and the completion side. A precise wrong answer (`Namespace::Todo`, YARD `Object`) displaces a
   right guess.
3. **A `Receiver` reaching `types::method_receiver` must be in graph coordinates**
   (`Receiver::rebased`). The exception is `Receiver::Assigned`'s offset, which is provenance and
   rendered against the buffer.
4. **Generated RBS reaches the table only through `Types::harvest`.** `types.rs` never learns a
   Rails, Sorbet or YARD word (`synthesized.md`).
5. **A generated answer is *Derived*, never *Resolved*.**

## The table

- **Keyed by `DeclarationId`, and looked up *after* rubydex found the member.** `[].tap` is owned
  by `Kernel`. `rubydex_spells_a_signature_the_way_this_keys_it` pins the key spelling, including
  `Shelf::Book::<Book>#open()`.
- **Resolve `self` at lookup, and everything else at harvest.** `Kernel#tap -> self` on a `String`
  is a `String` (`Return::Same`).
  - A Ruby body returning `self` gets the receiver too (`Typed::same`, `from_body` →
    `owner.one()`). A margin on the `def` itself keeps the declaring class.
- **`Return::Element` / `Return::Collection`** are "this receiver's model" and "its relation".
  They are the sentinel names `types::ELEMENT` / `types::COLLECTION`, read only in `class_of`.
  `model_of` answers `None` for any other receiver.
- **One method, many documents: keep the raw arms per document and settle over their union.** A
  re-harvest of one URI replaces only that document's arms.
  - A document whose every arm is unreadable (`-> untyped`) is filed empty: it gets no vote.
  - Within one document, an unreadable arm still breaks agreement.
- **A signature disputed by more than one real Ruby `def` is unioned with the bodies.** The union is
  terminal. `:x.as_json` has two real answers, and we can't know which ran. `.rbs` definitions
  don't count as bodies.

## Constants

1. **A constant's own signature type** (`Types::constants`, keyed by the constant's `DeclarationId`)
   comes before the `Todo` and the singleton. Only a class instance type or a literal counts.
   `Derivation::constant`.
2. **Then the assignment** (`assigned_to`, `cursor::constant_assignment`), keyed by the name
   span's last segment, resolved in *its own* document and nesting. It is the one rung that maps
   graph → buffer (`span_to_buffer`). `Derivation::assigned_constant` names the file.
3. **`Sources::constant_hops` (3) is a cycle guard.** `A = B.new` beside `B = A.new` would overflow
   the stack, which no bulkhead contains.
4. **`Namespace::Todo` is not a class object** (`is_todo` before `singleton_of`).

## Reading RBS: which arm answers

Arms are partitioned by what the call wrote. A partition that disagrees answers nothing.

1. **By block:** `bytes` without a block is `Array`, with one it is `String`. A required block
   answers only calls that wrote one. `?{}` counts both ways.
2. **By arity:** `3.7.round` is `Integer`. An optional positional covers two arities. A rest covers
   all above it (`Arms::beyond`). Uncountable arguments get `Arms::any`. A call no arm accepts gets
   nothing.
3. **By argument type**, only where the partition already refused (`1 + 2` → `Integer`). Match
   against the argument's ancestry. Abandon on any optional or rest positional, a non-class
   parameter, an unreadable argument, or two arms fitting.
4. **Arms differing only in `nil` agree:** the class is compared and the mark is OR-ed.
5. **Two arms that disagree about a type argument keep the head and drop the argument**
   (`Return::agreed`, `agreed_arguments`).

## Unions, `nil` and booleans

- **Fold `X | nil` into `X` marked, and `true | false` into `bool`**, however either is spelled.
  Every other union is kept for display only: `Typed::one` refuses it, so it stops chains and
  empties lists.
- **`bool` is carried by `TrueClass`, but the lookup runs on both halves.** ActiveSupport gives them
  opposite `blank?` bodies.
- **A literal `-> false` is just `FalseClass`**, not a fold.
- **`!x` is `bool` when `x` is unknown.** Never leave `!` to member lookup (`TrueClass#!: -> false`).
- **`&&` and `||` are executed, not unioned** (`Receiver::Shortcut`, `types::shortcut`). A left
  side that is never falsy or always falsy picks a branch. The branch not taken is never resolved.
- **Spelling lives only in `render::typed`:** `bool`, `nil`, `true`, `false` in lower case. It is a
  spelling, never a type.
- **Generics are drawn one level deep** (`render::held_by`). An unknown position is `untyped`. An
  all-unknown head gets no brackets. A union is spelled as-is.

## Type variables and blocks

- **A class's type variable is answered by a literal receiver only** (`Return::Parameter`,
  resolved at `argument`): `[1, 2].first` → `Integer`. Every element must agree. The variable must
  belong to the receiver's own class (`Enumerable[E]` on a `Hash` is a pair).
- **Type arguments survive a step** (`Return::Class` → `Typed`). `self` and assignments pass them
  through. A body's exits must agree about each position.
- **A method's own `[U]` is the block's value**, only when `U` is both the block's return and in the
  method's return (`Return::Block`; `map`, `then`).
  - The block's value comes from the `Exits` walk.
  - A `next` or `break` anywhere refuses the block.
  - Walk the block only when `substitutes_a_block`.
- **What a block is handed is a second table** (`Types::yields`, `Receiver::Yielded`), filled
  independently of returns. The arms must agree.
- **Tuples are a fourth table** (`Types::tuples`), readable only at a destructure
  (`Receiver::Destructured`, `returned_element`).
  - Use blockless arms only, and refuse when the call wrote a block.
  - One unreadable position refuses the whole tuple.
  - A written `a, b = x, y` needs no table. A `*rest` refuses.
- **Name a call by what Ruby looks up** (`[]`, `-@`), not by its span text.
- **`relations::query_interface` holds `Enumerable` with `E` filled in**, only where that changes
  the answer.

## Aliases

- **An RBS `alias` copies the target's row in all tables**, after the walk (targets can come later
  in the file).
- **A Ruby `alias` / `alias_method` copies too**, after the resolve, beside
  `place_generated_members`. It reads `Definition::MethodAlias`. Normalise the parentheses.
  Never displace a row.

## Parameters

- **A `def` parameter is typed by its declared signature, then its default** (`Receiver::Parameter`,
  `from_parameter`, `Types::Parameters`), and asked last.
- **Match positionals by slot and keywords by name** (`ParameterSlot`, `bound_by`).
- **No slot for `*rest`, `**rest`, `&block`, or anything after a rest.**
- **Refuse `= nil` as a type, and refuse `Object`, `BasicObject`, `Class` and `Module`.**
- **A `def` inside `Class.new do` is declined.**

## Bodies: what a `def` hands back

1. **`BODY_HOPS` = 10.** Measured need is 6. It is a guard against cycles, not detection.
2. **Every exit must agree, and an unreadable exit is an exit (`Unknown`).** Dropping unreadable
   exits turns a guard into a confident wrong type.
3. **An unwritten branch is `Exit::Nil`**: `if` without `else`, a bare `return`, an empty body. A
   `return` inside `->` belongs to the lambda. `proc` and `lambda {}` are left alone.
4. **An assignment hands back its value** (`cursor::assigned_value`, twelve node kinds).
   - `+=` and `&&=` are left alone.
   - `obj.x = v` and `h[k] = v` are the argument (`attribute_written`).
   - `obj&.x = v` is refused outright in `Finder::receiver_of`.
   - `||=` is the memoisation idiom.
5. **`super` walks the ancestors *after* `self`** (`from_super`). It stops at an `Ancestor::Partial`
   and is refused in a module or outside a `def` (`super_in`). `Derivation::superclass` names the
   declaration.

- **The margin refuses `initialize` and setters; this module still answers for them** (`hints.md`).
- **Memos:**
  - `ReadBodies` (per request; holds `Rebase`)
  - `HeldExits` (on `Analysis`, keyed by URI + content hash, capped at `HELD_DOCUMENTS` and
    dropped whole)
  - `cursor::returns_in` (one parse per document)

## Walk bounds

- **`Budget { links, fanout }`, both against `MAX_WIDTH` = 20.** `links` counts a `.`, parentheses
  or a block's call. `fanout` counts "which assignment did this come from".
- **Candidates newest first, stopping at the first that fills the solid slot.** `Finder::memo`
  answers each (span, budget) once. It is unbounded on purpose.
- **No overrides.** Re-measuring means editing the constant and building twice.
- **One `Unknown` ends a chain.**

## Assignments and variables

- **Three slots, in order:**
  1. `solid`
  2. a write relaying a parameter (block, `def` without a default) or `super`
  3. a write rooted in a call on `self`

  `type_the_local` and `type_the_instance_variable` share them.
  - Slot 3 walks down the chain and stops at a variable.
  - Slot 3 sits below `yielded_to`.
- **A block parameter is asked after every assignment**, wrapped in `Receiver::Spelled`. The
  innermost block wins. A receiverless block call hands over nothing.
- **`Receiver::Spelled { was, name }`**: a typed shape falls back to its own spelling only when
  `was` answers nothing. This is a step, not a rung.
- **An ivar is wrapped in `Receiver::Assigned`** (the card names the line). A local is not.
- **An ivar its own file never writes is typed from the class above** (`from_ancestor`). This
  refuses:
  - the singleton side
  - an ancestor whose own answer is a guess
  - non-workspace documents
  - more than `ANCESTOR_DOCUMENTS` (8) documents, with `Sources::ancestor_hops` = 2
- **Which `@foo` it is comes from `scopes`**, never from re-derived logic.
- **Blank the half-typed call first** (`Finder::without_the_half_typed_call`), or Prism reparents
  everything below.

## `self` and receiverless calls

- **A receiverless call is an implicit `self`** (`Returned { on: SelfObject }`), never a name.
  The name rung stays below it through `Spelled`.
- **A `self` is placed where it was *written*** (`Receiver::SelfObject` carries its offset,
  resolved with `sources.scope_at`).
- **The innermost body decides.** `Class.new do` inside a `def` is the class object.
- **`Sources::walked` is the memo**, keyed by `UriId`. Only `scope_at` reads it.
- **`new` on a class object is an instance of it** (`instance_of` strips `::<`). Ask it after the
  signature and before the member lookup. The instance side is refused.

## The guess

- **Only a bare name:** an ivar, an untyped local, or a receiverless call with no arguments and no
  block.
- **The member lookup filters most wrong guesses.** The footnote is the defence for the rest.
- **Look up a guessed class lexically, never through ancestors** (`constant_named`).
- **A guessed *receiver* is not the name list.** `Completion::precise` stays true, and a second
  field carries the doubt.

## The view↔renderer rung

- **Only the class the path names, exactly** (`declared`). Otherwise fall through.
- **Controller first, then a gated mailer**, both from `Views::rendered_by`. The card says which.
- **`cursor::assignments_in` returns every write in file order, and the graph picks** (walked in
  reverse). `scopes::writes_to` decides which `@story`.
- **Read the renderer through `Sources::read`**, which returns text plus its own `Rebase`. Unsaved
  controllers count.
- **`Derivation::assignment` is cleared by `from_renderer`.** Its offset belongs to another
  document.

## Hover footnote

- **One line per *kind* of thing followed.** Consecutive repeats collapse (`Vec::dedup`).
- **It never says where a signature came from.** Hovering the declaration does.
- **`the_three_tiers_of_answer_drawn_side_by_side` pins all three tiers.**

## Known wrong answers with no repair yet

- **Two assignments naming two classes:** the textually last one speaks.
- **A correctly typed gem model has no columns here.** Its members vanish from the list.
- **`count` after `group` says `Integer`.**
- **`x || raise` can't be told from `x || anything`.** `bot` is refused like `untyped`.
