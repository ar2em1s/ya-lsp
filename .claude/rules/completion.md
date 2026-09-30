---
paths:
  - "src/analysis/completion.rs"
  - "src/analysis/cursor.rs"
  - "src/analysis/signature_help.rs"
---

# Completion and signature help

## The split

- **`cursor.rs` never sees a graph, and `completion.rs` never sees syntax.** That keeps the awkward
  cursor shapes cheap to test.
- **What a receiver *is* lives in `types::method_receiver`**, shared with navigation, so `person.`
  can't mean different things on a keystroke and on a hover.

## The cursor

1. **Read Prism's error recovery, not the text.** `Foo::` recovers to a `ConstantPathNode` with an
   empty `name_loc`, and `foo.` to a `CallNode` with an empty `message_loc`. A backwards text scan
   only finds where the half-typed word starts, and only after Prism has placed the cursor.
2. **A literal's class comes from the parse** (`Receiver::Literal`): `4.2.` is a `Float`.
   `Foo.new.` is `Receiver::Instance`.
3. **Classify after the walk (`Pending`), never during it.** `b = a.foo` must work in either
   statement order. `Receiver` is not `Copy`.
4. **A literal receiver with no declaration (`[rbs]` off) falls through to `by_name`**, never to
   silence. `completion::declared` returns `None`.
5. **`Receiver::SelfObject` carries the offset where `self` was written** (`types.md`).
6. **The repaired text blanks only a `.` whose message Prism took from a later token**
   (`Finder::took_a_later_token`): a line break before it, or a closing word (`i.| end`). A member
   already written on the line (`items.|size`, `items.first|(1)`) parses as written; blanking it
   left `items size`, a call to a method `items`, and the local was lost.
7. **The message of an operator write is a call** (`a.b ||= c`, `&&=`, `+=`;
   `Finder::operator_written`): the cursor on `b` is `MethodCall { receiver: a }`, by
   `visit_call_node`'s span test.

## A union receiver

**Every class's members, each row marked with its class** (decided 2026-09-30): a call on a union
runs on each class that has the member (`types::narrowed`), so each row is right for the values
that have it.

- **rubydex is asked once per class** (`receiver_for`, `from_graph`), each with its own chain
  numbered. Past `MAX_UNION_CLASSES` (64) the receiver is untyped and the name rung answers, as
  for every union before. `nil` stays folded out, as for a `T?`.
- **One row per name** (`one_row_per_name`). A name two classes declare apart is one row whose
  detail names both; the card is the kept row's.
- **A name several classes offer ranks at the farthest distance any gives it.** Every object's
  members sit three steps from an `Array` and sixty from a model, and the nearest would put
  `frozen?` above the model's own `save`.
- **The list's tier is the union's**, as for one class.
- **A bare word's rebound `self` is asked the same way** (`completion::rebound`,
  `types::rebound_self`): an example group, a concern's `included do`, a `[self: T]` block. Each
  class is one more `Expression` receiver beside the lexical one, since hover asks it first
  (`locator::rebound_call`) and falls back to the lexical answer.

## What `self` is

- **A class or module body completes against the class object.** The innermost body decides, so
  `Class.new(base) do … end` inside a `def` is the class. Held by
  `a_class_new_block_inside_a_def_completes_against_the_class_it_opens`.
- **The top level is `main`, an ordinary `Object`.**
- **Always pass `self` yourself.** `receiver_for` passes `Scope::caller` (`self_id.or(nesting_id)`).
  rubydex dropped its fallback, and `None` now yields no methods at all.
- **State `self_decl_id` for `MethodCall` and `NamespaceAccess` too.** `None` means "outsider", which
  hides a class's own private class methods.
- **`Cursor::in_a_closure` is a field**, so the file isn't parsed a second time.

## Ranking

The key is `(tier, internal, group, distance, length, generated, locality, sequence, label)`.

