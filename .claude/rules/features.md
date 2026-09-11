---
paths:
  - "src/workspace/features.rs"
  - "src/workspace/config.rs"
  - "src/analysis/synthesize.rs"
  - "src/analysis/types.rs"
  - "src/analysis/views.rs"
  - "src/workspace/rails/**"
  - "src/knowledge/**"
---

# What a project can turn off

## The eleven keys, and the two kinds they are

Eight turn off **a body of knowledge** — something ya-lsp brings to an answer, which a project
may decline because it does not apply. Three say **where this project keeps the trees the fence
is about**. They are not the same kind of setting and they are not in the same table.

| key | what it gates | seam |
| --- | --- | --- |
| `rails.enabled` | all six below at once | the registered-row filter + the rungs outside the pass |
| `rails.schema` | `db/*schema.rb`, `db/*structure.sql`, `table_name` | `Schemas` + `Renamed` |
| `rails.models` | associations, `enum`, `attribute`, `delegate`, the 17 tail macros, the query interface, a concern's class methods | `Models` + `Concerns` |
| `rails.routes` | `config/routes.rb` and the helper module | `Routes` |
| `rails.entrypoints` | mailers, jobs, Sidekiq workers | `Entrypoints` |
| *(none — `rails.enabled` alone)* | what the framework's own singletons return | `Framework` |
| `rails.views` | the view context's three halves and the view→renderer type rung | `views.rs`, `types::from_renderer` |
| `types.structs` | `Struct.new` and `Data.define` | `Structs` |
| `types.annotations` | a Sorbet `sig`, a YARD `@return` | `Annotated` |
| `trees.test` | `TEST_TREES` — **replaces** | `environment::Names` |
| `trees.test_support` | `TEST_SUPPORT` — **extends** | `environment::Names` |
| `trees.migration` | the `db`/`migrat` pair — **replaces**, as `parent/mark` | `environment::Names` |

Six of the eight are one registered row each, and `rails.models` is two: `rails::CONCERNS` is a
projection of its own because it is the one a **gem's `lib/`** may join, and it is the same body of
knowledge — a concern's class methods are `validates`, `scope`, `belongs_to` and `has_many`. There
is no ninth key because there is no ninth body of knowledge.

## It is about answers, not speed

"Turn Rails off to make it fast" is what a user will reach for and it is the **wrong reason**,
and any description written for these settings has to lead with that. The generator lists are
reference-filtered before anything is read: a project with no `belongs_to`, no `schema.rb` and no
`routes.rb` already puts nothing on five of the seven lists and opens no files.

What a non-Rails project really pays is narrower and real:

- **The projection walk** — 70 ms of a keystroke on discourse when the gate makes it run, and
  22 ms on lobsters. It was 275 and 52 before the walk started holding what each document
  contributed; `synthesize.rs` has the table.
- **The path conventions**, asked of filenames rather than of calls. `rails::controller_of` reads
  a path and `is_schema`/`is_structure` read a name, so **any** project with an `app/views/` or a
  file ending `schema.rb` has Rails applied to it whether or not it is Rails. A Sinatra or Hanami
  application with an `app/views/` gets a *Derived* card citing a controller that does not exist.
- **The view context**, which claims `app/helpers` for every bare word in a template.

## Where a switch is written, and where it must not be

- **A gate goes at the registered-row filter, never inside a `workspace/rails/` reader.** Every file of
  that directory is at 100:100 and `rails/mod.rs` states the property that makes it reviewable —
  *pure text in, text out, no I/O and no graph*. A gate inside one breaks that for a decision the
  caller already holds, and costs a test in a 100:100 file for nothing.
- **A module with no key is a module that ships nothing declinable.** `knowledge/rspec.rs` is
  `#[cfg(test)]` and its `wanted` is `list == GROUPS` with no flag at all, which is right: there is
  nothing for a project to decline. A key exists where a project may reasonably say *this body of
  knowledge does not apply to me*, and that is a judgement about the knowledge rather than a slot
  the registry hands out.
- **`contribution`'s loop is the cheap gate and the `retain` after the walk is the correct one**,
  and both are needed. The loop stops the per-document predicates running for a list nobody
  wants. The `retain` catches the one membership decided *after* the walk: a model that writes no
  macro at all joins `rails::MODELS` there without passing the per-document test, so a
  `models = false` that only filtered the rows would go on reading every model in the project. It
  was written with only the first and the test caught it.
- **A gem's `lib/` is inside the pass for exactly one row, and the switch reaches it.**
  `rails::CONCERNS` is gated on `features.models` like `rails::MODELS`, so a project that declines the
  model macros declines the class-side half with them — which is right, because they are the same
  macros seen from the other side. `Wants::gems` is what admits the documents and no other row sets
  it.
- **Three rungs live outside the pass and a switch that misses them lies.** `types::from_signature`
  and `types::from_block`'s two receiver-relative returns (`Return::Element` and
  `Return::Collection`, which are ActiveRecord's and nobody else's), `types::from_renderer`, and
  `views::Views::reachable`. Plus `mod.rs`'s `is_structure` watch arm. Each is gated on the
  specific flag, never on `features.rails`, because `Features::resolve` has already folded the
  umbrella into every one of them.
- **The inflector is never gated.** `rails::camelize` and `rails::element_of` are called by
  `types.rs`, which says why in its own words: the inflection is shared rather than copied.
  Singularising a directory name is not a Rails feature, and switching it off would move answers
  in projects that have nothing to do with this.
