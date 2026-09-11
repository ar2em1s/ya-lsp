---
paths:
  - "src/analysis/views.rs"
  - "src/workspace/rails/conventions.rs"
---

# What a template can call

- **The gap is a *receiver*, not a declaration, which is why this is not a generator.** Every other
  answer starts from something the file writes down: a receiver, a constant, a `def`.
  `<%= time_ago(story) %>` writes none. Rails builds the view context at render time from two things
  no file names — every module under `app/helpers`, and the `helper_method` proxies of the
  controller the template's *path* implies — so rubydex resolves it to nothing.
- **Generating a declaration is impossible, not merely worse.** A generated `module` holding the
  view context cannot be reached: RBS cannot say *self in this file is X*. The obvious repair —
  making a template's top level genuinely inside a `class … end` — is closed by construction,
  because `erb::ruby_view` replaces markup with **one space per byte** so every offset is the
  template's own, leaving no room to write anything. **The property that makes templates indexable
  is what forbids the easy road here.** Third reason: declaring the helpers half in RBS would give
  every helper method in the corpus a second *place* on its card.
- **So it is a rung, stated once and read twice.** `locator::resolve_typed` takes the first answer,
  `completion::in_view` collects all of them — `locator::extended_modules`' seam reached by a
  second road. The gate deciding what the view context *is* lives in `views.rs` and nowhere else,
  so a jump and a list cannot disagree.
- **The two halves an application writes are not the same size.** `app/helpers` reaches an order of magnitude more
  template call sites than `helper_method` does, and a third of the export half names a method that
  is *also* an `app/helpers` `def`, so its marginal reach is smaller again. A rough survey badly
  understated the gap; the counted truth is far more lopsided. If this is ever cut down, that is
  where the line goes.
- **`rails::is_helper` is Rails' own glob and both halves are load-bearing.** `all_helpers_from_path`
  globs `**/*_helper.rb` under each `app/helpers`, so a file under `app/helpers` not named that way
  is in **no** view context by default — one corpus writes several, under a nested
  `app/helpers/.../controller_helpers/`, reached by an `include` a controller writes. The
  `app/helpers` anchor keeps `spec/helpers`, which every corpus has, off the list.
- **The export half is keyed by the body that wrote the macro, never by the controller it reaches**,
  and the ancestor walk makes three of its four hosts free. `helper_method` is written in a
  controller (most of them), a concern `module`, a module under `app/helpers` a controller
  `include`s, and a **mailer**. A reader walking `app/controllers` and keying by class would miss
  most of one corpus' exports and nearly all of its sites. rubydex's linearized ancestors answer all
  four with no case for any.
- **`helper_method` hands over a permission, not a member.** The `def` it names is already in the
  graph on the class the macro is written in — `AbstractController::Helpers` writes a proxy onto
  `_helpers`, and a second copy of a `def` rubydex has would be a second *place* on the card. So
  `Model::exports` records names, `Reachable::member` tests membership **before** the ancestor
  lookup, and a controller method nobody exported is not reachable from a template. That per-name
  gate is the whole of this half's safety.
- **The export wins where both halves hold a name, and it is Ruby rather than a preference.**
  `helper_method` defines its proxy *on* `_helpers`; `helper` includes a module *into* it, so the
  proxy runs. A third of all export sites are that shape — the commonest meeting of the two halves,
  not an edge. `Reached::step` carries the same order into completion ranking: export 0, helper module 1.
- **A mailer gets what it asked for by name, not the glob, and both clauses are needed.**
  `include_all_helpers` is `ActionController::Base`'s default and `ActionMailer::Base` has no such
  thing, so a mailer template reaching every application helper would be an answer Rails does not
  give. But **most `helper` calls the corpora write under `app/` are in a mailer**, and refusing to
  read them left the corpus that writes the most of them answering almost nowhere. `helper :accounts`
  is `inflect::helper_module`'s camelize-and-append; `helper Admin::OrdersHelper` is the constant;
  `helper Rails.application.routes.url_helpers` (two corpora write it) is a module this reader cannot
  name and skips *beside* names it can.
- **`mailer_of` must be gated by its caller; `controller_of` must not.** A directory named `stories`
  can only produce `StoriesController`, a name nothing but a controller has; the mailer spelling
  produces whatever the directory spells, and `app/views/shared/` spells `Shared`. `Views::mailers`
  is the gate — `rails::is_mailer` asked of `Context::superclasses`, the same superclass table the
  mailer and job reader uses.
- **`Views::rendered_by` is the one place the two are ordered, and `types` reads it as well.** The
  controller first and the mailer only where there is no controller, which is Rails' own order: an
  application that really writes a `UserMailerController` has said where that template renders
  from. The same question decides three different answers — what a template may *call* here, what
  a card says its `@ivar` *is*, and where `definition` *jumps* — so it is asked once and answered
  in `RenderedBy`. Two modules resolving one path to two classes is not a state this can reach.
  The `controller` flag travels with the name for the two readers that need it: the `app/helpers`
  glob is a controller's and not a mailer's, and the card's footnote has to say *mailer* rather
  than call one a controller.
- **The third half is ActionView's own, and it is two names rather than a reader.** The view
  context is the two halves an application writes *plus* every helper module ActionView ships, and
  the last is `rails::VIEW_CONTEXT` — `ActionView::Helpers` and `ERB::Util`, which is what
  `action_view/base.rb` writes as `include Helpers, ::ERB::Util, Context`. Nothing is generated
  and nothing is declared: actionview is in the bundle and therefore already in the graph, so all
  the table supplies is a **root to walk ancestors from**, and rubydex's linearization reaches the
  24 helper modules `ActionView::Helpers` includes at module-body level — an alias like `t`
  included. A project whose bundle has no actionview has no such declaration, which is the whole
  of the gate: there is no switch, because a name the graph does not hold cannot be walked.
