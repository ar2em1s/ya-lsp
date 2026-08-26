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
