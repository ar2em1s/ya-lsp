---
paths:
  - "src/analysis/erb.rs"
---

# ERB templates

ya-lsp reads a template as Ruby by **blanking** the markup: one space per byte, newlines kept. The
template's offsets are then the only offsets, so no position map is needed. The same technique is
used by `signatures::without_interfaces` and `cursor::Finder::without_the_half_typed_call`.

## Must

1. **Pad by byte, never by character.** Offsets are UTF-8 bytes end to end. Character padding moves
   every answer below a non-ASCII character in the markup. A `proptest` in `erb.rs` holds this.
2. **Blank a `<%# … %>` comment tag whole.** Keeping the `#` comments out only its first line.
   `<% # … %>` stays code, because that `#` is Ruby's.
3. **Blank on both paths:** `Analysis::index_templates` on the cold walk, and `Analysis::index_buffer`
   for `didOpen`, `didChange` and the watcher. A raw template records no references at all.
4. **`Analysis::with_text` returns the blanked view.** Most requests read it (outline, folds,
   scopes, tokens, hints, cursor). The editor's buffer stays exactly as sent, and `with_source`
   reaches it (completion only).
5. **Convert positions against the source and address the view.** LSP positions count UTF-16
   units of the editor's text. `TextDocument::blanked` carries both, and `with_text` is its only
   caller (`core-invariants.md`). Held by `a_cursor_in_a_template_is_where_the_editor_put_it`.

## What is gated in a template

- **Diagnostics are dropped** (`Analysis::collect_diagnostics`). `<%= yield %>` is legal in a
  compiled template but not in a file, and no length-preserving edit fixes that. `make canary`
  asserts that no template publishes one.
- **Completion is gated when the caret is in markup.** Otherwise a caret inside `<h1>` gets every
  constant in the workspace. Held by `every_request_asked_inside_a_tag_and_in_the_markup_beside_it`.
- **`codeAction`:** the four refactorings are declined, because they write whole lines. *Show
  generated RBS* stays (`code-actions.md`).
- **`foldingRange` returns `null`**, so the editor's indentation folding handles the markup
  (`ranges.md`).
- **`documentSymbol` and `rename` need no gate.** Both already work.

## Which files are templates

- **`erb::is_template` and the VS Code manifest's `erb` language must agree.**
  `tests/vscode_manifest.rs` holds them together. `is_template_uri` delegates to `is_template`.
  Test the extension, never `contains("erb")` (think of `.erb.tt`).
- **`.jbuilder`, `.builder` and `.ruby` are indexed as plain Ruby, not as templates.**
  - They are globs in `config.rs` only. `is_template` must stay false for them, because blanking a
    file that has no tags erases it.
  - They keep their diagnostics. A syntax error there is the user's own bug. Held by
    `a_jbuilder_is_indexed_whole_and_keeps_the_squiggle_a_template_beside_it_loses`.
  - They get no view context yet: no `current_user`, no `*_path` helpers, no controller `@ivar`s
    (`views.md`). A corpus's `.rss.ruby` files are the fixture to write that against.
- **Gems never contribute templates.** `gems.rs` has its own `.rb` filter.
