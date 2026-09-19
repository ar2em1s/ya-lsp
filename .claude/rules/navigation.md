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

1. **Instance variables go to the scope walk first** (`locator::variable_at`, then `resolve_variable`).
   The graph answers `@name = 1` wrongly: it has only the write, and none of the reads. `definition`,
   `hover` and `documentHighlight` all ask in this order. `hover` alone falls through to the graph
   when nothing typed the variable.
2. **Then the graph**, through `locator::resolve_typed`, which `hover` and `definition` share. It
   has three tiers (`types.md`).
3. **A macro's `:symbol` last** (`cursor::macro_symbol`, `locator::resolve_symbol`).
   - A macro is any receiverless call written straight into a class body. Look the name up in the
     enclosing class's ancestors: the instance side first, then the class object.
   - **Positional arguments only.** Skip `dependent:`, `on:`, `only:`. A name no ancestor declares
     answers nothing.
   - **No macro table in `analysis/`.** That would put a Rails word outside `workspace/rails/`.
4. **A local answers `definition` with nothing** (you can see the line). This is pinned as a gap in
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

1. **The layout is the answer, then footnotes**: fence, rule, prose, then one italic footnote per
   line. `hover::footnote` is the only place that shape lives.
2. **A footnote is about ya-lsp's confidence, never about the code.** There are three kinds: none,
   derived-from-X, and guessed.
3. **"No type" and "no such member" are separate footnotes.** `Resolution::missed` carries the
   class, and `hover::why_guessed` picks the sentence. The tier is still *Guessed* either way.
4. **The class a footnote names must be openable.** Refuse `Namespace::Todo` and anything
   `render::is_nameable` rejects. A singleton is spelled through `render::class_object_of`.
5. **Cards are pinned whole and side by side** (`every_hover_card_in_one_file_drawn_side_by_side`,
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
5. **`Missed` distinguishes "no such method" from "exists but private here".**

- **`locator::Modifiers` repairs rubydex's visibility record.** A bare `private` inside a block is
  applied to every `def` below it. The rule is the one that never refuses: a modifier governs only
  the body it is written in, and a block body counts as a body. It only narrows refusals.
  `completion::reachable` reads the same repair.
- **`signatureHelp` is gated** (`Privacy::written(call.allows_private)`). Keyword completion, outgoing
  calls, `references`, `rename` and the hierarchy prepares are not.

## The name-based fallback

- **At a class-object receiver, drop candidates owned by a `class`**
  (`reachable_on_a_class_object`). The singleton walk already covered them. Module-owned candidates
  stay. The list is narrowed, never emptied.
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
- **A root member whose every `def` sits inside a block is not a member** (`locator::Blocks`,
  `declared_on_the_root`).
  - A block body has no namespace in rubydex, so `class_eval do def x` lands on `Object`.
  - One `def` written as its own body keeps the whole declaration.
  - The check applies on three roads: the root arm, `constructor`, and the typed rung.
  - Withdraw the claim, not the `def`: it stays in `workspace/symbol` and in the name list.
- **Nothing else precise is filtered.** `references` is never filtered.

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
  - `missed` spells the name too: a known-but-unnameable type is not an unknown one.
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

- **`def Foo.bar` and `def Foo::bar` name their receiver.** `locator::declaration_of` answers them
  outright, not as a fallback, because rubydex files them as instance members of the lexical
  namespace.
- **A namespace alias** (`Gem::URI = Bundler::URI`, `Ripper = Prism::…`): retry the lookup through
  the `ConstantAlias` target, following chains up to 8 hops. This is a fallback only.
- **Compact paths** (`class A::B::C`) with no `b.rb` rely on the Zeitwerk declarations that
  `workspace/rails/` writes.
