---
paths:
  - "src/analysis/locator.rs"
  - "src/analysis/symbols.rs"
  - "src/analysis/hover.rs"
  - "src/analysis/render.rs"
  - "src/analysis/requires.rs"
  - "src/analysis/search.rs"
---

# Navigation, outlines and rendering

- **rubydex invents constant references you must not follow.** Every call with an implicit or
  constant receiver gets a fabricated reference to `<Foo>` so the singleton class can be
  resolved. `Person.new` therefore has two references over the same six bytes, and for an
  *implicit* receiver the invented one spans the **whole call** — so `alias_method :a, :b`
  carries a reference that resolves to `class << self`. Angle brackets are rubydex's only
  spelling for singleton names; `analysis::locator` drops references that start with `<`.
- **`Foo.new` is answered with `Foo#initialize`, and the redirect is marked.** `Class#new` is the
  exact answer and a useless one — measured on a Rails app, all 25 `.new` call sites hovered as
  `Class#new(*args, **kwargs, &block)` in `core/class.rbs`. `locator::constructor` steps off it
  through the *attached* class, which is the singleton's `owner_id` and not a name taken apart.
  It stands aside twice: a hand-written `def self.new` is the method the call actually reaches
  (rbs declares one for `Struct`), and `BasicObject#initialize` is what every object inherits
  rather than a constructor anyone wrote, so a class with neither keeps `Class#new`. Both guards
  ask the *owner* of what was found, which is independent of how rubydex spells a member.
- **`Resolution::redirected` exists because `references` must not take the redirect.**
  `def initialize` is not a declaration of `new`, and with `includeDeclaration` it was listed as
  one for every class whose constructor is in the user's own code. Navigation and
  `completion::precise_call` want the redirect — a constructor's keyword arguments really are
  what `Foo.new(` takes. Anything new reading `Resolution` has to decide which it is.
- **rubydex records an anonymous `*`, `**` or `&` under the sigil itself**, not under an empty
  name, so `render`'s `sigil` writes it once. Prepending unconditionally spelled `def f(*, **, &)`
  as `(**, ****, &&)`, which `.new` hovers made visible on every constructor that takes one.
- **`selectionRange` must sit inside `range`, and Prism's error recovery hands out pairs that do
  not.** `locator::spans` is the one place that reconciles them, for `DocumentSymbol` and
  `LocationLink` alike — both carry the rule, and VS Code enforces the first by *throwing*, which
  drops the entire outline rather than the one bad symbol. A bare `def` at the end of a line
  recovers into a node whose location is the three keyword bytes and whose name location is the
  whitespace after them; a 403-state sweep over five files being typed character by character
  found that shape and no other. Half-typed code is the normal state of a buffer, so never build
  the pair by hand from `offset()` and `name_offset()` again.
- **A definition with no name in it is not an outline entry**, which is the visible half of the
  same recovery — a blank row while `def` is being typed. `is_outline_worthy` checks it because
  `document_symbols` already builds every name; `locator::locate` deliberately does *not*, since
  a string lookup per definition would land on every hover and completion for a transient that
  only shows when the cursor is parked on the whitespace itself.
- **A definition matches only its name span, never its body.** `Definition::name_offset` is
  `None` for constants, `attr_*`, and aliases — for those, `offset()` is already just the name.
  Widening this to the body makes hover fire over whitespace.
- **Hover cards are pinned side by side, in one file, whole — and that is the only shape that
  catches the failure they have.** `every_hover_card_in_one_file_drawn_side_by_side` renders every
  construct in `GALLERY` with its card under it and asserts the lot. Before it existed each
  construct had a `contains` somewhere and no two cards were ever read next to each other, which
  is exactly how `class << Book` came to sit above a `private Shelf::Book#hide`: `attached_name`
  read the attached class out of rubydex's `Shelf::Book::<Book>`, where it is spelled
  unqualified, and the one test covering the construct used a *top-level* module, where the
  qualified and unqualified names are the same string. Take the part before the `::<`, never the
  part inside it. A new card, or a new construct, goes in that fixture — a card asserted on its
  own is blind to drifting away from the others, which is the whole of what a reader notices.
- **`render::signature_label` writes the label and the spans in one pass, and the offsets are
  UTF-16 code units.** `signatureHelp` highlights a parameter by handing the client a pair of
  offsets into the label, so the function that writes the string is the function that says where
  it wrote each piece — nothing downstream counts characters. LSP's other spelling, the parameter
  as a substring to search for, mis-highlights the moment a label holds the same token twice, and
  `def each(key, value = key)` already does. The unit is UTF-16 because that is what a client
  indexes the label by; the protocol ties `Position` to the negotiated encoding and says nothing
  about these, and `def приветствие(имя)` is legal Ruby, so the two counts genuinely differ.
- **Which definition of a declaration a *list* points at is `locator::preferred_definition`, and
  it is one decision.** A class reopened two hundred times has two hundred definitions and every
  list that mentions it once has to pick the same one, or the same class opens in `app/models` from
  the outline and in whichever gem reopened it from the type hierarchy. `locator::declared_in` is
  the other half — whether *any* of them is the user's, which is what both the symbol picker and
  the subtype list rank by. Both lived in `search.rs` until v0.3.0's item 4 needed them; neither
  belongs to the feature that happened to want it first.
- **How a construct is spelled for a human lives in `render`, and is shared.** `split_qualified`
  (symbol lists) and `qualified_name` (hover) both go through `singleton_parts`, and
  `symbols::kind_of` is shared with `search`. A symbol that reads differently in the outline and
  in the picker is a bug, not a style choice.

- **A hover card is the answer, then what ya-lsp knows about the answer.** Fence, rule, prose,
  and then footnotes — one italic line each, at the bottom. A footnote is for ya-lsp's confidence
  (the receiver's type was not known; the class is reopened elsewhere), never for a fact about
  the code, which belongs above. `hover::footnote` is the one place the shape lives, because it
  was two: a guessed single match carried the caveat as a trailing italic and a guessed *list*
  carried the same sentence inline, in bold, at the top, after an em dash. The four cards are
  pinned whole in `analysis::tests` — a core method, a stdlib method, a reopened class, a
  name-only match — the way `ANCESTRY` pins the first ten completions, because composition is
  the kind of thing that drifts while every part of it still passes a `contains`.

- **Nothing raw-HTML may reach a `MarkupContent`, and nothing inside code may be touched.** RDoc
  lifted Ruby's documentation out of the C source and left the HTML in — 1,867 `<code>` spans in
  the vendored signatures, plus `<em>`, `<strong>`, `<tt>`, `<b>`, `<i>` — and a client renders a
  hover as markdown, which strips them: `<code><=></code>` arrived as a bare `<=>`, and prose
  that merely looks like a tag (`<vowel>`, `<rhs>`, `<main>`, a whole `<html>` example) vanished
  with everything up to the next `>`. `render::to_markdown` converts what is markup and escapes
  what is not, and RDoc's `rdoc-ref:` links — 912 in `core/` alone, every one pointing into a
  tree the editor has never seen — keep their words and lose the link. It skips fenced blocks,
  four-space and tab-indented blocks, and backtick spans: what is in them is Ruby, and a
  backslash in front of `Hash<Symbol, untyped>` is a visible bug where the old behaviour was a
  silent one.
