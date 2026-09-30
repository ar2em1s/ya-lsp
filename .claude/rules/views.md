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
- **The rung is stated once and read three times.** `locator::resolve_typed` takes the first
  answer, `completion::in_view` collects them all, and `types::returned_by` types a receiverless
  call through it, before `self`'s own lookup, so a margin and a card read one rule. The gate
  lives only in `views.rs`, so a jump, a list and a label cannot disagree.

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
- **A mailer's `default template_path:` moves its views** (`Views::moved`, read into
  `Entrypoints::template_paths`). The nearest class up a mailer's ancestors that wrote one decides:
  `default` merges into a class attribute each subclass inherits.
  - A written string is the writer's directory, and the writer stands for every mailer below it.
  - A lambda or `proc` whose body is one string around the mailer's own name
    (`mailer.class.name.underscore`, `self.class.name.underscore`, `mailer_name`) is read backwards
    (`rails::mailer_in`): `mailers/notify_mailer/` is `NotifyMailer`.
  - Anything else only refuses the default directory, which a mailer under any setting never gets.
  - `self.mailer_name =` is not read.
- **A mailer gets only the helpers it names** (`helper :accounts`, `helper Admin::OrdersHelper`),
  never every application helper. `ActionMailer::Base` has no `include_all_helpers`. Skip what can't
  be named (`routes.url_helpers`).
- **A module under `app/helpers` is itself in the view context.** It reaches sibling helpers and
  ActionView, but not exports, because no single controller applies. `Views::reachable` answers for
  helper files and templates only.
- **A jbuilder view is a template here** (`views::is_view`, `rails::is_jbuilder`):
  `app/views/stories/show.json.jbuilder` gets its controller's variables and the view context. It
  is no class an ERB partial, a helper or a layout runs on (`types::every_renderer` counts ERB
  views only: a JSON view renders none of them), and a jbuilder partial's renderers come from its
  calls. It stays indexed whole, as Ruby (`erb.md`): only blanking and
  markup questions (folding, completion's markup test, refactorings) stay ERB's. `json` is a
  `JbuilderTemplate` where the bundle declares it (`rails::JBUILDER`, `types::partial_local`), and
  its `partial!`, `array!` and any key written with `partial:` are render calls
  (`Render::json`) that find jbuilder partials alone, as an ERB view's find ERB ones.

## Layouts

- **A layout's path names no class** (`layouts/application` spells a `LayoutsController` nobody
  writes). `Views::lays_out` answers for a template in a directory a layout name is looked up in
  (`rails::is_layout`), and only where `rendered_by` names no class.
- **Its renderers are every class whose views it wraps**, by ActionView's own lookup
  (`rails::layouts_of`, `Views::layouts_of`):
  1. the nearest `layout` on the class or an ancestor (`Model::layouts`, keyed by the body, like
     the exports; a concern's `included do` counts);
  2. with none, or `layout nil`: `layouts/<controller_path>` where that template exists, else the
     parent's answer;
  3. `only:`/`except:` add rule 2's answer; a symbol or a lambda may be any layout.
- **The candidates are `types::every_renderer`**, and the table is held with it
  (`Indexed::layouts`). `types::view_context` is the one door that hands `reachable` a layout's
  renderers.
- **The view context unions them**: each renderer's exports (one name is one row), the
  `app/helpers` glob when any is a controller, and the helpers each named. The derivation names
  the renderer that reached the export (`InView::Laid`).
- **A render call joins only where it writes the layout's name** (`Render::writes_the_name`): a
  nested layout's `render template: "layouts/application"`. A `render options` names any template,
  and would put every class that writes one in every layout (a mailer layout read the
  controllers' `@user`).
- **Bounds:** a layout template ya-lsp does not index (HAML, Slim) is invisible, so a class whose own
  layout is one falls through to its parent's, a wider answer. A per-action `render layout:` is not
  read. A mailer whose views the path convention cannot find renders no view, so it is in no
  layout.

## Controller callbacks

- **`Views` also carries what each body runs before a controller's actions** (`Views::callbacks`,
  from `Model::callbacks`, and every document's unattributed calls, `Model::loose_callbacks`), read
  whatever `[rails] views` says: a controller runs its callbacks either way (`Views::with_callbacks`).
- **`Views::runs_before(graph, object, action)`** walks the object's linearized ancestors (a
  partial ancestry answers nothing: an unread link may skip) and asks `rails::runs_before`.
  `types::written_before` is the one reader.
- **The reader is `workspace/rails/callbacks.rs`**: a `before_action` family call counts with no
  `if:`/`unless:` and literal `only:`/`except:`; a skip (`skip_before_action`, a `:process_action`
  `skip_callback`/`reset_callbacks`) anywhere on the chain applies unless its lists leave the
  action out; a call under `with_options`, a condition or in a `def` is `loose`: its skip applies
  everywhere, its callback nowhere. Any `*_action` naming a method makes that method no action.

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
