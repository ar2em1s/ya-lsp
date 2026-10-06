---
paths:
  - "src/analysis/types.rs"
  - "src/analysis/cursor.rs"
  - "src/analysis/hover.rs"
  - "src/workspace/rails/**"
  - "src/workspace/rspec.rs"
  - "src/workspace/i18n.rs"
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
   came back imprecise. The view convention answers before the guess (`instance_read` asks
   `rendered_by` first; `types::named` is the guess alone). `cursor` never lets a
   `Receiver::Named` count as an assignment's answer. Remove any one of these and a guess starts
   displacing a checkable answer.
2. **When adding a rung, ask what it *displaces*, not just what it answers**, on both the navigation
   and the completion side. A precise wrong answer (`Namespace::Todo`, YARD `Object`) displaces a
   right guess.
3. **A `Receiver` reaching `types::method_receiver` must be in graph coordinates**
   (`Receiver::rebased`). Two offsets are exceptions: `Receiver::Assigned`'s is provenance, rendered
   against the buffer, and `Receiver::Variable`'s is a row of the text's own `cursor::Variables`,
   read from the same buffer.
4. **Generated RBS reaches the table only through `Types::harvest`.** `types.rs` spells no Rails,
   Sorbet or YARD word (`synthesized.md`). It asks `workspace::rails`' pure functions in two places
   only: the renderer rung (`template_renderers`, `every_renderer`) and the name guess's inflector
   (`class_named_like` → `rails::camelize`). The collection names come from `generated.rs`.
5. **A generated answer is *Derived*, never *Resolved*.**
6. **Never show a wrong type.** A label right only by luck is refused. A wider answer (a union, an
   extra `?`) is fine, and so is a guess that says it is one, which a hint never draws.

## The table

- **Keyed by `DeclarationId`, and looked up *after* rubydex found the member.** `[].tap` is owned
  by `Kernel`. `rubydex_spells_a_signature_the_way_this_keys_it` pins the key spelling, including
  `Shelf::Book::<Book>#open()`.
- **Resolve `self` at lookup, and everything else at harvest.** `Kernel#tap -> self` on a `String`
  is a `String` (`Return::Same`).
  - A Ruby body returning `self` gets the receiver too (`Typed::same`, `from_body` →
    `owner.one()`). A margin on the `def` itself keeps the declaring class.
- **A name a relation lacks is its model's class's** (`delegated`): ActiveRecord's
  relation hands it over with `public_send`. So a model's `def self.digest` answers on
  `Story.where(…)` and inside a `scope` lambda. Public members only; gated on models.
- **`Return::Element` / `Return::Collection`** are "this receiver's model" and "its relation"
  (a grouped relation's own class, where the receiver is one).
  They are the sentinel names `generated::ELEMENT` / `generated::COLLECTION`, read only in `class_of`.
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
3. **`Sources::constant_hops` (`CONSTANT_HOPS` = 10) is a cycle guard, not a depth.** `A = B.new`
   beside `B = A.new` would overflow the stack, which no bulkhead contains. Real chains run four
   deep (addressable's character classes); at 3 they answered nothing.
4. **`Namespace::Todo` is not a class object** (`is_todo` before `singleton_of`).
5. **Which constant a receiver is comes from `locator::locate_held`** (`constant_at`): `locate`'s
   targets, answered from a large document's sorted spans (`Indexed::spans`), since a body read
   asks once per constant it meets. A smaller document is walked as `locate` walks it.

## Reading RBS: which arm answers

Arms are partitioned by what the call wrote. A partition that disagrees answers nothing.

1. **By block:** `bytes` without a block is `Array`, with one it is `String`. A required block
   answers only calls that wrote one. `?{}` counts both ways.
2. **By arity:** `3.7.round` is `Integer`. An optional positional covers two arities. A rest covers
   all above it (`Arms::beyond`). Uncountable arguments get `Arms::any`. A call no arm accepts gets
   nothing.
   - **A keyword hash is kept apart** (`Arity::Keyed`, `Arms::keyed`): keywords to an arm that
     takes them, one more positional to any other, and only where that position can hold a `Hash`
     (`takes_a_hash`). So `create(title: "x")` reaches `(Hash)` and not `(Array)`, and a bare
     `where` is not `where(title: "x")`.
   - **Keywords are read by name** (`Arm::reached_by`). An arm with a required keyword
     is out of every call that writes none, at a counted arity and past every one (`beyond`). At a
     call, each required name must be written and each written name taken, unless a `**rest`
     takes any (`Types::returns_to`, `only_arm_reached`). A name no arm takes reaches no arm.
   - **`**opts` may be empty**, so it also reaches the arms of its plain count (`Arm::reached_at`).
   - **A keyed bucket keeps a row by itself** (`Arms::is_empty` counts it): `true === (a: 1)` is
     `bool`.
3. **By argument type**, only where the partition already refused (`1 + 2` → `Integer`). Match
   against the argument's ancestry. Abandon on any optional or rest positional, a non-class
   parameter, an unreadable argument, or two arms fitting.
   - **A symbol literal parameter is matched by name** (`pick_by_literal`, `Arm::symbols`),
     where every argument is a symbol literal (`Receiver::Literal::symbol`). **Asked before
     the partition**: an arm it picks is one the partition reached, so it only
     narrows what they agreed on (`pluck(:depth)` is `Array[Integer]`, where the arms agree on
     `Array`). The arms one document wrote are tried in written order, as RBS tries an overload
     set: an arm written as
     those symbols answers, one naming others is skipped, and any other reachable arm before a
     match refuses (a `(Symbol)` or a rest takes the call first). An `untyped` position between
     them takes any argument, so it neither refuses nor needs a literal: `create_list(:user, 3)`
     reaches `(:user, untyped amount, *untyped)`. Two documents' arms have no order, so they
     refuse. `Comment.where(…).pick(:depth)` is `Integer?`.
   - **A `T?` argument picks `T`'s arm** (`Typed::one` drops the mark). Where no arm takes `nil`,
     passing it raises (`find` raises `RecordNotFound`, `Integer#*` raises `TypeError`), so the
     answer holds whenever the call returns. Measured over six corpora: 17 hints, 12 through
     `find` and 5 through core, none through a gem signature. Requiring a `nil` arm lost only
     right labels.
4. **Arms differing only in `nil` agree:** the class is compared and the mark is OR-ed.
5. **Two arms that disagree about a type argument keep the head and drop the argument**
   (`Return::agreed`, `agreed_arguments`).
   - **The one arm a keyword call's names reach may take the block's value** (`handed` read for
     `picked`): `Rails.cache.fetch(k, expires_in: 1.hour) { … }` reaches only the
     `[T]` arm, since the `raw:` arm requires its keyword.
6. **Where nothing picked an arm, the call is every reachable arm joined** (`join_arms`):
   one of them runs. `Story.find(params[:id])` is `Story | Array[Story]`. Not for a splat, which
   reaches any arm. At least two arms, and every reachable arm readable: an arm the policy refuses
   (`untyped`, a tuple, a union the table drops) could return anything. A keyword call joins too
  : `CSV.read(path, headers: true)` reaches the `CSV::Table` arm by its keyword and the
   `Array` arm by its options `Hash`, and reading the body instead answered `Array` alone.

## Joining answers

**`Join` is the one rule for several values meeting at one place**: a read's writes
(`fold_reached`), both halves of a `T?` or a `bool` (`either_half`, `split_bool`), a signature and
the bodies disputing it (`disputed`), a method's exits (`read_exits`) and a block's
(`block_return`). Never merge classes, facets or provenance by hand at a new site.

- Classes union; `nil` becomes the mark and `true` beside `false` is `bool` (`Sides`), however a
  value spelled it: a `?`, a `NilClass` of its own, a `bool`.
- Type arguments survive where every value that adds a class agrees (`agreed_arguments`); `nil`
  has no vote.
- The weakest value decides the tier, first of equals kept (`Join::add` answers whether it just
  became the weakest, for a note naming it). Assignments are unioned.
- `Typed::same` and `Typed::made` describe one object and are dropped. `Typed::shaped` is kept
  where every value adding a class carries the same, except in a join of stored values
  (`Join::of_stores`).
- `nil` alone is `NilClass`; nothing added is no answer.
- **A block's value must still be one class** (`block_return` refuses a union after the join): it
  fills a type argument.
- **Provenance from another document merges through `Derivation::absorb`** (`read_body`,
  `disputed`), never field by field at the site.

## Unions, `nil` and booleans

- **Fold `X | nil` into `X` marked, and `true | false` into `bool`**, however either is spelled.
  **Only a lone pair folds to the carrier** (`Typed::boolean`, `Folds::fold`): beside another class
  `TrueClass` and `FalseClass` stay two classes of the union, drawn `bool`, so a call on
  `String | bool` runs on each of the three (`narrowed`). Folded, the union read as one class and
  every call on it refused. Every other union is kept whole: `Typed::one` refuses it, so it stops every step but a call.
  Completion lists each class's members, each row marked with its class (`completion.md`).
- **A signature's union return is read** (`Return::Union`): `typed_return` resolves each
  member and joins them, at a call, in `join_arms` and for a block parameter (`handed_to`, since
  2026-10-03). One unreadable member refuses the union. `resolved` answers `None` for it, so a
  type argument and a parameter's type still refuse one. `-1 | 1` is two literals of one class, so
  `Integer`.
- **An accessor whose storage only its writer fills is what that writer is given**
  (`Return::Written`, `generated::WRITTEN`). `written_to` reads every call of `name=`
  from `calls_named`, keeps those whose receiver is typed as the same object (a constant, `self`, a
  local holding it; one nothing types is passed over), types each value where it is written and joins them with `nil`. Only the
  application's own Ruby, fenced as the asking document. One untyped or guessed value refuses; a
  `nil` write adds nothing. The generator decides when the sentinel is safe.
  A class variable of the accessor's name spelled anywhere in the graph, or a `class_variable_set`
  in the application that can name it (`scopes::class_variable_sets`), refuses
  (`class_variable_written`): a plain `mattr_accessor` keeps its value there.
  **A store every instance of a class shares is filled on any of them** (`Return::Shared`,
  `generated::SHARED`, `shared_by`): Rails' configuration settings. Every call of `name=`
  counts, on any receiver: one typed as the reader's declaring class or a descendant is a write,
  one typed as anything else is another object's, and **one nothing types, or only a guess, refuses**.
  `nil` joins only where a write passes it: a read before any write raises.
  **A sentinel inside a union is the rest joined with the writes** (`written_beside`):
  `Rails.logger`'s `BroadcastLogger | WrittenByItsWriter` is Rails' value, plus what the
  application assigns. No write is the rest alone, and `nil` joins only where a write passes it.
  **A current attribute's store is its class object's and its one instance's** (`Return::Held`,
  `generated::HELD`, `current_held`, `Storage::Held`): every value `name=` or
  `set(name: value)` is given on a receiver whose class, or attached class, is exactly the
  receiver's (`base_of`; a subclass keeps its own store), with `nil`; beside a literal default (a
  union with `HELD`) as `written_beside` joins. One untyped or guessed value, `set(**options)`, a
  `send` of the writer on either (`sent_to`), and a writer the class `def`s in Ruby refuse.
  `Shapes::setters` records `set`'s keywords (`Setter::set`), which no other store counts.