- **`Views` carries a flag rather than being left empty.** The two halves of that module fail
  differently when empty: `named_by` answers nothing, which is harmless, while the renderer half
  goes on asking `controller_of` of a path. `Views::default()` is therefore **off**. **And that
  flag is the type rung's gate too**: `types::renderer_documents` asks `Views::rendered_by` rather
  than reading `rails.views` a second time, so the switch is checked once, on the value the pass
  built, and the two cannot disagree about a project that said no.

## `auto`, and the coupling it must not go through

`rails.enabled` defaults to **`"auto"`**, not to `true`. The stated motivation is *if the project
is not a Rails app*, and a plain boolean makes the user go and find a setting to turn off a thing
they never asked for.

- `auto` answers yes when the root holds `config/application.rb` **or** `Gemfile.lock` locks
  `railties`. **Both halves are needed**: an engine has no `config/application.rb`, and a fresh
  clone has no `Gemfile.lock`.
- `railties` and not `rails`: the `rails` gem is a metapackage a project may leave out.
- **Detection reads `Gemfile.lock` itself and must never go through `Workspace::gems()`.**
  `gems::discover` returns early when `gems.enabled` is false and never reads the lockfile at all,
  so a user who turned gems off would silently also have turned Rails off.
- **Whichever way it lands, it says so once at `info`.** A detection nobody can read is a
  detection nobody can argue with, and `workspace/gems.rs`'s `ruby_lib` comment is the standing
  example of what that costs.
- **Only `auto` touches the filesystem.** A project that has already answered is not asked about,
  which keeps a `stat` and a `read_to_string` off every other configuration's path.

## The three fence keys

- **Replace for two and extend for one is the module's own asymmetry, made settable.**
  `environment.rs` says it outright: a name on the **target** list *deletes an answer* when it is
  wrong, so a key that appended to it would invite somebody to add `lib` and silently delete their
  whole workspace from `completion`, `definition` and `hover`. A name on the **cursor** list only
  turns the fence off, so being wrong costs nothing but the protection it was going to give — and
  that one takes the additive shape `gems.paths` has.
- **Replacing says what it replaced**, at `info`, from `index_workspace`. A log line and not a
  `messages::` sentence: setting these is legitimate, and a notification every session about a
  setting the user meant is a nag. `test_support` says nothing because it replaces nothing.
- **`trees.migration` is a pair and the key keeps it.** The rule is not "a directory called
  `migrate`" — that is an ordinary enough name for `app/services/migrate/` — it is a `migrat`
  **substring** in a directory whose parent is `db`, which is what makes `db/migrate`,
  `db/post_migrate` and `db/old_migrations` one rule. An entry with no separator is skipped and
  `config::validate` says so, because a bare `migrate` would fence a tree the project loads.
- **`[]` is a fence turned off and is different from unset.** It is a legitimate answer, and
  unlike `index.include = []` an empty fence list breaks nothing. That is why the resolved fields
  are `Option<Vec<String>>` and why `config.ts` sends an empty list rather than dropping it.
- **`[trees]` and not `[index]`.** These keys move what may be **answered**, never what is
  **read**: a fenced file is still indexed, and `references`, `rename` and `documentHighlight`
  still find every use in it, because a use under `spec/` is a use. Folding them into `[index]`
  would suggest the opposite and be the first step towards somebody implementing it.
- **The generator-template fence gets no key, and that is a decision.** A generator template is
  the same thing wherever it is shipped from — a gem-authoring convention, not a choice a project
  makes about its own layout — so nothing a user could write would make their tree more or less
  copied-rather-than-loaded. If a case turns up it is a fourth key and it says so then.

## What is not switchable, here or after here

**The advertised capabilities.** Nobody asked for a switch on a request method and this does not
invent one. What is configurable is the same kind of thing `[rbs] enabled` and `[gems] enabled`
already are — a body of knowledge the server brings to an answer. A request method is not that:
it is the wire contract, *fixed at `initialize` and never renegotiated*, which is why
`capabilities::server_capabilities` takes only the encoding. **Every switch here changes what an
answer is made of and none changes whether the question may be asked.**

## The cost, which is tests and is the discipline rather than an objection to it

Every key costs four edits a test *enforces* — the `Config` field, the `PartialConfig` field, a
`package.json` property and a `config.ts` branch, held together by
`tests/vscode_manifest.rs::every_setting_the_server_reads_is_one_the_editor_can_set` — plus a
covered default and `apply` arm in `workspace/config.rs`, which is at 100:100. That is the
argument against a switch per macro: `tail.rs` alone is 17 macro families, and thirty switches is
thirty of those and a settings page nobody reads.

- `workspace/features.rs` and `workspace/config.rs` are both floored at 100:100.
- `analysis/environment.rs` is at 100:100, so every arm of *configured versus default*, *replace
  versus extend* and *empty versus set* needs a test.
- **The two published defaults are read from the code, not written out.** The VS Code manifest
  documents `trees.test` and `trees.migration` as the built-in lists, and
  `analysis::{TEST_TREES, MIGRATION_PAIR}` are re-exported for `tests/vscode_manifest.rs` to check
  them against. A default written out in JSON with nothing holding it to the code is documentation
  that rots — which is how `ya-lsp.logLevel` shipped two releases saying `warn`.
- **The harness is a Rails project and says so through the client's layer.** `rails.enabled` is
  `auto`, and a `tempdir` with three files in it has neither marker — so every fixture that writes
  `has_many` would silently stop being about Rails. Writing a marker *file* instead would put
  `config/application.rb` in the index or a lockfile in front of gem discovery, and both move
  counts the tests assert on. Because it is the client's layer, a fixture that writes its own
  `ya-lsp.toml` still wins, which is what lets a test turn a family back off and mean it.
