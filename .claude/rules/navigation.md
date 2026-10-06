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

## Who answers a cursor, in order

**Every question below walks one parse of the buffer** (`cursor::Parsed`): a request makes one
per `with_text` closure and hands it to each rung, the scope walk included (`scopes::variable`,
`instance_family`). Hover has one closure; `definition` still has three. A question taking `&str`
parses again: hover once parsed its buffer up to six times, a third of its time. Held by
`hover::tests::a_hover_parses_its_buffer_once`.

1. **Instance variables go to the scope walk first** (`locator::variable_at`, then `resolve_variable`).
   The graph answers `@name = 1` wrongly: it has only the write, and none of the reads. `definition`,
   `hover` and `documentHighlight` all ask in this order. `hover` alone falls through to the graph
   when nothing typed the variable, and **a read nothing typed gets the card its first write in the
   file gets** (`locator::written_variable`, asked last), which is where `definition` jumps.
   **Where the file writes nothing**, both read the writes the type side folds for the read
   (`types::instance_writes`): `definition` lists each in every file of the object's
   classes (subclasses too), and the card is the variable's declaration on the reading class's
   own ancestry first. One filter (`types::counted_writes`) serves the type and the jump, and one
   answer to whose variable it is (`types::read_on`): a read in a block a signature
   rebinds is that object's, and a template's is every renderer's, listed after the path's own
   class has answered (`renderer_writes`). A template's card names one class or none.
   - **Every write is a place** (`types::object_writes`): the name where it is spelled, else the
     `instance_variable_set` or accessor that makes it. Renderers whose ancestry cannot be read
     are left out of a template's or a helper's list, never out of the card's fold.
   - **A loose occurrence is grouped where its block runs** (`locator::occurrences_at`,
     `types::rebound_level`), so `if: -> { @x }` jumps to, and lights with, `def load`'s `@x`.
   - **A symbol naming a variable** (`locator::named_variable`: `instance_variable_get(:@x)`, a
     macro's `:@x`) gets a read's card and jump, and highlight lights it with this file's writes.
   - **A bare name a partial's render calls pass as a local** (`locator::partial_local`)
     gets a variable's card (`story: Story`) and jumps to each value passed, before the graph.
2. **Then the graph**, through `locator::locate_written` and `locator::resolve_typed`, which `hover`
   and the four gotos share. It has three tiers (`types.md`). Two calls rubydex misfiles
   (an operator write through a call, a call in a constant path's parent) are read from the parse (`cursor::misplaced`), **only where the graph
   holds nothing better at the cursor**:
   - **An operator write through a call** (`a.b ||= c`, `&&=`, `+=`): the reference filed on the
     operator or the `.` is answered, moved onto the message, keeping only the read name
     (`locate_written`). `cursor::at` reads that message as a call on `a`, so the typed rung runs.
   - **A call inside a constant path's parent** (`a.b::C`) has no reference at all.
     `locator::resolve_misplaced` answers it by the typed-receiver rung (`on_a_typed_receiver`,
     shared with `resolve_typed`), else the name rung, fenced the same way. `hover` and `definition`
     only.
3. **A method's name handed to a member that takes one** (`cursor::named_symbol`,
   `locator::resolve_named`, asked through `locator::symbol_at`): the first argument of
   `send(:shout)`, `method(:shout)`, `try(:title)`, `instance_method(:shout)`, anywhere.
   `references` asks it too, and lists the symbol's method's uses (`search-references.md`).
   - **The call is resolved first, as a jump from its own name would be**, and only a precise,
     unguessed answer counts: the member it reaches decides whether the symbol is a name at all
     (`types::named_by_symbol`, `NAMERS`), and the class it was sent to is where the name is found,
     privacy included. So a class's own `send` names nothing, and `send(:x)` straight in a class
     body names the class object's `x`.
   - `documentHighlight` asks it through a closure, only where the graph and the scope walk had
     nothing: it parses the buffer.
4. **A macro's `:symbol` last** (`cursor::macro_symbol`, `locator::resolve_symbol`).
   - A macro is any receiverless call written straight into a class body. Look the name up in the
     enclosing class's ancestors: the instance side first, then the class object.
   - **Positional arguments only.** Skip `dependent:`, `on:`, `only:`. A name no ancestor declares
     answers nothing.
   - **No macro table in `analysis/`.** That would put a Rails word outside `workspace/rails/`.
5. **A translation key** (`synthesized.md`): the literal key a member that looks one up
   is handed (`locator::resolve_keyed`) jumps to the main locale's line writing its last segment
   (`Knowledge::keyed_entry`, an ordinary file `Location`), before a `require` path; its card is
   what that key holds, as YAML, the key first (`hover::keyed`, `i18n::yaml`: a subtree nested and
   cut after ten lines, a value with no text a YAML comment). A key the main locale lacks answers
   nothing.
6. **A local answers `definition` with nothing** (you can see the line). This is pinned as a gap in
   `a_local_is_not_this_question_and_both_requests_say_so`. `typeDefinition` does answer at a local.

- **Rebase the type, never the places.** Variable writes come from the buffer and are already in
  reply coordinates. The nesting and the `Receiver` go through the map. A symbol's target is a graph
  declaration, so it goes out through `locator::sites` and `link`.

## Templates' instance variables

- **`requests::template_variable_links` follows the walk into the renderer's document.** The class
  comes from `types::renderer_documents` → `Views::rendered_by`, the same source the card uses.
- **Link every write**, typed or not (`scopes::writes_to`). The tier stays *Derived*.
- **A partial no single controller renders answers nothing.** Several writes in several files is a
  different answer, not a wider one.
- **Mailer views hang off the mailer**, but only where no controller exists (`views.md`).

## Hover cards

1. **The answer, ya-lsp's lines, then the prose** (`hover::compose`, the one place the shape lives):
   the fenced answer, one italic line per thing ya-lsp says, a rule, and the documentation comment
   a person wrote. ya-lsp's lines sit above the prose: a class's comment runs to hundreds of lines.
2. **A card says what the answer is, never how it was found** (decided 2026-09-29). ya-lsp says
   two things only: *Guessed from name alone.* (`hover::GUESSED`, whatever was guessed: the method
   by its name, the receiver's type, a return read through either) and *Defined in N places.* (a
   method's or a variable's, never a class's or a module's). No derivation, no generated
   provenance comment, no hint tooltip: Derived reads as Resolved.
3. **The type is beside the name.** A method's `-> T` is what its signature declares, else its body
   read, else this call's type (`locator::call_type`). A variable's or constant's `: T`
   is what it holds (`types::constant_type`). A typed instance variable's card is the variable
   (`hover::variable`, `Owner#@name: T`), never the card of the class it holds. **A local, a
   block's parameter and a method's parameter get `name: T`** (`hover::local`,
   `locator::local_type`, `cursor::local_at`): at its binding, a read, or the `def`'s header. A
   method's parameter itself (the header, or a read no write reaches) says `| untyped` after its
   classes where a call that passes it something was left out (`types::Derivation::left_out`); a
   local holding something computed from it does not.
4. **A list card counts and does not list** (`hover::listed`): `**N definitions**` for an exact
   answer per class, `**N possible definitions**` and the guess line for the name rung, counted as
   spelled. `definition` at the same cursor shows the places.
5. **Parameters are what a person wrote** (`hover::written_def`): the method's own Ruby `def`, else
   the `def` at one of a generated declaration's places. **Each parameter's type goes before it**
   where ya-lsp has one (`hover::parameter_types`, `types::parameter_type`: the signature's, else
   what its callers pass), RBS's order kept for every kind: `greet(String | untyped name, Integer
   times = 2, bool loud: false)`. Signature help keeps the bare names. Defaults as written where they fit on the
   line (`types::written_defaults`, `render::LONGEST_DEFAULT`, one line). An RBS signature's unnamed
   parameter keeps rubydex's `argN`; a generator names a writer's `value` (`synthesized.md`).
   - **An alias has the parameters of the method it renames** (`hover::aliased`, decided
     2026-09-30), Ruby's or RBS's: every definition an alias agreeing on the old name (a generated
     row beside it does not count), looked up through the class's ancestors as `types::renamed`
     does. Without it `alias send __send__` read as taking nothing, and i18n's `l` printed its
     row's typing-only `default:` arm. Held by
     `an_alias_is_called_with_the_parameters_of_the_method_it_renames`.
6. **A namespace a body of knowledge invented is shown as it says** (`Knowledge::shown`,
   `render::shown_as`): `ActiveRecord::Relation#where`, a bare `story_path`. Only where every
   definition of the name is generated, so a project's own class of that name is its own.