- **A generated member standing for a `def` Ruby extends onto the receiver answers from that
  `def`** (`generated::DEFINED`, `Types::defined`, `defined_as_own`). The sentinel's arm is
  no vote; at the body seam (`from_body`) the Ruby declaration `Holder#name()` is read with
  `Sources::extended`, so a `self` written in `Holder` is the receiver: a concern's class method
  runs on the including class. Its ivar reads refuse (a block straight in a module body) unless
  the block's call names one object.
- **A member a call defines from its block answers that block's value** (`generated::BLOCK`,
  `Types::blocked`, `made_from_block`): RSpec's `let(:user) { … }`, and a method
  `define_method(:x) { … }` makes (`workspace/defines.rs`). The block is found at
  the member's own place (its mapping), in `cursor::blocks_handed_back` (a block with a `break` is
  left out), and read where it is written, so its own `[self: instance]` decides `self`. One exit
  nothing types refuses. A member placed in a spec file is fenced from a body read in a gem's file,
  so rspec-core's `self.class.described_class` does not reach it; the generator declares the
  instance side itself.
- **A member a call declared from its lambda is that lambda's truthy value, beside the rest**
  (`Return::Scoped`, `generated::SCOPED`, `scoped_beside`): a `scope` is
  `X::Relation | ScopedByItsLambda`, Rails' `instance_exec(…) || self`. The lambda is found at the
  member's own place (`cursor::lambdas_handed_back`, a call's one proc literal) and read where it
  is written; its `||` with the rest is `shortcut`'s (`reaching_end`). No place, two places, or an
  exit nothing types is the rest alone, what the member said before.
- **A member that hands its call on is the two calls** (`Return::Forwarded`,
  `generated::FORWARDED`, `forwarded`): the target asked of the receiver as a receiverless
  call (a private member answers) or a constant spelled from the receiver's class, then the call as
  written asked of that on a written receiver (public only, each half of a `T?`). `?` on the
  sentinel adds `nil` (`allow_nil:`). The hops' own derivation travels, so a guess stays one.
  **A member asked again while it is answered refuses** (`Reads::forwarding`): a target that is
  the receiver, or one a guess makes a class with the same member, recursed until the stack
  overflowed on a large application, and nothing contains that.
- **A union whose members all inherit one of them is drawn as that one** (`render::common_superclass`):
  `URI.parse`'s ten classes are `URI::Generic`. The type keeps every member.
- **A call on a union runs on each class that has the member** (`narrowed`). Ruby raises
  on a class without it, so that class adds nothing; the answers of the rest are joined.
  `Story.find(params[:id]).title` is `Story#title`'s answer.
  - A member `reach` refuses drops its class out too: private on a written receiver raises
    (`from_nil`'s rule), and a member the root gate withdrew is no member on any road. A script's
    top-level `def tags` must not refuse `Story#tags`.
  - Refused where a class might answer anyway, because it writes its own `method_missing`.
    Refused where no class has the member.
  - The jump agrees (`narrowed_classes`, `locator::on_a_typed_receiver`): each class that has it,
    answering its own declaration. They all reach one (`URI.parse`'s ten classes and
    `URI::Generic#host`): one place. They differ (`to_i` on `Float | Integer`): each, sure, **N
    definitions** on the card, as a concern's `self` answers (`locator::on_each`). One class
    failing the root or privacy gate leaves the name rung. `nil` keeps its own rule below, which
    is the same one.
- **`bool` is carried by `TrueClass`, but the lookup runs on both halves.** ActiveSupport gives them
  opposite `blank?` bodies.
- **A `T?` receiver asks `NilClass` too** (`returned_by`, `from_nil`), and the halves fold like
  `bool`'s. `Kernel#nil?` is `-> false`, so asking `T` alone draws a wrong literal.
  - `T`'s answer stands where `NilClass` lacks the member, the member is private (a top-level
    `def`; `locator::is_private`), or its answer is unreadable (`NilClass#to_h -> {}`).
  - Without those refusals the fold loses `x.id`'s `Integer` across a whole corpus.
- **A block on a `T?` is handed what `NilClass` hands it too** (`yielded_by`): `nil.then { |v| }`
  hands `v` a `nil`. Where `NilClass` lacks the member the block never runs on `nil`, and `T`'s
  answer stands. A `&.` call never runs the block on `nil` (`Receiver::Yielded::safe`).
- **`a&.m` is `M?`; `&.` skips one call, not the chain.** `NilClass` is not asked for the `&.`
  link, and on `nil` alone the answer is `nil` (`either_receiver`). The next link is an ordinary
  call on `M?`. An unmarked receiver is taken at its word.
- **A literal `-> false` is just `FalseClass`**, not a fold.
- **`!x` is `bool` when `x` is unknown, or typed only by a guess.** Never leave `!` to member lookup
  (`TrueClass#!: -> false`). A guess narrowing `!x` to a guessed literal would lose a label.
- **`&&` and `||` are executed, not unioned** (`Receiver::Shortcut`, `types::shortcut`). A left
  side that is never falsy or always falsy picks a branch. The branch not taken is never resolved.
  **`x || raise` is `x`'s truthy half**: its classes and `true`, without `nil` and `false`.
- **Spelling lives only in `render::typed`:** `bool`, `nil`, `true`, `false` in lower case, and a
  class object as `Foo:class`. It is a spelling, never a type.
- **Generics are drawn one level deep** (`render::held_by`). An unknown position is `untyped`. An
  all-unknown head gets no brackets. A union is spelled as-is.

## Type variables and blocks

- **A guess never becomes part of another type.** A block value that is only a guess does not fill
  a type argument (`block_return`), a guessed argument picks no overload (`pick_by_argument`), and a
  guessed left operand decides no shortcut (`shortcut`). Each would carry the call's tier around a
  class read off a name.
- **A class's type variable is answered by a literal receiver only** (`Return::Parameter`,
  resolved at `argument`): `[1, 2].first` → `Integer`. Every element must agree. The variable must
  belong to the receiver's own class (`Enumerable[E]` on a `Hash` is a pair).
- **Type arguments survive a step** (`Return::Class` → `Typed`). `self` and assignments pass them
  through. A body's exits must agree about each position.
- **A position holds a bare class, so what is not exactly one is not held.** An RBS argument
  written `String?` or `bool` (`arguments_of`), and a block value that may be `nil` or is `bool`
  filling a position (`held_by_return`), leave it unknown. Holding `String` would say no element is
  `nil`. A block value that is the whole return (`then`'s `U`) carries its `?` and `bool` onto the
  answer (`returned_from`). A block whose every exit is `nil` is `NilClass`, unmarked.
- **A method's own `[U]` is the block's value**, only when `U` is both the block's return and in the
  method's return (`Return::Block`; `map`, `then`).
  - The block's value comes from the `Exits` walk.
  - A `next` is one more value of the block, and a `break` a value of the call.
  - Walk the block only when `substitutes_a_block`.
- **A method's own `[X]` is the argument that binds it whole** (`Return::Argument`,
  `passed_variables`, `bound_to_arguments`): `ENV.fetch("PORT", 3000)` is
  `String | Integer`. Only where `X` is the whole type of one leading required positional and
  appears in no other parameter (`clamp`'s `(T, T)` and `(Array[X])` refuse), and only for an
  argument resolved or derived: an untyped or guessed one leaves the call unanswered. The whole
  return carries the argument's `nil` and union; a position in a class holds one class or nothing.
  Bound in `returned_for` and handed to `returned_from` as `picked`, which it reads first.
- **What a block is handed is a second table** (`Types::yields`, `Receiver::Yielded`), filled
  independently of returns. The arms must agree.
- **Tuples are a fourth table** (`Types::tuples`), readable only at a destructure
  (`Receiver::Destructured`, `returned_element`). **A tuple *return* is also an `Array`**
  (`returned_type`): `Array(x)`'s `[] | Array[T] | [T]` arms agree on `Array`. Only the
  return: a block parameter written as a tuple is the value Ruby unpacks across `|k, v|`.
  - Use blockless arms only, and refuse when the call wrote a block.
  - One unreadable position refuses the whole tuple.
  - A written `a, b = x, y` needs no table. A `*rest` refuses the names beside it; the `*rest`
    target itself is an `Array`.
- **Name a call by what Ruby looks up** (`[]`, `-@`), not by its span text.
- **`relations::query_interface` holds `Enumerable` with `E` filled in**, only where that changes
  the answer.

## Aliases

- **An RBS `alias` copies the target's row in all tables**, after the walk (targets can come later
  in the file).
- **A Ruby `alias` / `alias_method` copies too**, after the resolve, beside
  `place_generated_members`. It reads `Definition::MethodAlias`. Normalise the parentheses.
  Never displace a row.
- **A call that reaches an alias with no rows is a call of the method it renames** (`renamed`, in
  `returned_on`): that method's signature, else its body, with every rule a call gets. Only where
  every definition is an alias agreeing on the old name; the old name is looked up on the class the
  alias is written in, through its ancestors, as Ruby does when the alias runs. So an alias of an
  application's own method, or of an inherited one, is typed; the copy above never reached either.
- **A stand-in's rows are the Ruby `def`'s of the body it is mixed into** (`adopt_stand_ins`,
  before the Ruby aliases). RBS writes `Random::Formatter`'s `hex`, `uuid` and the rest on
  `RBS::Unnamed::Random_Formatter`, which `module Random::Formatter` includes, while Ruby's
  `random/formatter.rb` writes them on the module itself: the lookup finds the `def` first, as Ruby
  does, and `SecureRandom.hex` read its body. Only a body a signature mixes the stand-in into
  itself (`include`, or `extend` for the class object), only a member the graph declares there, and
  never over a row it has.

## Parameters

- **A `def` parameter is typed by its declared signature first** (`Receiver::Parameter`,
  `from_parameter`, `Types::Parameters`), then by the one call a body is read for, then, for an
  `initialize` read for one object, by that object's constructions, else by every caller (the
  three sections below). A default alone is not a type: it says what the parameter holds
  when the caller passed nothing, and a caller may pass anything.
- **Match positionals by slot and keywords by name** (`ParameterSlot`, `bound_by`).
- **No slot for `*rest`, `**rest`, `&block`, or anything after a rest.** The first three hold a
  class Ruby fixes instead (`cursor::containers`): an `Array`, a `Hash`, a `Proc` or `nil`.
- **Refuse a declared `Object`, `BasicObject`, `Class` and `Module`.**
- **A `def` inside `Class.new do` is declined.**

## One call's arguments