- **The rung is additive and must never be exclusive.** An application writing `def tag` in
  `ApplicationHelper` shadows `ActionView::Helpers::TagHelper#tag`, so its own is right there, and
  the framework's half is read **last** for exactly that reason — the order is Ruby's own, since
  the proxy sits on `_helpers`, the application's module is included into it, and ActionView's
  were included into the view class before either. A name in none of the three halves still falls
  to the name rung: after the third half shipped, 8,325 of the six corpora's 8,732 bare-word call
  sites answer exactly and **407 are that residue** — `can?` 151, `defined?` 35, `confirm` 24,
  `policy` 21, and a long tail of one project's own words.
- **A module under `app/helpers` is *in* the view context, not merely read by it.** Rails includes
  every one of them into the same `_helpers`, so a bare call written in one reaches the sibling
  helper modules and ActionView's own exactly as a template's does — and it is the only other
  place in an application where that is true, which is why `Views::reachable` answers for a helper
  file and for nothing else that is not a template. What a helper file does **not** get is the
  export half: `helper_method` is a permission one controller grants and a helper module is
  included into every controller's context, so there is no class to name and none is picked. 296
  of the 4,697 positions this half moved are in that lane. **The bound it inherits is the
  template lane's own**: `rails::is_helper` is asked of the path under the cursor, so a helper
  file inside an indexed *engine* gets the application's helper modules in scope exactly as an
  engine's template already did. It is stated rather than fenced because the population is six
  files across six corpora — 0 in lobsters and discourse, 1 each in mastodon, chatwoot and forem,
  3 in solidus — and a fence for it would be the first time this module read `environment`.
- **Declined, and the measurement that declined each.** Over 8,732 bare-word call sites in six
  corpora's templates and `app/helpers` files, asked of the graph one module at a time against a
  control class that includes nothing: `ActionView::Helpers` answers 27 words no other rung did,
  `ERB::Util` 2 (`h`, `json_escape`, 38 sites), and **`ActionView::Context` 0** — it is the
  renderer's own plumbing, `output_buffer` and `view_flow`, which no template calls bare.
  **`ActionView::Base` itself is declined too, and not for lack of reach**: it answers the same 29
  words, being these two plus `Context`. What it would add is `Object` and `Kernel`, whose members
  would then arrive through the view rung rather than the name rung at every template in the
  project — a far wider displacement bought for nothing.
- **Swept with one binary per side, nothing else different: 4,697 of 8,732 positions moved from a
  candidate list to one exact answer, 0 moved the other way, 0 went silent, 0 lost a place.** By
  corpus: forem 3,144, solidus 993, mastodon 212, discourse 178, chatwoot 88, lobsters 82. `t` and
  `translate` are 3,966 of it — the translation helper really is the call a Rails template makes
  more than any other — then `link_to` 181, `raw` 86, `content_tag` 85, `render` 51. Every new
  answer names an ActionView module or, in three cases, the sibling helper the widened helper lane
  reaches; `h` lands on `ActiveSupport::CoreExt::ERBUtilPrivate`, which is the method that
  actually runs.
- **Completion was measured apart, because the audit sees none of it, and it is where the cost
  is.** Over 297 real template lists the rows a template is offered went 266 to 285 on average,
  with **0 lists shrinking** — but 37 of the 297 were already at the 512-item ceiling, and 312
  rows were pushed past it, 79 of them route-helper labels. That is the cap and the ranking
  behaving as specified rather than a new rule, and the obvious repair is **declined by
  measurement**: filtering the view context's rows to public methods lost **932** rows across
  135 lists instead of 312 across 37, because ActionView marks 185 of its 438 helper `def`s
  private *and the corpora's own helper modules mark 192*, every one of which was already being
  offered. `completion`'s `private_ok` is the rule — a cursor with no receiver written may call a
  private method, because Ruby lets it — and this half does not get an exception to it.
- **In completion the view context *shadows* what `Object` offers, and that is Ruby too.** A helper
  module is `include`d into the view class and `Kernel` is at the end of every chain, so an
  application writing `def format` in `ApplicationHelper` really has replaced `Kernel#format` for
  every template. `add_view` takes the graph's row away rather than adding a duplicate, which would
  complete the word and then jump to the wrong method.
- **Two bounds are stated rather than discovered.** `controller_of` reads the directory, not the
  file name, so `shared/_header.html.erb` names a `SharedController` nothing defines — under half
  the corpus' partials sit under a directory naming a real controller, and the rest get the helpers
  half only, which is most of what this rung leaves unmoved. And an application setting
  `include_all_helpers = false` gets only its matching helper; that config is out of reach for the
  same reason `database.yml` is, and none of the six corpora sets it.
- **Swept over five applications: nearly every template site is better, none worse, none
  lateral.** Candidate lists collapse to one answer and none gets longer; completion offers the word
  the file itself wrote almost everywhere it previously offered nothing. `sweep.tier` puts `list`
  and `name` at one rank, so the navigation half was expected to read near zero; it does not,
  because the answer lands on `derived` — a convention that names a class is a derivation with a
  footnote. `sweep.tier` had to be taught that footnote first; it read "Reached through" as
  *resolved*, which would have handed these positions an unearned top tier.
- **The residue is small and every part of it is named.** Some positions were already `resolved`;
  some stay on the name rung — mostly partials under `shared/` or a vendored view path no path
  convention can reach; some stay a list; and a handful **answer nothing**, which is the harness
  over-collecting inside a `%i[]`, a `%r()` and a `#` comment in a tag. (More answered nothing until
  the template coordinate conversion was fixed — `erb.md`.)