7. **Cards are pinned whole and side by side** (`every_hover_card_in_one_file_drawn_side_by_side`,
   `GALLERY`). Add new constructs there. `attached_name` takes the part *before* `::<`.

- **"Defined in N places" is `places().len()`**, never a second count.
- **Offsets become lines in `analysis::mod`.** `hover::markdown` knows nothing about encodings.

## `typeDefinition`

- **It asks what type the thing is.** `story.author` goes to `class Author`. The code is
  `locator::type_of` → `types::method_receiver`, so it matches the card and the completion list.
- **A binding** (`story = …`, `|story|`) goes through `cursor::bindings_in` with an empty range, the
  margin's own call, which handles destructuring. **A read** goes through `Finder::receiver_of`. **A
  call** answers only when the cursor is on the message name.
- **Instance variables go to `resolve_variable` first.**
- **A singleton type lands on its attached class** (`locator::attached_class`).
- **A guess is refused** (`requests::jumpable`), **and so is a union** (`Typed::one`).
- **A parameter in the `def` header is not answered**; a use inside the body is.

## `declaration`

- **It returns the `.rbs` only, never the `.rb`** (`locator::all_signatures`). The other narrowings
  are shared through `locator::narrowed`.
- **An empty answer is `null`**, so the client falls back to `definition`. Never fall back to the
  `.rb` yourself.