Where no signature types a parameter, **one call** reads the method's body with what it passed
(`Foo.bar(1)` is `Integer` where `def self.bar(baz) = baz` has no type). The method's own margin
and card at the `def` are what every caller passes (next section).

1. **The binding is one call's** (`Binding`, `Sources::bound`). `from_body` sets it, always, like
   `Sources::object`; it is a key into `Reads::bindings` (a reference could not outlive
   `Sources::walked`), and part of every read's memo key. `from_parameter` binds only the
   method the binding names, so another `def`'s parameter met on the way (an ivar's writer) is
   never bound.
2. **Only where no signature answers**: `passed_to` types the arguments at the seam in
   `returned_for`, in the caller's `Sources`. A guessed argument binds nothing, and its
   assignment offsets are dropped (another document's lines).
3. **Positionals by index** (`Shape::leading`): the required and optional ones land by index
   whatever a `*rest` or `...` after them takes, with a count Ruby accepts. A trailing required
   parameter moves which argument lands where, so none binds. A `**opts` to a method with no
   keywords may be empty (`Passed::spread`): the slot it would fill holds a `Hash` or its default,
   so it binds nothing there.
4. **Keywords by name** (`Receiver::Returned::keywords`, braceless only). `None` is no claim: a
   `**` splat, a key that is not a plain symbol. A method with no keyword parameters takes a
   braceless hash as one more positional, bound as a `Hash` (Ruby 3's rule): unbound,
   `create!(name: x)` read Rails' `attributes.is_a?(Array)` branch as reachable once checks
   narrowed.
5. **A left-out argument holds its default at that call** (decided 2026-09-24), read in the
   method's own scope with the binding (`def f(a, b = a)`), from `cursor::Shapes::defaults`. Only
   for a method with one Ruby definition, and never for a positional a braceless hash filled or a
   keyword after `**`.
6. **Every definition must agree on the parameters**, or nothing binds.
7. **The receiver rule is the body read's**: a self-call in the body starts at the receiver's class.

## What `new` passed

**An object holds what `new` passed it**: `Service.new(story).call` reads `@story = story` in
`initialize` as a `Story`, for that one object.

1. **`Receiver::Instance` carries `new`'s arguments**, recorded by `cursor` as for a `Returned`.
   The class is still answered as before; only the binding is new.
2. **`Typed::made` is the binding of the `initialize` the object runs** (`made_by`, a `Binding`).
   It travels with the object: a local, a `T?` and `self` in a body read for it keep it; a fold of
   two values and every new answer drop it. It is invisible to every step and label.
3. **`from_body` sets `Sources::made`, always, and only where `Sources::object` is set.**
   `from_parameter` binds from it only `initialize`'s own parameters. It is part of every read's
   memo key (`ReadKey`), and an argument's own `made` is part of a binding's key.
4. **Only through `Class#new`**, or a `new` nothing declares. A class's own `self.new` may pass
   `super` something else, so it binds nothing. `initialize` must have Ruby, or no argument is
   typed at all.
5. **The arguments are typed eagerly at every `new(…)`**, which costs about 1 s over the largest
   corpus's hints (decided 2026-09-24 to keep it; making it lazy is a later item).

## What every caller passes

**A parameter no signature types and no call's binding answers is what every call that can run its
method passes there** (`from_callers`, `read_callers`, `Derivation::callers`, decided 2026-10-02):
`def greet(name)` called with `"x"` and with `nil` is `String?`, at its margin, its card and every
body read without a call.

1. **Only the application's own methods** (every Ruby definition `is_own`). Spelled names and
   interpolated sends are noted for its documents alone (`indexer::named`, `Callers::note_names`,
   Ruby files, a template's code included), so a gem may call its own method by a name nothing
   here sees.
2. **Only the documents that name one of its classes or write the body of one of their ancestors
   are read** (`naming_documents`, every method since 2026-10-03, `initialize` alone before): a
   call typed as a runner in a document that names none is left out, as one on an untyped receiver
   is, and the union is narrower there. Every call of the name in them is read, gems' included
   (`Indexed::calls_named`, one index of every call, built when startup finishes
   (`Indexed::index_every_call` in `Analysis::generate`) so no first hint waits 45–170 ms for it,
   and caught up by content hash after a write), from the caller's buffer, fenced by the method's
   own document before counting: a spec's call is the suite's. At most `CALLER_SITES` (1,024) past the fence, a sanity
   guard: 4,096 answered the same. `initialize`'s calls are those documents' `new`s; a `new` on a
   constant counts only for the class it names, one on an object is that object's own method.
   - **Measured** (2026-10-02/03, on AC): coverage 53.45% → 53.20%, the
     largest corpus's peak 1,659 → 1,230 MB, the whole-file hints' cold sum 21.6 → 8.2 s.
   - **An alias's calls are callers too** (`called_as`). An alias copies what its class or module
     finds under the old name, so its calls are read where that lookup reaches the method or an
     alias of it, as calls of the method; an alias elsewhere copies another method. `new` renamed
     on the class object of a runner or of a class above one, or on `Class` or `Module`, builds as
     `new` does, on a class object that finds the alias. Each name is held to every rule of 3.
   - A call that reaches the method joins its argument at the slot, or the default it left out
     (read where the method is written). So does one reaching a generated copy whose body is the
     method's `def` (a concern's `class_methods do`, `defined_as_own`).
   - **A class method a framework installs beside it that runs it on a new instance of the class
     it is called on is a caller too** (`Knowledge::run_from_the_class`, `rails::run_from_the_class`,
     2026-10-03): a job's `perform_later`/`perform_now`, a mailer's action on its class. Such a
     call joins where it reaches the row the convention declared on a runner's class object
     (`installed_beside`). After `set(…)` or `with(…)` (`Knowledge::passes_to_the_class`,
     `passed_through`) the call is made on that class object, unless that class method is the
     project's own. A job's answer is always marked: a scheduler enqueues it by its class's name.
   - **A call on a relation of a name the relation lacks is the model's class's** (`delegated`),
     as typing reads it.
   - **A call of the enclosing `def`'s own `&block`** (`block.call(x)`, shaped `Receiver::Yield`)
     runs `Proc#call`, another method: skipped, not an unshaped call.
   - **A call that may run it joins too**: a union receiver one of whose classes can run it, or a
     class whose lookup reaches another method or none where an object of it may be a runner (the
     class is in a runner's linearization). Anything else is another method's call.
   - **A receiver nothing types, or only a guess, is not read** (decided 2026-10-02: "We don't
     know `klass` type? We don't use it in the caller args then"): it may be any object, and its
     argument is another method's as often as this one's. Joined, one gem's `x.new(hash)` put a
     `Hash` into every `initialize`. **Unless no other method has the name**
     (`Indexed::members_named`): the call runs this one or raises, and joins. A call ya-lsp cannot
     read is a limit of the tool, so the answer is narrower than Ruby's there, never another
     class's, and widens as more is read.
   - **A name spelled is a caller only where something calls by it** (`spelled_as`, on the
     literals `cursor::spelled_uses` reads in each document that spells the name, held per text
     by hash and fenced as a call there is):
     - the first argument of `send`, `__send__`, `public_send`, `try` or `try!` is a call of the
       name with the arguments after it, on the same receiver, read as one written out
       (`sent_as_called`); a sender the receiver writes itself (a socket's `send`) calls what it
       says;
     - a `when`, a hash's key, an `alias`, an `undef` and Ruby's calls that ask about, define,
       hide or compare a name (`UNCALLED`) call nothing; a one-word string is text anywhere but a
       sender's first argument;
     - a body of knowledge says what its framework does with a name its macro is passed
       (`Knowledge::spelled_use`, `rails::spelled_use`): a callback and its `if:` call the method
       with nothing on the class whose body writes it, so its default joins; `only:`, a routes
       file, an association, `render` name and call nothing;
     - **anything else may call it with anything** (`method(:x)`, `map(&:x)`, a gem's DSL, a
       constant holding the name): left out like an untyped argument, and the card says
       `| untyped`.
   - **What a call passes that nothing types, or only a guess, is left out** (decided 2026-10-02):
     an untyped value, a splat, keywords a `**opts` hands on, an untyped default. The answer is
     the readable calls' join, and `Derivation::left_out` marks it: the parameter's own card and
     its method's card write `| untyped` after its classes. Nothing computed from the parameter
     carries the mark (`Derivation::absorb` drops it), and every rule reads the classes alone.
     Where every call is left out, nothing is left.
   - A call reaching another method on a class that cannot run this one adds nothing, and so does
     one whose count Ruby rejects (`Binding::unfit`; beside a `**opts` it may not raise, so it
     refuses).
3. **Any sign of a caller no call records refuses the whole answer:**
   - the name built around an interpolation a sender can reach (`built_by_a_send`:
     `send("handle_#{x}", y)` is every `handle_*` with one argument, one sent to `self` only
     `self`'s);
   - anything before the method in a runner's linearization that declares the name
     (`runners`): an override below, a module a subclass includes, a prepend. Each may take the
     call and `super` into it with arguments of its own;
   - a writer, an operator, a protocol Ruby runs itself (`INVOKED_BY_RUBY`). **`call` is not
     one** (`CALLED_UNWRITTEN`, 2026-10-03): its written calls are read and the answer is always
     marked, since Ruby, Rack and every `&callable` call it where nothing is written. rubydex
     records no call for `x.(a)`, which the mark covers;
   - a method a framework calls (`Knowledge::called_by_a_framework`: a Sidekiq worker's `perform`,
     a channel's actions; `rails::called_by_rails`);
   - `initialize` through a `new` that is not `Class#new` on a class that can run it;
   - more calls than `CALLER_SITES`.
4. **A caller's argument that is its own method's parameter is answered the same way**,
   `CALLER_HOPS` = 3 deep (3 and 6 answered the same over the six corpora; 3 costs half). One
   passed straight back to a parameter being answered (`passed_back`: `again(n, false)` inside
   `again`) adds nothing: every value in the loop came in through another call. Anything computed
   from it refuses, and so does a two-method loop (the inner parameter has no caller of its own).
5. **The `def` read is its class's own member** (`from_parameter`): Ruby's lookup finds a prepended
   module's first, whose callers say nothing about what its `super` passes on.
   - **A `scope`'s lambda is read the same way** (`proc_parameter`, `scope_member`,
     `scoped_lambda`, `handed_to_lambda`): a parameter of a lambda literal no call of it binds,
     where the literal is the one a generated member's call was passed (`Return::Scoped`), is what
     every call of the scope passes, on the class or on its relation (both rows run the lambda).
     Bound as Ruby binds a lambda: a count it does not take raises and adds nothing, an empty slot
     holds its default (read where the lambda is written). Keywords, a splat and a `proc` are left
     out. The class method is the answer's key; its own document is the fence and the
     application's-own test.
6. **Held with the graph** (`Indexed::callers`): answers by method, slot and depth left, dropped by
   every write and every `didChange`, and not kept where a loop reached above them
   (`Callers::low`); who runs a method, dropped by a write; each caller text's call shapes by its
   content hash (`cursor::every_call`, at most `EVERY_CALL_TEXTS` = 64 texts, the text's walk held
   from the same parse where none is), kept across writes;
   each spelling document's literals by its content hash (`Callers::spelled`), kept across writes.
   Only the slot asked is typed at each call (`passed_at_slot`).
7. **Closed world, knowingly.** A gem calling the application's method by a name it holds itself
   (a `public_send` of a symbol a DSL stored) is a caller nothing records. Such a convention needs a
   `called_by_a_framework` row.

## What an object's constructions pass

**An `initialize` parameter read for one object is what that object's constructions pass**
(`from_constructions`, the user's design, 2026-10-03): `AccountSerializer#username` reading a gem's
`@object = object` is what the application's `AccountSerializer.new(account)` passed, never what
another serializer is built with.

1. **Where:** `from_parameter`, after a signature and a binding, for `initialize` where
   `Sources::object` is a class whose linearization holds the `def`'s class. Anywhere else, the
   section above.
   - **Refused for a class written as a value** (`handed_on`, `cursor::constants_handed_on`;
     decided 2026-10-04: "we want correct answers"): the class or a subclass handed to code that
     may build it with what nothing here shows (`mount_uploader :cover, CoverUploader` builds it
     with a `Symbol`), so the constructions read are not all of them, its own `new`s included.
     Fenced as its own document (a spec's `describe` is the suite's), and only a Ruby file's
     reference counts: a signature or a generated declaration (a `Struct` constant's namespace)
     names the class and builds nothing. Not values: a call's
     receiver, also through parentheses, a conditional's branches or `||`, and a local of one `def`
     every read of which is a receiver (`klass = Foo; klass.new`); a namespace; a class's name or
     superclass; a `rescue` or `when` class; the argument of `is_a?`, `kind_of?`, `instance_of?`,
     `include`, `extend` and `prepend`.
   - **A class with no `initialize` of its own that nothing builds is answered as the class whose
     `initialize` it runs** (the user's rule, 2026-10-03): that class's and every subclass's
     constructions. A policy only Pundit builds holds what the application builds its siblings
     with. A class with constructions keeps its own answer, one whose constructions are written
     but none is read (a custom `self.new`, an untyped argument: `Callers::left_out`) answers
     nothing, and one with its own `initialize` never falls back.
2. **The object reaches the write.** A write `instance_read` counts in a strict ancestor of the
   read's one object is typed for that object (`fold_objects`), and so is a setter call on `self`
   written there (`setter_values`), where nothing set `Sources::object` already: a direct `@object`
   reads as the `object` reader does.
3. **Who is built** (`built_as`): the object's class and every subclass, closed. One whose lookup
   reaches this `initialize` first is a runner, and its `new`s are read as the section above reads
   calls (`read_callers`: naming documents, the cap, receivers, arguments), fenced by the object's
   class's own document.
4. **Through an override.** A class whose lookup reaches another `initialize` first reaches this
   one through the `super` of the one right before it (`RunnerSet::through`). Each `super` in that
   `def` (`cursor::supers`, held per text by hash in `Callers::supers`) is a call of it, read for
   the object, so the override's own parameters are what the object's constructions pass it. A
   bare `super` passes the `def`'s parameters, each unknown where the `def` writes it; a rest, a
   `**rest`, `...` or a destructured parameter passes a count nothing knows.
5. **A gem's `initialize` is read here**: its callers are the application's constructions of the
   object, and the gem's own on a typed receiver. The gem's on a receiver nothing types are left
   out, as any such call is.
6. **Left out and marked** (`Derivation::left_out`): a custom `self.new`, an untyped argument.
   The answer is narrower than Ruby's there. An unresolved ancestor refuses (`built_as`), as
   `instance_read` does for such an object.
7. **Held with the callers' answers**, keyed by the object too (`Callers::held`,
   `Callers::runners`).

## Readers

**An `attr_reader` returns its variable** (`reader_return`), read as `instance_read` reads it in a
method of the same class: the object's writes, narrowed by `Sources::object`, `nil` unless every
`initialize` writes it.

- **Which variable and which side come from `scopes::readers`**, held per document in
  `cursor::Variables::reader_at` and keyed by the name's start, where rubydex files the method. A
  reader in `class << self` reads the class object's variable. A reader in a block straight in a
  namespace body is not placed, so it answers nothing.
- **An `attr_accessor`'s variable holds what its setter's calls pass** (rule 3 below).
- **Only where the name has no Ruby `def`.** A `def` after the reader is the idiom's override and is
  read as a body.
- **An RBS `attr_reader`/`attr_accessor` is a row in the table** (`Harvest::attribute`):
  a `def` with no arguments, on the singleton for `self.x`, so it answers before any Ruby reader
  is read. `attr_writer` declares no reader.

## Bodies: what a `def` hands back

1. **A recursion is refused where it comes back** (`Reads::bodies`): a body asked for again, for
   the same object, call and `new`, with no variable read opened since. A loop through a variable
   read is left to the reads' rounds (`@filters ||= []` beside `@filters = filters.first(3)`).
   **`BODY_HOPS` = 32** guards the rest; the deepest real chain is 13.
2. **Every exit is joined ([`Join`]), and an unreadable exit is an exit (`Unknown`).** Two classes
   are a union, which a chain cannot step off. Dropping unreadable exits turns a guard into a
   confident wrong type.
   - **A `raise` or `fail` exit is no value**: `cursor::Exits::push` never files one, for
     a body or a block, so `return x if ok; raise` is `x`'s type and no reader has to remember.
     Where every exit raises, nothing is left and the method declines. Syntax only: written with
     no receiver or on `self`, never `&.`. `cursor::never_returns` reads the same call as a `||`
     operand.
   - **A `retry` in tail position is no exit either** (`Exits::tail`): it runs its `begin`
     again, so the method leaves through that `begin`'s other exits.
3. **An unwritten branch is `Exit::Nil`**: `if` without `else`, a bare `return`, an empty body. A
   `return` inside `->` belongs to the lambda. `proc` and `lambda {}` are left alone.
   - **`MAX_BRANCHES` = 10 counts nesting, charged only by `Exits::branch`.** An `elsif` is a
     branch of the same conditional, like a `when`, and a tail `begin` is read at the depth a
     `def`'s own `rescue` is. Charging them again declined all-`String` methods
     (`nesting_is_what_the_branch_bound_counts`).
   - The corpora nest a tail 4 deep at most. Raising the bound from 4 to 10 changed no label and
     no time.
4. **An assignment hands back its value** (`cursor::assigned_value`), wrapped as
   `Receiver::Stored`: its value's type, but what the object held when it was made
   (`Typed::shaped`) is dropped, since the name it was written to may write into it. `unguarded`
   reads through it, as `dead` and `Typed::same` need.
   - A variable's `||=`, `&&=` and `+=` combine the old value with the new one
     (`Finder::rewritten`): `x ||= v` is `Shortcut { x, v }`, `x += v` is `x`'s `+`.
   - A constant's or a global's `||=` is the new value: nothing tracks their old one.
   - `obj.x = v` and `h[k] = v` are the argument (`attribute_written`).
   - `obj&.x = v` is the argument or `nil` (`Receiver::Either`, in `Finder::receiver_of`).
5. **`super` walks the ancestors *after* `self`** (`from_super`). It stops at an `Ancestor::Partial`
   and is refused in a module or outside a `def` (`super_in`). `Derivation::superclass` names the
   declaration.

- **The margin refuses `initialize` and setters; this module still answers for them** (`hints.md`).
- **Memos:**
  - **`Memo`, one per request and always present** (`Sources::memo`, built beside every
    `Analysis::sources`): `Reads` (each variable read, body and binding resolved, a cycle's rounds,
    and the documents read, with their `Rebase`), the scope walk (`Walked`), and navigation's
    `locator::Modifiers` and `Blocks`. No rung answers differently for want of a memo. A caller
    reading other text (completion's repaired buffer) builds its own.
  - **An object's `Hierarchy` is held with the graph** (`Indexed::hierarchy`, refusals too), keyed
    by the namespace, the side and `environment::Fence::key`, never by the asking document: every
    cursor in the project's code shares one. `graph_mut` and `forget_placement` (the fence's
    layout) drop it. Rebuilt per request, it was most of a slow hover: every renderer's
    descendants re-read by path on every view or helper read (5–7 ms, now under 2).
  - `HeldExits` (on `Analysis`, keyed by URI + content hash; the walks least recently used let
    go first past `HELD_WALK_BYTES`, 8 MB of text, about 70 MB of walks). It holds `cursor::Shapes` (exits, variables, defaults, raising `def`s, from one
    parse; eight of its parts from one walk, `cursor::Gathered`, since a walk of the tree costs
    more than most parts' own reading), plus the foreign-name, passed-name and render memos, and
    `locator::Modifiers`' walk (`HeldExits::escapes`, by URI + text hash, so safe for open
    buffers). **The cap sits above the widest single read**: one `instance_read` walks every
    writer document that spells the name, and a
    read wider than the cap empties it partway and parses all of them again on every request
    (889 `shopify_api` resources at 512: 190 ms a hover; 5 MB of text, under the cap). **A walk
    weighs about eight and a half times its text** (336 MB live for 39 MB of text over a long
    session), so its rows stay small: one shared value per parameter binding
    (`Reaching::bound`) and per instance variable's writes, the rare parts boxed
    (`Reaching::instance`, `Block::Breaking`). **8 MB, least recently used first, was chosen on
    2026-10-04, memory over a few re-walks.** A long session over the largest corpus (664 files,
    29,000 requests, ten open) ends 161 MB lower than at 64 MB for 5% more request time; hints and
    hovers over each corpus's 40 busiest files take 5% longer on the largest (its walks reach
    17.5 MB of distinct text) and no longer on the other five. The full audit's servers take 8%
    more CPU (one corpus 14%, the largest 9%, the rest within 1%) with identical answers. Before
    it: at 2,048 walks dropped at once, the largest corpus's files were walked 13 times each;
    64 MB of text (2026-10-02); and 8 MB refused on 2026-09-28, when the full audit took 72% more
    CPU for 19 MiB. It also holds
    **closed documents' texts** (`HeldExits::text`, `HELD_TEXT_BYTES`, 16 MB, least recently used
    first),
    shared into each request as `Rc<str>` (`types::ReadText`), never copied, which
    `Analysis::read_of` keeps only where the text read is the version the graph holds (its
    `content_hash`) and the document is not open, and forgets for the one document a `didOpen`
    opens (dropping every text there read the largest corpus's files from disk seven times as
    often). A file changed on disk and not yet indexed reads as the graph holds it. `inlayHint`
    hands over the walk of its own parse (`HeldExits::keep`), so does the callers rung where it
    parses a caller for its call shapes (`cursor::every_call_and_shapes`), and
    `cursor::shapes_of` hands the tree to `scopes` (`scopes::seed`), so one text is parsed once per
    request.

## The `!` mark

**A method whose own code writes `raise` or `fail` has its return spelled with `!`**, after any
`?`: `String!`, `String?!` (decided 2026-09-25).

- **A warning, not a proof.** `cursor::raises_in` counts every `raise`/`fail` in the `def`'s body
  and parameter defaults, blocks, lambdas and `rescue` clauses included, and not a nested `def`.
  A `raise` a `rescue` in the same method catches still counts.
- **Own code only.** A method returning a call to a `!` method is not `!`: resolving every call in
  every body to find the ones that raise would cost every request.
- **The method's, not the value's.** It is spelled only where a method's return is: the `def`
  margin (`hints::returns`, that `def`'s own body) and the card's `-> T` (`types::raises`, any of
  the method's Ruby `def`s). A variable holding the result is spelled by `render::typed`, unmarked.
  `Typed` carries no mark, so nothing downstream can pass it on.
- **One scan, `cursor::raises_in`.** The margin runs it on the `def` it labels; the card reads
  `Shapes::raising`, which the same scan fills, for every Ruby `def` of the method.

## Walk bounds

- **Every bound is a sanity guard, never a limit real code meets** (decided 2026-09-24). Set it
  above the deepest case the six corpora write, and show that raising it changes no answer and no
  time. A trip-logging build over the six corpora trips none.
- **`Budget { links, fanout }`, both against `MAX_WIDTH` = 64.** `links` counts a `.`, parentheses
  or a block's call. `fanout` counts a sub-expression a rule branches into (a shortcut's operands,
  a conditional's branches, arguments, a block's value). A variable read spends neither: it is a `Receiver::Variable`
  reference, not a walk. The longest chain in the corpora is 24 links.
- **An argument list is bounded by `MAX_ARGUMENTS` = 1024, not by `MAX_WIDTH`.** A list costs its
  length. The corpora write up to 89 positionals and 132 keywords in one call.
- **A read is answered once per request** (`types::Reads`), so a graph of reads costs its size,
  not its paths. Copying each write's answer into every read instead grew as 2^n and took a machine
  down. `READS_DEEP` (128) is a stack guard.
- **A cycle is settled in rounds** (`ROUNDS` = 6): a read still being answered is `pending`, adds
  nothing that round, and the head re-runs until two rounds agree on the type and its tier (the
  provenance may keep growing). No agreement is a refusal.
- **No overrides.** Re-measuring means editing the constant and building twice.
- **One `Unknown` ends a chain.**

## Assignments and variables

**A read is every write that can reach it, folded like a body's exits** (`cursor::Variables`,
`types::variable`, `fold_reached`). One write nothing can type refuses the read; it is never
skipped.

- **Which variable a read is comes from `scopes::every_variable`**: Prism's `depth` for a local
  (a `def`, `class` or `module` is a hard scope, a block sees out), what `self` is for an ivar.
  Never re-derived.
- **A local's reaching writes** (`reaching_local`):
  1. Every write before the read back to the last one that runs on every path; that one kills
     the writes above it. A write inside a branch (`if` arm, `&&` right side, `rescue`, a block,
     a safe call's arguments, `||=`'s value) adds and kills nothing.
  2. A loop or a block around the read brings back writes that take effect after it, unless the
     variable is written again on every turn before the read. The variable's own block does not:
     a block-local starts fresh.
  3. A block or lambda may run later: a read inside one sees every write below it in the
     variable's scope, and a write inside one is never killed.
  4. `nil` joins where no write runs on every path and no parameter binds the name. A parameter's
     own value (`Receiver::Parameter`, `Yielded`, a block-local's `nil`) joins the same way.
  5. A binding this walk cannot read (`for x in`, a pattern's capture, the names beside a `*rest`
     target) is an `Unknown` write, so a read it reaches refuses.
  6. **`rescue A, B => e` binds what was caught** (`Receiver::Rescued`): an instance of
     `A` or `B` (a subclass has every member), `StandardError` where no class is named. A splat,
     a module and a constant nothing defines name no class, and refuse. An instance variable
     target is the same write.
  7. **A check narrows each value that took effect before it** (`cursor::Narrowing`,
     `types::narrowed_by`), where the check holds: its branch, the right side of
     `&&`/`||`, a `when`, and the rest of a statement list after a guard whose other branch leaves
     (`return`, `next`, `break`, `raise`) or writes the local (`user = create unless user`).
     - The checks: the local itself (read, or written: `if (x = find)`), `nil?`, `== nil`,
       `!= nil`, `is_a?`/`kind_of?`/`instance_of?(K)`, `case x when K, nil`, with `!`, `not`,
       `&&`, `||` and parentheses read by Ruby's rules. A negated conjunction and a holding
       disjunction say nothing; neither does `&.`, nor a negated `instance_of?`.
     - Per value, never per read: a write below the check (or inside the conditional and outside
       its predicate, which a modifier writes first: `x &&= 2 if x`) is its own value; a write in a block or lambda the check is not inside may run
       at any time and is never narrowed; a loop's write coming back past the read is below the
       check. `nil` and the parameter's own value are above every check.
     - `is_a?(K)` keeps a class at or below `K`, makes one above it (or a module) `K`, and rules
       out one beside it; an untyped or guessed value becomes `K`. `K` must resolve to a class, or
       nothing is narrowed. A value every fact rules out is gone; all of them gone refuses the read,
       which is then unreachable (`Reads::unreachable`): a method exit or a conditional's branch
       whose value is a call chain rooted at it never runs and adds nothing (`dead`), a write of such
       a value never took effect (`typed_members`), and it is no name to guess from
       (`Receiver::Spelled`). Only the chain's root: a ruled-out read in an
       argument or a shortcut's side may never be evaluated.
     - Locals only: an instance variable may be written by any call between the check and the
       read.
  8. **An empty local container its method fills holds what it was given** (`cursor::Fill`,
     `types::filled_with`): a local whose one write is `[]`, `{}`, `Array.new` or
     `Hash.new`, every read of which is a statement adding to it (`<<`, `push`, `append`,
     `unshift`, `prepend`, `insert`, `[]=`, a `Hash`'s `[]=`/`store`), a member that hands back
     something else and changes nothing (`cursor::READS`), an `each` whose value is dropped, a
     `return`, or the `def`'s last statement. Anything else escapes (`cursor::Use::Escapes`: an
     argument, another variable, a chained `<<`). The element at each position is the one class
     every added value is, sure and never `nil`; otherwise the container stays as the literal
     said.
- **An ivar's reaching writes are every write any class of its object makes** (`instance_read`,
  `Hierarchy`). Methods run in any order, and any method of any ancestor of any class the object
  can be an instance of can write it:
  1. **The object** is the read's namespace (its singleton class for a class-level read) and every
     descendant the app loads, closed transitively. A class object's subclasses inherit its
     singleton methods; a module's includers do not.
  2. **The writers** are every ancestor of those, gems included. A write counts where the graph's
     nesting at the write is an owner on the same side (`Side`), so a reopening under any spelling
     is one class. A write in a strict ancestor of a single instance-side object is typed for it
     (`Sources::object`), so an `initialize` parameter it reads is what that object's
     constructions pass (see above).
  3. **A setter** (`attr_writer`/`attr_accessor`, `scopes::setters`, `InstanceWrite::setter`)
     writes what its calls pass (`setter_values`):
     - **The calls are rubydex's** (`calls_named("x=")`) in application Ruby, fenced as the
       reader, and **a call on `self` in a library's file of the object's classes**
       (`self.object = object` in a gem's `initialize`), typed for the object as an inherited
       write is. Such a library call only adds: one nothing types is left out, not refused. A call counts where its receiver can be the object: a class of it is one the
       object's classes are, descend from or include (`Hierarchy::owners`; a class object's
       singleton for a class-side variable). Its value joins, wider where the receiver was an
       ancestor's other object, never narrower. Another class's receiver adds nothing.
     - **Refused:** a receiver nothing types or only a guess; a value likewise; a hand-written or
       signature `def x=` in any owner (a column's writer), which may run instead; a call that
       can send `x=` by name (`cursor::Shapes::sends`: `send`, `public_send`, `__send__`, `try`,
       `try!` with one value) on a receiver that can be the object, in application Ruby, or on
       `self` in any file of the object's classes where the name is built to end in `=`
       (ActiveModel's `assign_attributes`, so an Active Record model's or `ActiveModel::Model`'s
       accessor refuses). A gem's forwarding (`public_send(*args)`) is its caller's call, read
       there. A value reading the variable again refuses (`Reads::setting`).
     - **No call adds nothing**, and `nil` joins only where a call passes it or rule 5 says.
     - **Not read:** a gem calling the writer on an object it is handed (not `self`), a multiple
       assignment's call target (rubydex indexes none), `method(:x=)`, a `delegate` of the
       writer.

     A reflective write on `self` (`instance_variable_set`, `scopes::reflections`) is an `Unknown`
     write, and
     `remove_instance_variable` writes `NilClass`. An interpolated name is a pattern
     (`scopes::Spelled`); a local names every literal it holds; a parameter names what its callers
     pass (`types::passed`, at most `CALL_SITES` = 64, read through `Indexed::calls_named`).
     Anything else matches every name.
     - **On another object it refuses every read it can spell**, wherever the read is
       (`written_by_another`): the receiver is rarely typed. Only application documents count,
       fenced like the reader; `Indexed::reflective_documents` finds them and `HeldExits` holds
       each one's names by content hash.
     - **On the top level's `self`** (`Reflected::Main`) it refuses only that text's top-level
       reads.
  4. **A block straight in a namespace body** (`scopes::Group::loose`) may run on either side: its
     writes count at both levels. Its reads are on what the block's call runs it against
     (`loose_read_on`, `rebound_self`): a callback's `if:` lambda, a mailer's
     `default` lambda, a one-includer `included do`. A union, or a block nothing rebinds, refuses.
  5. **`nil` joins** unless the reading method has written it on every path, or every class the
     object can be runs an `initialize` that writes it as a statement of its own body, following a
     top-level `super` up the chain (`initialized`). A class object never has one, and neither
     does a class whose superclass chain holds a class defined in a library's Ruby
     (`built_by_a_library`): Active Record builds a loaded record with `allocate`. `Object` and
     `BasicObject` do not count.
     - **"Statement of its own body" stops at the first statement that may `return`**
       (`cursor::may_return`, a block's `return` included, a nested `def`'s not): `return if x`
       above `@y = 1` leaves `@y` unset.
     - **A controller's callback is the other `initialize`** (`written_before`): in an
       action (a public receiverless `def`, level 0, that the application's Ruby of the object's
       classes never calls by name on `self`, `send` included: `action_called_by_name`), `nil`
       also stays out where, on every class the object can be, a method `Views::runs_before` says
       surely runs first writes the variable as an opening statement (`opens_with`,
       `Variables::opening`). The Rails rules (`only:`/`except:`, `if:`, skips anywhere on the
       chain, a callback's own method) are `workspace/rails/callbacks.rs`'s. A reader's return
       (`reader_return`) is never an action.
  6. **The reading method's last write decides where nothing runs between** (`InstanceRead::decided`):
     the last plain `@x = value` in the same `def` that runs on every path to the read
     is its only value, and no other write joins, where no construct that may run a method ends
     between the write and the read (`cursor::CallEnds`: calls, operator writes, `super`,
     `yield`, interpolation, splats, ranges, `case`, `for`, `def`, class bodies, constant writes),
     no other write of the variable starts there, and no loop or block around the read leaves the
     write out. The read's own chain (`@x.name`) and a call it is an argument of run after it,
     while a modifier's condition runs before the body written ahead of it (`@x.save if valid?`),
     so its calls are placed at the modifier's start. `||=`, `&&=`, `op=` and a multiple
     assignment's target never decide.
  6. **It refuses** on an `Ancestor::Partial`, a writer document that cannot be read, a write in
     another file typed only by a guess, no write anywhere (not `NilClass`: unseen code may write
     it), and past `OBJECT_CLASSES` (2048) or `WRITER_DOCUMENTS` (8192). Never a partial fold.
  7. **A linearization rubydex found cyclic refuses too** (`linearized`), and so does an object
     class sharing its last name with one (`Indexed::cyclic_named`). `class ApplicationController
     < ApplicationController` inside `module Admin` resolves to itself upstream.
     `Indexed::repair_superclasses` relinks it after every resolve, so
     only the shapes Ruby itself would raise at stay cyclic, and those still refuse.
  8. **A document is walked only if its text holds the name** (`Document::shapes` is lazy), or
     it is the application's and calls `instance_variable_set` or `remove_instance_variable`
     (`may_write`): a name it builds is never spelled. A library's is not walked for
     that, for `written_by_another`'s reason (actionpack's test helper removes every variable).
     The call is looked up in `Indexed::reflective_documents`, never in the text, except in a
     buffer edited since the last settle, which the index does not know yet.
  9. **A body read for a known receiver narrows the object** (`Sources::object`, set by `from_body`
     only). `Story#errors` hears `Story`'s writes, not every includer's; it is part of every read's
     memo key. The same field is what `self` is in that body (see `self` below).
  10. **Top-level (`main`) and island ivars** are this text's writes only; no writes refuses.
- **`Receiver::Spelled { was: Variable, name }`**: the read falls to its own spelling only when
  the fold answers nothing, never while it is still being answered in a cycle (`Reads::pending`).
- **Completion reads its receiver's variables from the repaired text** (`Cursor::repaired`), or
  Prism reparents everything below a dangling `.`. Only that `.` is blanked, and only where Prism
  took its message from a later token (`completion.md`).

## Conditionals read as values

**`x = c ? a : b` is whichever branch ran** (`Receiver::Either`, `either`): every branch's last
value, joined like a method's exits. `if`, `unless`, `case`, `case … in`, `begin … rescue … end`
and `a rescue b`.

- A missing branch is `nil` (`if` or `case` without `else`, an empty branch). A `case … in` without
  `else` raises instead, so it adds nothing.
- A branch ending in `raise`, `fail`, `return`, `next`, `break`, `redo` or `retry` hands nothing
  back. Every branch doing so is no value (`Unknown`).
- An unreadable branch is kept as `Unknown`, which refuses the whole, never dropped.
- The margin draws it where some branch is worth saying (`hints::worth_saying`).
- **A `begin … end` that rescues nothing is no conditional**: it is its last statement, as
  parentheses are (`cursor::unrescued`), an empty one `nil`; an `ensure` adds nothing. So
  `@x ||= begin … end` holds what the block ends with.

## Blocks, procs and lambdas at the call

What a block, proc or lambda hands back, and what it is handed, typed at each call the way a method's
arguments are bound into its body.

- **A block's value** (`Block`, `block_value`): its tail, and every `next` (bare: `nil`). A `break`
  is not the block's: the call returns it instead, so the call is its answer or any `break`'s value
  (`Block::Breaking`, `broken`). A `return` inside a block leaves the enclosing method: that path
  adds nothing. A `next`/`break` in a nested block or loop is that construct's (`cursor::Leaving`).
  `next a, b` and `redo` make the block unreadable.
- **A method returning its block's value** (`Receiver::Yield`): `yield …`, or `block.call` /
  `.()` / `.yield` / `[]` on the method's own `&block` never written to. Answered only inside a body
  read for one call, from that call's block (`Handed`, `yielded_to_block`); the `def`'s own label
  has none. No block, a forwarded one, or an untyped one answers nothing.
- **`&:name`** (`Block::Symbol`): `name` called publicly on what is handed; `nil` or a guess
  refuses (`called_by_name`).
- **A block parameter from a Ruby `yield`** (`yielded_from_body`), where no signature says: the
  union over every `yield` (and `block.call`) in the method's `def`s, read with the call's
  arguments bound (`Shapes::yields`). Ruby's block binding: a missing value is `nil`, or an
  optional parameter's default (read where the block is written); extras are dropped. One value
  handed to a block Ruby unpacks (`spreads`), a splat, keywords or a block argument refuse. Block
  keyword parameters stay untyped. Recorded as `Derivation::yielded`.
- **Proc and lambda literals in reach** (`Receiver::Proc`, `Typed::procs`, `proc_value`): a value
  every write of which is `->`, `lambda {}`, `proc {}` or `Proc.new {}`. A fold keeps the union of
  literals only while every value is one. A call (`call`, `.()`, `[]`, `yield`) reads each literal
  with its values bound (`ProcBinding`, keyed with the key in force, so enclosing bindings stay).
  A lambda refuses a count it does not take; a proc binds like a block. A lambda's `return` and
  `break` are its values; a proc's leave the method, so it cannot be read. `&fmt` is read at a
  `yield` in the context of the call that passed it (`Handed::Procs`), and for a signature's `[U]`
  with what the signature hands (`passed_return`).
- **`lambda {}`, `proc {}` and `Proc.new {}` are calls too** (`Receiver::Proc::call`): typed like
  any call, with `Kernel`'s signature, and drawn in the margin like one. They are
  the literal only where Ruby's method answered (`made_by_ruby`): `Kernel#lambda` or
  `Kernel#proc` alone, or `Proc.new` naming the core `Proc`. A class's own `proc`, or a
  namespace's own `Proc`, answers for itself. `->` is syntax and stays `Resolved`.
- **A literal's `break` is never a value of the call that made it**: it leaves the lambda, or
  raises once `proc` has returned (`block_written`).
- **A lambda a method returns is not read**: only a write in reach carries its literal.
- **The cost rules.** A call's block is typed for a body read only where one of the method's Ruby
  `def`s yields or calls its `&block` (`block_use`). A binding of its own makes every call
  re-read the body, which cost the largest corpus's hints 13% before this gate. A call's receiver is typed
  once, for `called_proc` and the lookup both.
- **`x.()` is `x.call()`**: a nameless call with its `(` is a call, not a half-typed `x.`.

## What a framework hands on to a class

- **A call on what `with(…)` or `set(…)` hands back is the class's** (`handed_to_the_class`,
  `Knowledge::passes_to_the_class`): `UserMailer.with(user: u).welcome` is `UserMailer.welcome`, a
  delivery. Only a row the convention declared beside the instance method it runs answers (asked
  by the `def` at the row's place, `Knowledge::run_from_the_class`); the value raises on any other
  name. Syntax: the receiver must be the `with`/`set` call itself, so one held in a local answers
  nothing. Asked whether or not the receiver is typed.
- **A literal key read off a mailer's `params` is what each `with(key: …)` passed**
  (`keyed_by_class_calls`, `Knowledge::keyed_by_a_class_call`, 2026-10-03), on the class object of
  the mailer or a subclass, read as the callers rung reads calls (naming documents, the read's own
  fence, a receiver typed as one of those class objects). `params[:key]` adds `nil` (made without
  `with`, or by a call that leaves the key out); `fetch(:key)` raises there instead. A call
  handing a hash it does not write out, or a `**` splat, answers nothing for every key. What a
  call passes that nothing types is left out and marked. Held per graph by class, key and depth,
  only where no other keyed read was open (`Callers::keyed`); one asked again inside itself
  answers nothing.

## Calls by name

**A member that takes a method's name as its first argument is read by what it names** (`NAMERS`,
by the declaration the call reached, so a class's own `send` is its own):

- **`send(:shout, 2)` is the call `shout(2)`** on the same receiver (`sent`, in `returned_on`):
  `BasicObject#__send__`, `Kernel#send` and `Kernel#public_send`, and a generated member returning
  `generated::SENT` (ActiveSupport's `try`/`try!`, written in `workspace/rails/framework.rs`). The
  named call gets every rule a written call does; its tier is the answer's. `send` reaches a
  private method, `public_send` and `try` do not. A symbol literal only: a string, an interpolation
  or a variable names what only running Ruby knows, and answers nothing.
- **A member that looks a key up is what the main locale holds under it** (`generated::KEYED`,
  `types::keyed`, in `returned_on` after `sent`; `synthesized.md`): the first argument as a String
  or Symbol literal and the literal keywords go to `Knowledge::keyed_type`, and the answer is
  spelled back as a type (`spelled_type`). The sentinel's arm is no vote. Derived.
- **A read whose value the member that made its receiver decides** (`generated::READ_OFF`,
  `types::read_off`, in `returned_on` after `keyed`): only where the receiver is written as a call
  with no receiver, no arguments and no block (`Call::on`, unwrapping `Spelled`), never a local
  holding it. That call's member is looked up on `self`, and the read's member, that member's name,
  the literal arguments, whether the read is in own code, and `self`'s ancestors by name go to
  `Knowledge::read_type`, whose answer is a spelled union (`spelled_union`: `bool` is both halves,
  `nil` the mark; one undeclared member refuses it). Harvested where any arm returns the sentinel
  (`returns_the`), so an overload set may hold it in one arm. The arm is no vote. Derived. The
  request's `params` is the one reader (`synthesized.md`).
- **A value a call makes from its literal arguments carries what a read of it hands back**
  (`generated::SHAPED`, `types::shaped`, in `returned_on` before `read_off`): the call's member,
  its arguments as written (`written_as`: a name, or a list or hash of names, read off
  `cursor::Literals`), and its receiver as a chain of calls down to one with no receiver, no
  arguments and no block (`chain_of`: that call's member, then each call's member looked up on the
  class the first answers) go to `Knowledge::shaped_type`. A constant argument or keyword value
  goes as the frozen literal of names its one assignment writes (`written_at`, `constant_names`,
  `synthesized.md`), read per text once (`Document::constants`). Its answer is the call's type (a spelled
  union) and a table of what a read hands back by member and literal key (`knowledge::Shape`),
  carried on the value as `Typed::shaped` and read by `read_off` (`shaped_read`) before anything
  else: one literal key, no block, no keywords; a key the table lacks answers as any read does.
  Harvested and voted like `READ_OFF`. Arguments it cannot count (a splat) go as one unreadable
  argument. Nothing answered leaves the member's body; the generated row hides it otherwise, so
  the module must still answer the call's type where it reads no filter.
  - **The facet is what the object held when it was made, so it travels only where nothing can
    have written into it**: the call's own value, its answers on each class of a union, a body's
    exits (a `def` handing it back), and a local every read of which leaves it as it was and every
    write of which is a statement whose value its list drops (`fold_kept`,
    `cursor::Reaching::kept`: each read a `Use::Reads`, so no write, no argument, no other
    variable, no block; no `x = y = v`), each where every value carries the same table. An
    assignment read as a value (`Receiver::Stored`: a `def` ending in `@x = v`, `x ||= v`, `a = b
    = v`) gives the object a second name, and drops it. **A store drops
    it**: an instance variable (`instance_read`), an accessor's or a setting's writes and a reader
    (`fold_reached`), a partial's locals (`Join::of_stores`), what a mailer's `with` and every
    caller passes. So do an argument bound into a body and a proc's or a block's value bound for
    a `yield` (`passed_to`, `proc_value`: their memo keys know nothing of it), `self` in a body,
    and every answer made anew.
- **`try` on a `T?` is `M?`**: `NilClass`'s own `try` answers `nil` from its body, through the
  `nil` half's usual rule.
- **`Derivation::sent` records which call answered**: everything else is about the named method.
  It adds no doubt to the tier.
- **`method(:shout)` is a `Method` bound to `Widget#shout`** (`Typed::bound`, `bound_to`), drawn
  `Method[Widget#shout]`: `method` and `public_method` (privacy as each reads it), a symbol the
  receiver has. It travels like a proc literal (`Typed::procs`): a local keeps it, and a fold keeps
  it only where every value adding a class is bound to the same method on the same classes.
  - **A call of it is that method's call** (`called_method`): `call`, `.()`, `[]` and `===`, with
    the call's arguments, on the object it was bound on (`Sent::Bound`).
  - **Passed as a block** (`&method(:shout)`, `&formatter`, `bound_return`) it answers a
    signature's `[U]`: the method called with as many values as the block is handed, none bound.
    A guess is refused, as for `&:name`.
- **The symbol navigates** (`locator::resolve_named`, `navigation.md`): `send`, `public_send`,
  `method`, `public_method` and `respond_to?` name a method of the receiver; `instance_method`,
  `public_instance_method` and `define_method` one of its instances. `singleton_method` and
  `define_singleton_method` are left out: on an object that is not a class they name that one
  object's method, which its class's of the same name is not.
- **Not read:** `instance_method(:x).bind(obj)`, `const_get`, `constantize`,
  `instance_variable_get`, `method_missing`, and names built in a loop.

## `block_given?`

**A body read for one call leaves out the side of `block_given?` that call cannot reach**
(`Receiver::BlockGiven`, `unreached`). `return to_enum(:each) unless block_given?` adds no
`Enumerator` to a call with a block, and a call without one gets only that.

- **What asks** (`cursor::implied`): `block_given?` with no receiver, and a read of the `def`'s own
  `&block` where its body never writes the name and no block's own parameter of that name shadows
  it. `!`, `not`, `&&`, `||` and parentheses are read by Ruby's rules; anything else says nothing.
- **What a side is:**
  - a conditional's branch, in tail position (`Exits::tail`), anywhere a `return` is
    (`visit_if_node`) and as a value (`Finder::branches`), a missing `else`'s `nil` included;
  - everything after a guard in its statement list: a conditional on the block with one side
    that always ends in `return` or `raise` (`Exits::guard`).
  - A block's tail asks about the `def` around it, whose frame it runs in. A nested `def` asks
    for itself.
- **Which call says** (`passes_a_block`): nothing in the block slot is none, a written block and
  `&:name` are one, `&value` is one unless the value is `nil`. An anonymous `&` (`Block::Forwarded`),
  `...` (counted as `Arity::Unknown`), and a value that may be `nil` or is only guessed say nothing.
  The `name` of `&:name` is called with no block, which is Ruby's rule. Only a call the text wrote
  reaches a body read with its block slot: `new` on a literal does so only where no signature of
  its own answers.
- **Only for its own `def`** (`given_here`): a value is left out only where the binding in force is
  that `def`'s. A lambda called under another method's binding keeps both sides.
- **`Kernel`'s alone.** A Ruby `def block_given?` anywhere in the graph leaves nothing out.
- **A `def` with no exit left answers nothing**, like one whose every exit raises.
- **The cost rule:** whether a call passed a block joins its binding only where a `def` of the
  method asks (`Shapes::asks`, `block_use`).

## What Ruby's syntax fixes

No method is involved, so nothing can override these:

- **`*rest` is an `Array`, `**rest` a `Hash`, `&block` a `Proc` or `nil`**, in a `def`, a block or
  a lambda (`Declared::Container`, `cursor::containers`). Anonymous ones bind no name. No element
  type: a signature's `*String` is not read.
- **A multiple assignment's `*rest` target is an `Array`** whatever the value
  (`Assigning::Splatted`); the names beside it still refuse.
- **`defined?(x)` is `String?`**: a keyword, the name of what `x` is, or `nil`.
- **`$1` and the back references (`$&`, `` $` ``, `$'`, `$+`) are `String?`**: each reads `$~`,
  which Ruby lets hold only a `MatchData` or `nil`.
- **`obj&.x = v` is `v`'s type or `nil`** (`is_safe_attribute_write`). `obj.x = v` was already the
  argument (`attribute_written`).

## `to_s`

**`x.to_s` is a `String` whatever `x` is** (`CONVERSION`, `converted`, `Derivation::conversion`).
`Kernel#to_s` puts it on every object, and `String(x)` raises a `TypeError` for one that returns
another class.

- **An override can still return another class**: a direct call is not checked. Of 4,584 in the
  corpora and their gems, 2,969 read as a `String`, and eight were found returning something else
  (one application's admin fields: an `Integer`, a `Hash`). Kept by decision on 2026-09-26.
- **No other conversion.** `to_i`, `to_f`, `to_a`, `to_str`, `to_hash` and the rest are not on
  `Object`, and overrides returning `nil` or another class were found for them (`to_a` and
  `to_ary` may return `nil` by Ruby's own rule). `to_sym`, `to_h`, `inspect` and `to_json` are a
  habit Ruby never checks.
- **Asked after the call's own lookup**, only where it answered nothing or a guess. A body or a
  signature says more (`def to_s = 42` stays `Integer`).
- **A known receiver must have the member** on every class it may be; otherwise nothing.
- **`nil.to_s` is `""`**, so only `&.` adds `nil`, and only where the receiver may be `nil`.
- **Derived** (`Derivation::conversion`), where the rule, not a body, answered.

## Blocks a signature rebinds

**`self` inside a block or lambda is what the call's signature says with `[self: T]`**
(`Types::selves`, `rebound_self`), asked before the body's `self`.

- **The syntax half is `cursor::BlockSite`**, in `Shapes::blocks`: every block, and every lambda
  or `proc {}`/`lambda {}` written as an argument, with its call's receiver, the method, and which
  argument it is (`BlockSlot`: the block, a position, a keyword). Only sites inside the innermost
  body around the offset count: a `def` in a block has its own `self`.
- **The table** holds the block's `[self: T]` and a proc-typed positional's, keyword's or
  `**rest`'s, where every arm binding that slot agrees. `instance` is the receiver's instance, as
  RBS reads it (`Rebound::Instance`), not the declaring class.
- **Innermost block first; a block whose call binds nothing is passed over** (`each` inside
  `before_save do`). So is one whose receiver is untyped or only guessed, or whose member is not
  found: as before signatures could say anything.
- **A binding that cannot be read against this receiver refuses `self`** (`Some(None)`): the
  body's `self` would be the wrong answer.
- **So does a `[self: T]` the table cannot read** (`Rebound::Unread`): `top`, a `self` that may be
  `nil`, a type variable nothing binds, and arms that disagree about a slot. The signature says
  `self` is something else; keeping the body's read a class object's `Integer` inside a
  `define_method` block where the instance's `String` ran.
- **`define_method`'s block, or the proc handed to it, runs as a method of the receiver**
  (`MAKES_A_METHOD`, `Module#define_method()`): read as `[self: instance]`, which RBS can only
  write `[self: top]`. `define_singleton_method`'s own `[self: self]` is the receiver already.
- **Navigation reads it first too** (`locator::rebound_call`), for a receiverless call only, before
  rubydex's lexical answer. The closure rung stays for blocks no signature describes.
- **The Rails rows are `workspace/rails/blocks.rs`** (`synthesized.md`).
- **A class a generator made for this very block answers first** (`Synthesized::ran`, keyed by
  where the call starts in graph coordinates; `generated::Runs`):
  - `Runs::Made` (`made_for`): a `describe` block's `self` is its group's class object. The
    derivation is `Derivation::ran`, the call's name.
  - `Runs::Each { of, classes }` (`each_of`): a concern's `included do` is the union of
    every including class's class object, `Derivation::each` naming the module. **A body read for a
    known object is that object's alone** (`Sources::object`): `scoped_beside` sets it
    to the class a scope was called on (a relation's model for a relation), so a concern's scope
    on `Article` is `Article::Relation`, never every includer's.
  - `Runs::Instance` (`instance_for`): an object of the class named, which a library runs the
    block on with `instance_exec`: a FactoryBot factory's block runs on a class made for that
    factory, its callbacks' on a `SyntaxRunner`, its attributes' on the evaluator.
  - `Runs::Refused` refuses.
- **A union receiver is asked per class** (`site_self_each`): a callback block inside `included do`
  is each includer's record, joined. Where only some classes have the member or rebind the block,
  `self` is refused, never passed over to the outer union.

## One call, one lookup

- **`reach` is the member a call reaches**, with every rule applied once: the suite fence
  (`member_of`), the class_eval root gate (`locator::declared_on_the_root`) and privacy (a private
  member answers only a call written on `self`). `returned_on`, `returned_element`, `yielded_on`
  and both `nil` halves go through it. `super` and a parameter's own method use `member_of`: they
  are not calls on a receiver.
- **`Call` is one call as written** (name, arity, block, arguments, `&.`, whether on `self`),
  built once in `method_receiver`. The call rungs take it, not the loose parts.
- **`new` has one rule for both spellings.** `Foo.new(…)` (`Receiver::Instance`) asks first
  whether the class's own `self.new` declares a return, as `new` reached through `self` does.
  **A constant alias builds the class it names** (`locator::alias_target`): arel's
  `Attribute = Attributes::Attribute`, `ActionDispatch::Response::Headers = Rack::Headers`.
- **`method_return` is what a surface asks about a method with no receiver**: the margin and the
  card. A signature, else the body, and the union a chain reads where bodies dispute the
  signature.

## `self` and receiverless calls

- **A receiverless call is an implicit `self`** (`Returned { on: SelfObject }`), never a name.
  The name rung stays below it through `Spelled`.
- **A `self` is placed where it was *written*** (`Receiver::SelfObject` carries its offset,
  resolved with `sources.scope_at`).
- **A private method answers only a call written on `self`** (`returned_on`'s `on_self`,
  `locator::is_private` with its repair); on any other receiver Ruby raises. A
  relation's `load` reached `Kernel#load`, private, and a gem's patch of it typed `rel.load` as
  `bool?`.
- **Inside a block a signature rebinds, `self` is what the signature says** (see above), before
  every rule below.
- **In a body read for a known receiver, `self` is that receiver** (`Sources::object`),
  when it descends from the class the `self` is written in. Ruby looks a self-call up from
  the object's class, so `CsvImporter.new.run` reaches `CsvImporter#parse` from inside
  `Importer#run`, and a module's body read for an includer calls the includer's methods. A `self`
  written in another class's body (a constant's assignment, `Class.new`) stays that class's. It
  carries the object's `Typed::made`.
- **A receiver typed as a class answers from that class's declaration.** Its subclasses are never
  read: an override returning something else is the code's own inconsistency (decided 2026-09-24).
  The hover card's `-> T` is the method's own, read for the class it is written in; a call's type
  is the margin's.
- **The innermost body decides.** `Class.new do` inside a `def` is the class object.
- **A module's own `self` has `Object`'s methods** (`locator::find_member`, `types::member_of`):
  its instance is some class's, and every class descends from `Object`. So `send`,
  `instance_variable_get` or `format` inside a module's `def` answer. Looked up after the
  module's own ancestors, never before. **Not in `super`'s walk** (`member_in`): there a module is
  one step of a class's linearization, and what follows it is the class's next ancestor. `Object`
  there answered ActiveSupport's `Object#with` before rspec-rails' own matcher `with`.
- **A name a module's `self` lacks is each running class's** (`self_runners`, `run_by`,
  2026-10-03). In a module's instance method `self` is an object of a class whose linearization
  holds the module (`module_runners`): its descendants, closed, that are classes or class objects;
  what a hook mixes it, or a module of that closure, into (`hooked`, `cursor::hook_mixins`: Ruby
  hands `self.included(base)` each includer and `self.extended(base)` each extender, so
  `base.extend(ClassMethods)` makes every includer's class object, and its subclasses', run
  `ClassMethods`). `extend self` makes the module object one of them, and rubydex already places a
  `module_function` body's `self` on the module object. A call written on `self` that the module's
  own lookup (its ancestors, then `Object`'s) has no member for is made on each class that has it,
  joined as a union's is (`narrowed`); one without it raises there. `Derivation::each` names the
  module. The module's own members stay its own, as a class's subclasses are never read.
  - **An object nothing here shows is left out** (decided 2026-10-03, the callers rung's rule):
    one the module is `extend`ed onto at run time (`obj.extend(M)`), a mixin in a block, a view a
    helper runs in, what a class's `method_missing` answers. The union is narrower than Ruby's
    there, never another class's, and widens as more is read.
  - **Nothing is said where `self` is another object:** a `def` written in a block
    (`class_methods do`, `locator::Blocks`). A block a signature rebinds and a body read for one
    object are read as before. Nor for a gem's module, which runs in whatever its gem builds.
  - Held with the graph (`Callers::modules`, `Callers::hooked`); the module's own document's fence
    leaves a spec's includer out. At most `SELF_RUNNERS` classes, a sanity guard. The jump agrees
    (`locator::typed`, `on_each`).
- **`Sources::walked` is the memo**, keyed by `UriId`. Only `scope_at` reads it.
- **`new` on a class object is an instance of it** (`instance_of` strips `::<`). Ask it after the
  signature and before the member lookup. The instance side is refused. It binds `initialize` as
  `Foo.new(…)` does.
- **`class` on an object is its class object** (`returned_on`), over core RBS's
  `Kernel#class: () -> Class`, which in a module cannot name the object's class. Only where the
  member found is `Kernel#class` itself (a proxy's own `class` is asked) and the receiver is one
  class: not a module, a singleton, a `Todo` or a `bool`. The answer is drawn `Foo:class`
  (`render::typed`), and chains through it (`self.class.new`, `self.class.helper`) continue.

## The guess

- **Only a bare name:** an ivar, an untyped local, or a receiverless call with no arguments and no
  block.
- **The member lookup filters most wrong guesses.** The card's guess line is the defence for the
  rest.
- **Look up a guessed class lexically, never through ancestors** (`constant_named`).
- **A guessed *receiver* is not the name list.** `Completion::precise` stays true, and a second
  field carries the doubt.

## The view↔renderer rung

- **Only the class the path names, exactly** (`declared`). Otherwise fall through.
- **Controller first, then a gated mailer**, both from `Views::rendered_by`. The derivation
  records which.
- **The template's variables are every renderer's object's** (`instance_read` over each
  renderer's `Hierarchy`), plus the template's own writes. Any action may have run first. `nil`
  joins unless every class of every renderer initializes it; the template's own writes never
  remove it. Only Ruby documents are read (`writes_ruby`).
- **The renderers** (`template_renderers`):
  - a full template: the class its path names, plus every class whose render call can name it
    (`rails::read_renders`, found through `Indexed::calls_named`, held by content hash);
  - a partial: every class a view can run on (`every_renderer`: the convention's, and every
    controller or mailer with a render call of its own, `calling_renderers`), plus the same
    calls. Held with the graph (`Indexed::renderers`), not one request: it walks every document.
    A write drops it, and so do `forget_placement` and a rebuilt view convention
    (`Indexed::forget_renderers`), its two non-graph inputs. Its own path names no class, so it
    is read wherever `[rails] views` is on (`Views::reads`),
    `user_notifications/digest/_stats` included;
  - a layout: every class whose views it wraps (`layout_renderers`: `every_renderer` filtered by
    each one's `Views::layouts_of`, held beside it as `Indexed::layouts`), plus the calls that write
    its name (`Render::writes_the_name`; `views.md`);
  - a call in a class counts as the graph's class at the call (a concern's includers come with
    it); in a view, as that view's renderer; in a helper, a partial or a layout, as
    `every_renderer`. Only controllers, mailers, helpers and views render with their own
    variables (`rails::renders_its_own_variables`);
  - a call with a receiver written in ActionView's shapes (`ApplicationController.render(…)`)
    refuses the templates it can name; any other `x.render(y)` is another library's.
  - No renderer at all refuses.
- **A helper module's instance method reads the view's variables** (`ReadOn::Viewed`, `helps`):
  the module's own hierarchy plus `every_renderer`, where `[rails] views` is on
  (`Views::helps`).
- **A bare name in an ERB partial is a local where its render calls pass one** (`partial_local`),
  asked in `returned_by` before the view's helpers, as Ruby reads a local first:
  - **The sites** are every render call that can name the partial (`rails::Render::names`; a
    name without a `/` is any partial of that name), in the application's templates and Ruby,
    fenced like the partial. Each call's locals come from `rails::read_renders` (`Render::locals`),
    each value typed where it is written (`cursor::values_at`, held on the request's `Document`).
  - **The type is the union over the calls that pass it**; one that does not adds nothing (the
    partial raises there), unless a helper of the name answers, which refuses. `collection:` hands
    each element (`each`'s block value), `_counter` an `Integer`, `_iteration` an
    `ActionView::PartialIteration`.
  - **An object rendered by its own partial** (`render @posts`, `collection:` alone) counts only
    where its class's `rails::partial_of` is the partial; a collection is what answers `to_ary`.
    An object nothing types refuses the partial's own name, and a class whose own code writes
    `to_partial_path` refuses.
  - **A strict-locals comment** (`rails::strict_locals`, read from the markup through
    `Sources::markup`) says which names are locals; a literal default joins, any other refuses.
  - **A jbuilder partial** is rendered by `json.partial!`, `json.array!` and any key
    written with `partial:` (`Render::json`), which pass every option but `partial:`, `as:`,
    `collection:` and `cached:` unless `locals:` is written, and each element or the object under
    `as:` (`Value::Either`). A key's name no call index lists, so every jbuilder view is a site.
    A JSON call finds jbuilder partials alone, an ERB view's call ERB ones; a controller's either.
    `json` is `JbuilderTemplate` in every jbuilder view.
  - **Refused, never a partial fold**, where a readable call or the comment says the name is a
    local: then a call whose locals nobody can list (`locals: options`), a component's `render`
    or an unreadable document may pass it anything. Without that word the name stays a call, as
    before (a helper beside `render x, options` still answers). A value nothing types or only
    guesses refuses too, and so does a partial passing its own local back to itself.
- **A variable a symbol names is read on its object** (`NamedVariable`, `named_read`,
  `named_writes`): the receiver's of `instance_variable_get(:@x)` and
  `instance_variable_defined?` (Ruby's own, `reads_a_variable`), and a class-body macro's `:@x`
  is its instances' (`cursor::macro_variable`), found by syntax, not by the macro's name.
- **Read every writer through `Sources::read`**, which returns text plus its own `Rebase`. Unsaved
  controllers count.
- **`Derivation::assignments` is cleared for a write in another document.** Its offsets belong to
  that document; `FromRenderer::lines` (the renderer's) and `FromClass` (any other class, with its
  file) name the lines instead. The jump (`renderer_writes`) names the renderer's first; where
  it writes nothing, or the path names none, every write the fold reads (`instance_writes`).

## What a card says of a type

- **Nothing about how it was found** (decided 2026-09-29). `Derivation` still records every rung it
  followed, for the tier and for tests; no surface turns it into a sentence (`navigation.md`).
- **A return read out of a body that rested on a guess is a guess** on the declaration card too
  (`hover::card`), so the card says the tier the margin acts on.
- **`the_two_tiers_a_reader_sees_drawn_side_by_side` pins what each rung reads as.**

## Known wrong answers with no repair yet

- **A render the call index cannot read as ActionView's**: `render(options)` in a view where
  `options` holds `template:`, a render through `send`, and a gem's render of an application
  template.
- **Reflection a call index cannot see**: `send(:instance_variable_set, …)`, a gem's write on an
  application object, and a name built from a method's keyword argument.
- **A correctly typed gem model has no columns here.** Its members vanish from the list.
- **`bot` is refused like `untyped`** for everything but `raise` and `fail`: a method declared
  `-> bot`, `exit`, `throw`. `loop` is declared `bot` and returns what a `break` passes. The card
  says so, though: a method every overload of whose signature is `void`, or every one
  `bot` (`Types::nothing`), or whose every Ruby `def` raises on every path (`never_returns`), is
  drawn `-> void` or `-> bot`.