| Term | Rule |
|---|---|
| `tier` | Subsequence match. **Sigils first:** the label's `@`/`$` run must start with the prefix's. `@` admits `@@x`; `@@` does not admit `@x`. No sigil typed reaches all namespaces |
| `internal` | Sinks names starting with punctuation or `_` (via `significant`) |
| `group` | Derived from `Locality`: "scored at all" means "the user's own" |
| `distance` | The only relevance term. Seeded from rubydex's own walks, each numbered from 0. The lexical walk fills only what the ancestor chains missed. `Object` counts as the last ancestor |
| `length` | 0 with an empty prefix, the strongest tiebreak after the first keystroke |
| `generated` | Hand-written before table-derived, from any directory. `what_the_class_was_written_to_do_leads_what_its_table_says_it_holds_from_anywhere` |
| `locality` | Nearness **by directory**, not by namespace (Rails models have empty nesting) |
| `sequence` | Only a keyword argument's position. rubydex's order is a hash map's |

- **`take_best` uses `select_nth_unstable_by`**, so the cap keeps the best rows.
- **A generated document is scored at its source's step** (`Locality::at`, `synthesized::source_of`).
  `own_documents` stays untouched, which keeps rename away from generated code.
- **`Distance::extended` seeds the extend repair**, at `locator::Extends::step`, counting *classes*
  rather than ancestors.
- **`ANCESTRY` pins order and reachability together.** Add to it rather than starting a second
  fixture. `first_rows` prints the owner next to the label.
- **The name-based list is effectively ranked by `tier` alone.** Every list it returns has already
  been narrowed by the prefix. Don't split the comparator. `make audit-prefix` measures this path;
  `make audit-rank` does not.

## What gets dropped

- **A declaration defined only in a test tree, a migration or a generator template is dropped, not
  ranked** (`environment.md`). Sinking it would still use a slot and count against `by_name`.
- **The fence is off when the cursor itself is in such a tree** (`fenced_from`), and a declaration
  stays if *any* definition is loadable (`Tally`). `Locality::fenced` caches both gates separately.
- **A document `Locality` never scored counts as loadable.** A gem's `lib/foo/test/` is not the
  project's test tree.
- **`render::is_nameable` is the one test for names rubydex invented** (`Foo::<Foo>`, `<anonymous>`).

## Visibility

1. **`Context::allows_private` is the only test, and it is pure syntax.** A private method is
   allowed with an implicit receiver or `self.`. This is stricter than rubydex. `locator::Privacy`
   applies the same answer to navigation.
2. **`ALWAYS_PRIVATE` is Ruby's list of five** (`initialize`, `initialize_clone`, `initialize_copy`,
   `initialize_dup`, `respond_to_missing?`), for instance methods only (`singleton_owned`).
3. **`reachable` runs after `tier`.** `private_ok` skips it where no receiver was written.
4. **`reachable` repairs rubydex's visibility through `locator::Modifiers`.** A bare `private` inside
   a block is recorded against every `def` below it. This happens on the receiver path only, not on
   `by_name`.
   - **Known cost:** the memo lives for one request, so a few cursors spend nearly all their time
     re-parsing in `Modifiers::confirm`. The fix is a per-settle table like `indexed::Placed`. It has
     not been built.

## Per-candidate cost

`ranked_declaration` runs over every method in the graph, so anything per row runs ~100k times:

- **Spell the detail after the cap** (`detail_of`, `spelled_out`).
- **Ask each string question once:**
  - `in_an_unloadable_tree` answers both tags in one walk.
  - `segment_is_nameable` reuses the check already made.
  - `is_anonymous` looks for the first byte before the marker.
- **Keep `last_segment`'s two `rsplit_once` calls.** A hand-written walk measured slower.
- **Anything not about the cursor comes from `indexed::Placed`, once per settle.** It is lazy and
  dropped by `graph_mut`; a deferred keystroke does not drop it.