- **Generated and annotated declarations answer `null`.** Nothing wrote a signature for them.
- **Graph half only.** No scope walk, no `require`, no symbol. It is refused for guesses.

## rubydex's spellings

- **Drop references that start with `<`.** rubydex invents `<Foo>` references for singleton
  resolution. For an implicit receiver they span the whole call.
- **An anonymous `*`, `**` or `&` is recorded under the sigil itself**, so `render`'s `sigil` writes
  it once.
- **Rebuild a qualified name from its `ParentScope` chain** (`constant_path`). `resolve_outwards` is
  the lexical half of constant lookup.
- **Is `self` a class object here?** A receiverless call in a class body records the singleton, and
  inside a `def` it records the class. `attached_class` answers both.

## `Foo.new`, extends and `Resolution`

- **`Foo.new` answers `Foo#initialize`** (`locator::constructor`, through the attached class). It
  stands aside for a hand-written `def self.new`, or when only `BasicObject#initialize` exists.
- **`Resolution::redirected` exists so `references` can skip the redirect.** Anything new that reads
  `Resolution` must decide which it wants.
- **`Resolution::receiver` is the class the call was about**, set only on precise answers.
  `implementation` reads it.
- **Concern class methods are generated declarations** (`workspace/rails/concerns.rs`), found by an
  ordinary ancestor walk.
- **rubydex never linearizes an `extend` indexed after its namespace was declared**, while a late
  `include` works. So:
  - The generator writes members, never an `extend`.
  - `locator::extended_member` / `extended_modules` (`extends_written_on`) repair a user's
    late-indexed `extend`, only after the ancestor search found nothing.

