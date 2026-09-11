---
paths:
  - "src/analysis/hints.rs"
---

# Inlay hints

- **The resolved tier is unreachable here, by construction, and that is the fact the module rests
  on.** A hint exists exactly where the code does *not* say the type; where it does — `x = 1`,
  `x = Foo.new`, `x = Foo` — `worth_saying` refuses it as noise. Those are precisely the bindings
  whose type is resolved, so the tier that needs no footnote and the shapes that need no hint are
  one set seen from two sides. Every hint drawn is therefore derived, every one carries a tooltip,
  and the labels carry **no marker**: there is no second kind on screen to tell it apart from.
  Do not add one back without first finding a resolved hint.
- **The bottom tier is refused by a test on the tier, never by a list of shapes.** `Tier::Guessed`
  is dropped in one `retain`, after every family has had its say. A name-matched type painted into
  the margin of every method is the failure this project is organised against, and a rule written
  as "not these three shapes" is a rule the next rung below the graph walks straight through.
  `a_type_matched_on_a_name_alone_is_never_drawn_in_the_margin` asks the same expression twice —
  once as a card, which names the guess, and once as a label, which is not there.
- **A template has a margin too, and the view↔renderer rung is what fills it.** `inlayHint` is
  one of the requests ERB needs nothing extra for — `foldingRange`, `codeAction` and `completion`
  are the three that are gated, and this is not one of them — so whatever types a template's
  `@ivar` decides what is painted beside it. That is why widening that rung from the controller a
  directory names to a *mailer's* views was measured before it shipped rather than after: over
  the six corpora it moved the card from **759 to 788** of the 1,301 template reads drawn, and a
  label is built from the same answer a card is. What moved is the population; the tier did not.
  Every one of those is a convention, therefore *Derived*, therefore already drawable, and the
  refusal above is untouched — `a_mailers_template_is_a_margin_the_renderer_rung_reaches` draws a
  label off the mailer's own `@story` and refuses one off a `@person` nothing but its name types,
  in the same template and from the same request.
- **The range bounds the work, not the answer, and the assertion for that is a count.**
  `cursor::bindings_in` takes it, so a binding outside the editor's window is never classified
  rather than classified and dropped. Nothing in the *answer* can show that — a version that
  classified the file and filtered afterwards returns the same list — so `cursor::CLASSIFIED` is a
  `#[cfg(test)]` counter of the candidates whose shape was worked out, and
  `hints_answer_for_the_range_they_were_asked_about` asks for two bindings' worth of window and
  requires that two were classified. **The clock was tried first and was wrong**: a window costs
  ~13 ms of the whole document's ~67 on a quiet machine, but ~12 of that 13 is the whole-buffer
  parse every request pays, so under load the window inflates faster than the file and the ratio
  between them collapses — one run in six on an idle laptop, for a reason that has nothing to do
  with hints. A count is the same integer on any runner.
- **A scope question is asked once per binding, so the walk is hoisted out of the loop.**
  `types::Scope::at` reads every definition in the document, which is right for the one cursor
  every other caller has and quadratic for this one: it was **325 of the first draft's 352 ms**.
  `Scope::bodies` reads them once and `Bodies::at` places a point against that. Points only — a
  point cannot straddle a body's edge, which is why it can be a plain innermost lookup and
  `Scope::covering`'s refusal has nothing to do here.
- **And the walk is handed down, because `method_receiver` grew a second point to place.** A
  captured `self` is resolved at the offset it was *written* at rather than at the cursor's — see
  `types.md` — which is a `Scope::at` inside the loop, and the loop is this one. So `hints` puts
  its own `Bodies` on the `Sources` it passes, and `Sources::scope_at` is the only reader; a
  request with one cursor leaves the field `None` and pays the walk once. `Bodies` carries the
  `UriId` it walked, so a rung that recurses into another document falls back rather than testing
  containment against spans that are not its own.
- **A return hint is always derived, and never drawn where the file itself declares it.** A
  signature is a *claim* — nothing checks it unless a type checker is run, and where it is core
  RBS being overridden by the `def` under the label it is a claim about a method that is no longer
  there. And "RBS declares it and the source does not" is the family's whole definition, so a
  Sorbet `sig` or a YARD `@return` two lines up disqualifies it: the test is whether any of the
  method's definitions live in `synthesized::generated_uri` **of this document**, which is a pure
  function of its URI.
- **Only a class or a module is a label.** A singleton class is the type of a class *object*,
  which Ruby cannot spell, and a `Namespace::Todo` is a name the workspace references and nothing
  defines. `Return::Same`, `Return::Element` and `Return::Collection` are refused for the same
  reason one layer up — which is what keeps `types::ELEMENT`'s promise that its made-up name never
  reaches a reader, now that something does print a return type.
- **The margin is redrawn when the background index finishes, and that is the only push.** It is
  the one answer here that goes stale without the document changing: a client re-asks on an edit
  and on a scroll, and neither happens while somebody waits for a cold index.
  `workspace/inlayHint/refresh` is a server-initiated *request*, sent the way
  `client/registerCapability` is and gated on `ClientSupport::hint_refresh`. Where the client says
  no, the hints are right from the next keystroke — which is what they were before this existed.
- **`inlayHint/resolve` buys a cheap tooltip rather than a rare one.** Every hint ships `data` and
  no tooltip; the sentence is built for the one somebody pointed at instead of for every line on
  screen, every scroll. It is `hover::provenance`'s own sentence, shared rather than copied,
  because the card and the margin are never on screen together — two wordings for one answer would
  be a difference nobody could ever see.
