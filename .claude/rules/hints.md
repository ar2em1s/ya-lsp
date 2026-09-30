---
paths:
  - "src/analysis/hints.rs"
---

# Inlay hints

## Must

1. **Never draw a guess.** Drop `Tier::Guessed` in one `retain` on the tier, after every family has
   answered. Never refuse by listing shapes. Held by
   `a_type_matched_on_a_name_alone_is_never_drawn_in_the_margin`.
2. **Labels carry no tier marker, and no tooltip.** A hint appears only where the code does *not*
   state the type (`worth_saying` refuses `x = 1`, `x = Foo.new`). Those are exactly the resolved
   bindings, so every hint drawn is *Derived*, which no surface tells from *Resolved*
   (decided 2026-09-29). No tooltip says how the type was found, so there is no `inlayHint/resolve`
   and a hint ships no `data`.
3. **Label only a class, a module, or a class object.** A union over a concern's including classes
   (`Derivation::each`) wider than `MAX_INCLUDERS_SPELLED` (4) is not drawn (`hints::readable`);
   any other union keeps its width. A class object is spelled `Foo:class`
   (`render::typed`, decided 2026-09-24); a module object, a singleton's singleton and a
   `Namespace::Todo` are never drawn, and neither are `Return::Same`, `Element` or `Collection`, so
   that `generated::ELEMENT`'s made-up name never shows.
4. **Wrap a destructured target in `Receiver::Destructured`**, after the `Receiver::Unknown`
   refusal. Without the wrap, `a, b, c = prepare(...)` draws the whole call's type on every name.
   `worth_saying` recurses through it.
5. **Walk the scope once per request, not once per binding.** `Scope::bodies` reads the document
   once, and `Bodies::at` places each point. `hints` passes its `Bodies` on `Sources`, and
   `Sources::scope_at` reads them. `Bodies` carries its `UriId`, so a rung that crosses into another
   document falls back.

## Return hints

- **Always *Derived*, and never drawn where the file itself declares the return** (a Sorbet `sig`, a
  YARD `@return`). The test is whether one of the method's definitions lives in
  `synthesized::generated_uri` of this document.
- **What a `def` returns is `types::method_return`**, the card's question too: a signature, else the
  body, and the disputed union a chain reads (`types.md`).
- **`!` after the type where this `def`'s own code writes `raise` or `fail`** (`cursor::raises_in`,
  `render::returned`; `types.md`). A binding's label never carries it.
- **`-> bot` where every path through the `def` raises** (`types::never_returns`, `Because::Raises`):
  syntax only, and only where no signature types it.
- **No label on `initialize` or on setters** (`value_is_discarded`). Ruby discards both values:
  `Foo.new` returns the object, and `x.title = v` evaluates to `v`.
  - A name counts as a setter when it ends in `=` and the byte before is not one of `=<>!`.
  - This rule belongs to the margin only. `types::body_return` still answers for hover and for
    chains.
  - The setter rule is permanent. The honest label would be the parameter's type, which nothing
    declares.

## Range and refresh

- **One parse of the document per request** (`cursor::margin`): the bindings, the `def`s'
  label places and, unless `HeldExits` holds them for this text, the `Shapes` the type side reads
  next (`HeldExits::keep`). `hints::tests::a_hint_request_parses_its_document_once` counts parses
  (`cursor::parses_taken`): one cold, one warm.
- **The range limits the work, not just the answer.** `cursor::margin` never classifies bindings
  outside the window. The test counts (`cursor::CLASSIFIED`,
  `hints_answer_for_the_range_they_were_asked_about`) instead of timing, because timing flakes.
- **Push a refresh once, when the background index finishes** (`workspace/inlayHint/refresh`,
  gated on `ClientSupport::hint_refresh`). Otherwise the next keystroke fixes it.

## Templates

- **ERB needs nothing extra for `inlayHint`.** Whatever types a template's `@ivar` fills the margin,
  including the mailer-view rung. Held by `a_mailers_template_is_a_margin_the_renderer_rung_reaches`.