| The `extend` arrives in | rubydex links it |
| --- | --- |
| any file indexed with the namespace | yes |
| an edit to the one file declaring the namespace | yes |
| an edit to one of several declaring files | **no** |
| a new file reopening it | **no** |
| a new signature in `sig/` | **no** |

Held by `an_extend_is_read_wherever_it_is_written` and
`an_extend_written_after_the_first_resolve_is_still_read`.

- **Take only what an extended module declares itself, never its ancestors.** rubydex files an
  `include` written inside a `def` as a mixin of the enclosing namespace.
- **Ask the extend edge before keeping a hit on `Object`, `Kernel` or `BasicObject`.** A root hit
  carries almost no information.

## Privacy on navigation (`locator::Privacy`)

1. **A private declaration is not an answer where Ruby forbids the call.**
   `cursor::Context::allows_private` decides it from syntax.
2. **Gate all three rungs** (resolved, derived, name). Otherwise the same `def` comes back as a
   guess. If nothing is left, the answer is nothing.
3. **Refuse by re-resolving, never by emptying the `Resolution`.**
4. **`Foo.new` is exempt** (`holds_private` skips redirects), because `initialize` is always private.

- **`locator::Modifiers` repairs rubydex's visibility record.** A bare `private` inside a block is
  applied to every `def` below it. The rule is the one that never refuses: a modifier governs only
  the body it is written in, and a block body counts as a body. It only narrows refusals.
  `completion::reachable` reads the same repair. **Its walk of a file is held across requests by
  the file's text** (`types::HeldExits::escapes`), in that text's offsets, and mapped onto the
  graph's per request; a re-parse per request was a fifth of a hover. Held by
  `an_edit_that_leaves_the_def_where_it_was_is_walked_again_and_not_answered_from_the_held_walk`.
- **`signatureHelp` is gated** (`Privacy::written(call.allows_private)`). Keyword completion, outgoing
  calls, `references`, `rename` and the hierarchy prepares are not.

## The name-based fallback

- **At a class-object receiver, drop candidates owned by a `class`**
  (`reachable_on_a_class_object`). The singleton walk already covered them. Module-owned candidates
  stay. **Where the class object is certain, an empty list is the answer**
  (`ClassObject::Certainly`, `on_a_proven_class_object`): rubydex named a class's or a module's
  singleton, and the syntax proves it is `self` (`proves_the_class_object`: a written receiver
  other than `self.`, or a statement of the body). Falling back to the whole list sent a
  `Settings::General.app_domain` to a configuration setting. Only a root's method (`Object`,
  `Kernel`, `BasicObject`) survives an empty filter there: every class object reaches it, and a gate
  (a block's `def`, a test tree, privacy) refused it. **Where nothing proves it, the list is
  narrowed, never emptied** (`ClassObject::Perhaps`): a bare word in a block of a body, whose
  receiver may rebind `self`; a singleton rubydex made for a constant that holds a value or that
  nothing defines; and every caller without the buffer (`resolve`, so `documentHighlight`,
  `references` and the hierarchies). Making `resolve` certain cost a `rescue_from` block's call its
  highlight of the `def`.
- **Except where a generator said what the block's `self` is** (`types::rebound_self`).
  A concern's `included do` and `prepended do` blocks are the case: rubydex names the
  module's class object, which Ruby never makes `self` there (Rails runs the block against each
  including class, a callback block inside it against a record). `workspace/rails/concerns.rs` says
  which classes (`Runs::Each`) or refuses (`Runs::Refused`). So where `self` is the receiver (bare
  or `self.`), `typed` never reads the module object: **each including class's own member where
  every one has it** (`locator::on_each`, shared with `rebound_call`; precise, one declaration per
  class), else the name rung whole with nothing said about the receiver. `Mod.x` written out is
  untouched, and a plain `each` block in a module body keeps the module.
