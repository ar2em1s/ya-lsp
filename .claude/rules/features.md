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

## The keys

Eight keys turn off a **body of knowledge**. Three say **where the project keeps its test and
migration trees**.

| Key | Gates | Seam |
| --- | --- | --- |
| `rails.enabled` | all the rails keys below, plus the framework singletons and file-move renames | registered-row filter |
| `rails.schema` | `db/*schema.rb`, `db/*structure.sql`, `table_name` | `Schemas` + `Renamed` |
| `rails.models` | associations, `enum`, `attribute`, `delegate`, the 17 tail macros, the query interface, concern class methods | `Models` + `Concerns` |
| `rails.routes` | `config/routes.rb` and the helper module | `Routes` |
| `rails.entrypoints` | mailers, jobs, Sidekiq workers | `Entrypoints` |
| `rails.views` | the view context and the view→renderer type rung | `views.rs`, `types::from_renderer` |
| `types.structs` | `Struct.new`, `Data.define` | `Structs` |
| `types.annotations` | a Sorbet `sig`, a YARD `@return` | `Annotated` |
| `trees.test` | `TEST_TREES`, replaces the default | `environment::Names` |
| `trees.test_support` | `TEST_SUPPORT`, extends the default | `environment::Names` |
| `trees.migration` | `db`/`migrat` pairs, replaces the default | `environment::Names` |

## Must

1. **Put a gate at the registered-row filter, never inside a `workspace/rails/` reader.** Those
   files stay pure text in, text out.
2. **Gate twice in the pass:** `contribution`'s loop (cheap) and the `retain` after the walk
   (correct). A model with no macros joins `rails::MODELS` only in the `retain`.
3. **Gate every rung outside the pass on its specific flag, never on `features.rails`:**
   - `Return::Element` / `Return::Collection` in `types::resolved`, on `models`
   - `types::from_renderer` and `views::Views::reachable`, on `views`
   - the `is_structure` watch arm in `analysis/mod.rs`, on `schema`

   `Features::resolve` has already folded the umbrella flag into each of them.
4. **Never gate the inflector** (`rails::camelize`, `rails::element_of`). Singularising a name is not
   a Rails feature.
5. **Never make a request method switchable.** Capabilities are the wire contract, and
   `capabilities::server_capabilities` takes only the encoding. A switch changes what an answer is
   made of, never whether the question may be asked.

## Special cases

- **`rails::CONCERNS` is the one row a gem's `lib/` may join** (`Wants::gems`). It is gated on
  `models`, together with `MODELS`.
- **`workspace/willRenameFiles` is the only reader of `features.rails` on its own.** Zeitwerk
  belongs to no single body of knowledge. Never give it a key of its own.
- **`Views::default()` is off.** When empty, its renderer half would still query paths. The type
  rung asks `Views::rendered_by` instead of reading the flag a second time.
- **`knowledge/rspec.rs` has no key.** It is `#[cfg(test)]`, and there is nothing to decline.

## `rails.enabled = "auto"`, the default

1. **Answer yes if `config/application.rb` exists or `Gemfile.lock` locks `railties`.** Both checks
   are needed: engines have no `application.rb`, and a fresh clone has no lockfile.
2. **Read `Gemfile.lock` directly, never through `Workspace::gems()`**, which returns early when
   gems are off.
3. **Log the verdict once, at `info`.**
4. **Only `auto` touches the filesystem.**

## The fence keys

- **Replacing the target list is safe; extending the cursor list is safe.** Adding `lib` to the
  *target* list would silently delete answers, so `trees.test` and `trees.migration` replace the
  list. `test_support` only turns protection off, so it extends.
- **A replaced list logs what it replaced, at `info`**, from `index_workspace`. It is not a
  `messages::` sentence.
- **`trees.migration` entries are `parent/mark` pairs.** An entry without `/` is skipped, and
  `config::validate` says so.
- **`[]` means the fence is off, which differs from unset** (`Option<Vec<String>>`). `config.ts`
  sends the empty list.
- **They live under `[trees]`, not `[index]`.** Fenced files are still indexed, and
  `references`, `rename` and highlight still find uses in them.
- **The generator-template fence has no key.** No project layout changes what a template is.

## The cost of a key

- **Each key is four edits that a test enforces:** the `Config` field, the `PartialConfig` field,
  the `package.json` property and a `config.ts` branch
  (`every_setting_the_server_reads_is_one_the_editor_can_set`). Add a covered default and an
  `apply` arm. That is why there is no switch per macro.
- **`features.rs`, `config.rs` and `environment.rs` are all floored at 100.**
- **Published defaults are read from code:** `analysis::{TEST_TREES, MIGRATION_PAIR}` are
  re-exported for `tests/vscode_manifest.rs`.
- **The test harness declares Rails through the client's config layer**, not with marker files, so
  a fixture's own `ya-lsp.toml` can still turn a family off.

## Turning Rails off is about answers, not speed

- Generator lists are filtered by reference first, so a non-Rails project opens no files for them.
- **What a non-Rails project really pays:**
  - the projection walk (tens of ms per keystroke on a large app)
  - path conventions that fire on any `app/views/` or `*schema.rb`
  - the view context claiming bare words
- Settings descriptions must lead with that.
