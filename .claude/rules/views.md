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
  `completion::in_view` collects all of them — `locator::extended_class_methods`' seam reached by a
  second road. The gate deciding what the view context *is* lives in `views.rs` and nowhere else,
  so a jump and a list cannot disagree.
- **The two halves are not the same size.** `app/helpers` reaches an order of magnitude more
  template call sites than `helper_method` does, and a third of the export half names a method that
  is *also* an `app/helpers` `def`, so its marginal reach is smaller again. A rough survey badly
  understated the gap; the counted truth is far more lopsided. If this item is ever cut down, that
  is where the line goes.
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
- **The rung is additive and must never be exclusive.** The view context is the two halves *plus*
  every helper module ActionView ships, which this crate does not model. An application writing
  `def tag` in `ApplicationHelper` shadows `ActionView::Helpers::TagHelper#tag`, so its own is
  right there — and a template calling `link_to`, which it does not redefine, must go on answering
  what it always did. `Reachable::member` returning `None` reaches the same name rung as before.
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
  *resolved*, which would have handed the item an unearned top tier.
- **The residue is small and every part of it is named.** Some positions were already `resolved`;
  some stay on the name rung — mostly partials under `shared/` or a vendored view path no path
  convention can reach; some stay a list; and a handful **answer nothing**, which is the harness
  over-collecting inside a `%i[]`, a `%r()` and a `#` comment in a tag. (More answered nothing until
  the template coordinate conversion was fixed — `erb.md`.)