- **A name a module's `self` lacks is each running class's own member** (`types::self_runners`,
  `types.md`), where `self` is the receiver (bare or `self.`): the classes that have it, through
  `on_each`, before the name rung. Where the module has the name, or no running class has it, the
  rungs below decide as before.
- **A precise answer can be several declarations** (`on_each`, and a call on a union through
  `on_a_typed_receiver`): the card counts them as **N definitions** (`hover::listed`); only the
  name rung's list says *possible definitions* and guesses.
- **A receiverless call in a block a signature rebinds is looked up on the signature's `self`
  first** (`locator::rebound_call`), before rubydex's lexical answer: `before_save do
  update(…) end` is the record's `update`, even where the class object has one. Precise and
  derived; the root gate applies. Only where no receiver is written (`cursor::at`, parsed only
  inside such a block).
- **Inside a block in a class body, use the closure rung** (`locator::in_a_closure`). It needs both
  halves of the evidence:
  - the graph: the attached class has the member on its instance side
  - the syntax: `cursor::closure_in_a_body`, carried on `Cursor::in_a_closure`

  It fires only where the answer was already a name list. It never fires in a `def` or in a module.
  Its tier is *Derived* (`Derivation::closure`). When the class object also has the name, the class
  object wins. `completion::InClosure` is the matching list.
- **`locator::loadable_from` fences the imprecise answer on the way out of `resolve_typed`**: test
  trees, generator templates and outside documents. Where it empties the list, the answer is silence.
  The rules live in `environment.md`.
- **The root arm is the one precise rung that is fenced** (`resolve` passes `false`). A fenced hit
  falls through to `by_name`.
- **The receiver walk steps past a member only the suite loads** (`locator::find_loaded_member`,
  shared with `types::member_of`, and the extend repair does too): a spec helper that `extend`s a
  library's module is in the chain only for a cursor in the suite. A template's member never
  reaches the root arm, so `Fence::loadable_on_a_root` reads only test and migration trees. One application's `MessageBus.publish` went to
  `spec/support/diagnostics_helper.rb` as a *Resolved* card. The answer stays precise: it is the
  next member up, the one the application reaches.
- **A root member whose every `def` sits inside a block is not a member** (`locator::Blocks`,
  `declared_on_the_root`).
  - A block body has no namespace in rubydex, so `class_eval do def x` lands on `Object`.
  - One `def` written as its own body keeps the whole declaration.
  - The check applies on three roads: the root arm, `constructor`, and the typed rung.
  - Withdraw the claim, not the `def`: it stays in `workspace/symbol` and in the name list.
  - **The walk goes on past it** (`locator::past_the_root`, `types::member_past_the_root`), as
    Ruby's does: `Object` never had the member, so `Foo.new.freeze` is `Kernel#freeze` however many
    blocks write a `freeze`. Stopping there dropped the class from a union, a wrong type.
- **Nothing else precise is filtered.** `references` is never filtered: its fence has the tree
  gate off, so the walk is rubydex's own.

## Places (`locator::places`)

`locator::site` is the only point where a declaration becomes a place. Generated declarations map
back there, or answer `None`.

1. **A place is narrower than a definition.** Drop:
   - `.rbs` files when source survives
   - a copy that `require` would never reach (`Workspace::load_paths` order, keyed by path under a
     load path)
   - a non-`file:` URI
   - a test-tree copy when a loadable one survives

   A signature that is the only answer stays.
2. **`references` uses `locator::sites`, not `places`.** Every mention counts.
3. **One `def` reached from several declarations is one place.** `locator::all_places` and `sites`
   key on the name span. First occurrence wins.
4. **Rank a wide namespace's places by file name** (`definitions_of`, a stable sort):
   - Tier one: the squashed constant is contained in the file's stem and covers at least half of it.
     The smallest excess wins.
   - Tier two: a directory the constant names, then the file nearest the top.
   - Methods are not reordered this way (`a_methods_places_are_not_reordered_by_a_file_name`).
   - Strip the scheme first (`the_uri_scheme_is_not_a_directory_named_file`). Non-file documents
     sort last.