- **`shared_segments` takes the cursor's directory already split.**

## Rows from outside rubydex's walk

| Source | Joined | Rule |
|---|---|---|
| Concern class methods | as ordinary rows | Generated by `workspace/rails/concerns.rs` (`synthesized.md`) |
| `Extended`: `locator::extended_modules` | before `take_best` | Singletons only (`class_object`). `NamespaceAccess` looks up the singleton itself. Test the prefix before deduping |
| `InClosure`: a block written straight into a class body | before the cap; `add_closure` **fills gaps, never shadows** | Methods only, from the instance side. Declined in a `def`, a module, or when a receiver is written |
| View context (`in_view`) | before the cap; `add_view` **replaces** the graph's row | Carries its own step. `views.md` |
| `Object` beside a module's instance (`objects_side`) | in `ranked_for`; **fills gaps, never shadows** | A module's instance is some class's (`types::member_of`); numbered past the module's chain. After a `.` it is asked as a call, at a bare word as an expression, so private `Kernel` methods come |

- **A translation key is asked first, and alone** (`completion::keyed`): inside the
  literal key of a member that looks one up (`cursor::keyed_literal`, `locator::resolve_keyed`),
  the keys one segment on from what is written, from the main locale (`Knowledge::keyed_under`).
  A key with keys under it is a `Module` item, a leaf a `Field` with its text as the detail; the
  range starts after the last `.`. A `scope:` is not applied here.
- **Rows added here pass ya-lsp's `reachable`, not rubydex's filter.** `protected` is left alone;
  it is rare.
- **The audit's completion key poses only `member` cursors**, so it cannot see bare-word rows.

## The response

1. **`isIncomplete` is set when the cap dropped rows, and on every empty list.** Filters are
   subsequence matches, so an untruncated list is a superset the client may narrow itself.
2. **Nothing found is `null`, never `[]`.** `null` lets the client use its own word list in comments.
3. **The list's tier travels on `data`**, as strings (a `DeclarationId` is a 64-bit hash, and JSON
   numbers are doubles). `resolve_completion` builds the card from it.
4. **`[types] guess_from_names = false` also turns off `by_name`.**
5. **Three ceilings:**
   - `MAX_COMPLETION_ITEMS` (512) bounds response size, not latency.
   - `MAX_UNTYPED_CANDIDATES` (512) decides whether a name guess is offered at all.
   - `MAX_UNTYPED_COMPLETION_ITEMS` (128) is how many guessed rows are sent.
   - Measure them with `make audit-prefix` and `make audit-rank`, never by assumption.

## Latency

- **`make audit-latency` is the standing measurement:** one request in flight, at member cursors,
  run twice.
- **Read a p50 together with the empty count.** A declined list is the *expensive* path: `by_name`
  walks the name universe and then throws it away.
- **A change smaller than the run-to-run spread moved nothing.**

## Signature help

1. **`locator::precise_call` is the one gate for anything that shows a signature**: keyword
   completion, `signatureHelp` and outgoing calls. It keeps the `Foo.new` → `initialize` redirect.
2. **Never guess keyword arguments from a name.** `MethodArgument` requires a precise resolve.
3. **Callers pass in the privacy gate and `&Blocks`**, so a card is never drawn for a call that
   `definition` refuses.
4. **`cursor::call_at` keeps answering where `at` gives up** (inside a string, after `(person.`, in a
   comment between arguments), so the popup doesn't flicker.
5. **`Active` has three states:**
   - `Nth` counts arguments, with a keyword hash spread into its elements first.
   - `Keyword` matches by name, in both spellings.
   - `AnyKeyword` covers the gap after a keyword.

- **An argument's claim runs up to the next comma.**
- **The highlighted parameter stops at `*rest`**, or at the last parameter. LSP 3.17 cannot say
  "no parameter".
- **Overloads stay overloads** (`Signatures::Overloaded`). Pick the first arm that has the parameter
  being written.
