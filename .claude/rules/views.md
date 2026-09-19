---
paths:
  - "src/analysis/views.rs"
  - "src/workspace/rails/conventions.rs"
---

# What a template can call

`<%= time_ago(story) %>` names no receiver. Rails builds the view context at render time, so
rubydex resolves the call to nothing. `views.rs` supplies it as a **receiver rung**.

## Why this is a rung and not a generator

- **RBS cannot say "self in this file is X"**, and blanking leaves no room to wrap a template in
  `class … end` (`erb.md`).
- **Declaring helpers in RBS would give every helper a second place** on its hover card.
- **The rung is stated once and read twice.** `locator::resolve_typed` takes the first answer, and
  `completion::in_view` collects them all. The gate lives only in `views.rs`, so a jump and a list
  cannot disagree.

## The view context, in lookup order

1. **Exports: `helper_method`.**
   - Key each export by the body that wrote the macro: a controller, a concern, a helper module or a
     mailer. rubydex's linearized ancestors reach all four.
   - It grants permission, not a member. `Model::exports` records names, and `Reachable::member`
     checks the name before the ancestor lookup. An unexported controller method is unreachable.
   - It beats a helper module that has the same name, because the proxy on `_helpers` runs first.
     `Reached::step` ranks it 0.
2. **Application helpers: `app/helpers/**/*_helper.rb`** (`rails::is_helper`, Rails' own glob).
   The `app/helpers` anchor keeps `spec/helpers` out.
3. **ActionView: `rails::VIEW_CONTEXT`** (`ActionView::Helpers`, `ERB::Util`).
   - Nothing is generated. It is a root to walk ancestors from.
   - No actionview in the bundle means no declaration, which is the whole gate.
   - Read it last, so an application's `def tag` shadows `TagHelper#tag`.
4. **Anything else falls to the name rung.** The rung only adds; it never excludes.

## Which class renders a template

- **`Views::rendered_by` decides it, and `types` reads the same answer (`RenderedBy`).**
  - The controller comes first. The mailer counts only where no controller exists, which is Rails'
    own order.
  - It decides three things at once: what the template may call, what its `@ivar` is, and where
    `definition` jumps.
- **Always gate `mailer_of`, never `controller_of`.** `app/views/shared/` spells `Shared`. The gate
  is `Views::mailers`, filled by `rails::is_mailer` from the superclass table.
- **A mailer gets only the helpers it names** (`helper :accounts`, `helper Admin::OrdersHelper`),
  never every application helper. `ActionMailer::Base` has no `include_all_helpers`. Skip what can't
  be named (`routes.url_helpers`).
- **A module under `app/helpers` is itself in the view context.** It reaches sibling helpers and
  ActionView, but not exports, because no single controller applies. `Views::reachable` answers for
  helper files and templates only.

## Completion

- **The view context shadows `Object`.** `add_view` replaces the graph's row instead of adding a
  duplicate, because `def format` in a helper really does replace `Kernel#format`.
- **Keep private helpers.** `private_ok` allows receiverless calls to private methods. Filtering
  them was measured to lose far more real rows than the cap costs.

## Known bounds, stated rather than fenced

- **`controller_of` reads the directory**, so `shared/_header.html.erb` gets the helpers half only.
- **`include_all_helpers = false` is not read.** It is config we cannot run, and no corpus sets it.
- **Declined roots:** `ActionView::Context` (plumbing, no bare calls) and `ActionView::Base` (it would
  drag `Object` and `Kernel` through the view rung).