5. **`preferred_definition` picks the one definition every list points at**, and `declared_in` says
   whether any is the user's.

- **A jump is placed without opening the file** (`position::range_in`, `core-invariants.md`).

## Outlines and spans

- **Build `selectionRange` ⊂ `range` only through `locator::spans`.** VS Code throws away the whole
  outline over one bad pair. Never build it from `offset()` and `name_offset()`.
- **A definition with no name is not an outline entry** (`is_outline_worthy`).
- **A definition matches its name span, never its body**, or hover fires over whitespace.

## Rendering (`render`)

- **Spell a construct in one place.** `split_qualified` and `qualified_name` both go through
  `singleton_parts`, and `symbols::kind_of` is shared with `search`.
- **Anonymous `Class.new` / `Module.new` names** (`<id>:<offset><anonymous>`) become `Class.new` or
  `Module.new` on every surface, through `render::spelled`. Replace every occurrence. Match the key
  as digits, colon, digits.
  - Spell first, then take apart (`class_object_of`).
  - `(is_anonymous, spelled)` both ranks and dedupes the candidate list.
- **`render::signature_label` writes the label and parameter spans in one pass, in UTF-16.**

## `require` paths

- **`requires::at` and `requires::all` are one visitor.** Only a bare `Kernel#require` counts, with
  no interpolation, and not inside comments or strings.
- **A require the graph can't place gets no link.**
- **`documentLink` needs no rebase.** A path string can't move.

## RDoc → markdown (`render::to_markdown`)

1. **No raw HTML reaches a `MarkupContent`.** Convert markup, escape what only looks like a tag
   (`<vowel>`), and drop `rdoc-ref:` links but keep their words.
2. **Never touch code:** fenced blocks, indented blocks and backtick spans.
3. **RDoc verbatim is two spaces**, except under a labelled list item (`[+x+]`, `x::`), where it is
   the item's description. Recognise `list_item` first.
4. **`+word+` equals `<tt>word</tt>`**, opening at a non-word boundary. `1 + 2` stays prose.
5. **A leading `:nodoc:` means no card** (`is_directive`).

## Where a lookup goes wrong

- **A superclass spelled like its own class** (`class ApplicationController <
  ApplicationController` inside `module Admin`) resolves to that class upstream.
  `locator::unless_its_own_superclass` answers the class Ruby names
  (`indexed::superclass_outside`), or nothing where Ruby would raise. **Only at that reference**:
  the same spelling elsewhere in `module Admin` is the same rubydex name, and there it does mean
  `Admin::ApplicationController`. The chains are repaired in the graph (`Indexed::repair_superclasses`),
  not here.
- **A top-level constant written under `SimpleDelegator`** (`locator::constant_named`). `Delegator
  < BasicObject`, so rubydex, following Ruby's lookup, resolves it to nothing; Ruby then asks
  `const_missing`, which `Delegator` defines as `::Object.const_get(n)`. Where rubydex resolved
  nothing and the first `const_missing` up the class side of the writing class's ancestors is
  `Delegator`'s, the name is looked up at the top level: for the constant at the cursor and for a
  call's receiver (`Current.account`). Another `const_missing` may answer anything, and gets
  nothing. It was answered before only by index order (Ruby's library is indexed after the
  workspace), and lost after an edit re-resolved the file.
- **`def Foo.bar` and `def Foo::bar` name their receiver.** `locator::declaration_of` answers them
  outright, not as a fallback, because rubydex files them as instance members of the lexical
  namespace.
- **A namespace alias** (`Gem::URI = Bundler::URI`, `Ripper = Prism::…`): retry the lookup through
  the `ConstantAlias` target, following chains up to 8 hops. This is a fallback only.
- **Compact paths** (`class A::B::C`) with no `b.rb` rely on the Zeitwerk declarations that
  `workspace/rails/` writes.
