---
paths:
  - "src/analysis/erb.rs"
---

# ERB templates

- **Blank, do not extract: a position map is a second coordinate system.** The alternative is to
  extract a template's Ruby into its own buffer and keep a map back; every request then has to be
  right in both, and every bug in the map looks like a bug in the request. Replacing markup with spaces — one
  space per byte, newlines kept — makes the template's offsets *the only offsets there are*, which
  is why the requests it shipped with needed one gate between them rather than a port each. The
  codebase already trusted the technique twice: `signatures::without_interfaces` and
  `cursor::Finder::without_the_half_typed_call`.
- **Pad by byte, never by character.** The natural spelling in Ruby is `" " * char.length`, in
  characters, because Ruby strings are character-indexed. ya-lsp is UTF-8-byte-offset end to end, so
  padding that way shortens the buffer by one byte per accent and three per emoji in the markup
  *above* the cursor, and every answer below lands on the wrong column. One Cyrillic-and-emoji line
  is enough to show the drift. It fails silently, and only for people who do
  not write markup in English. The `proptest` in `erb.rs` holds it — same length, same newline
  offsets, over an alphabet including an emoji and a lone `\r`.
- **A comment tag is blanked whole; both other answers were tried and measured.** Over the canary's
  templates, counted as parse errors in the Ruby view: `#` blanked as a sigil like `=` and `-` gives
  **hundreds**; `#` kept, so `<%# … %>` becomes a Ruby comment, gives **dozens**; blanking the tag
  gives **almost none**. The middle answer is the seductive one and is wrong for a reason worth
  keeping: a Ruby comment ends at the newline, so only the *first* line of a multi-line `<%# … %>` is
  commented and the rest is copied back as code. Most of its errors come from one four-line comment
  in a Rails partial, where the "`for` loop with no `in`" it reports is the English word *for* at the
  start of that comment's second line. `<% # … %>` stays code — there the `#` is Ruby's own
  marker inside an ordinary tag.
- **The two that survive are `<%= yield %>`, and they are the argument for no diagnostics in
  templates at all.** A compiled Rails template *is* a method body, so `yield` is legal there; Prism
  is reading a file and is right to refuse it. There is no length-preserving edit that makes it
  legal. `Analysis::collect_diagnostics` drops a template's diagnostics — `diagnostics.rs`'s own rule
  that a check firing on correct input has not earned a squiggle — and `make canary` asserts no
  template published one, checked non-vacuously by turning the drop off (20 templates, 65 extra
  `parse-warning`s and both `parse-error`s appear).
- **Two hooks, each needed for one reason.** `Analysis::index_templates` blanks on the cold walk,
  because a real Rails application keeps **one method-call site in seven** in its templates and
  indexing only what the editor has open makes `references` incomplete *by an amount that changes as
  the user opens tabs* — the failure `coverage.md` holds `references.rs` at 100 to prevent.
  `Analysis::index_buffer` blanks on the buffer path, which `didOpen`, `didChange` and the file
  watcher share, exactly as the `.rbs` interface rule does. A template reaching the graph raw records
  **no references at all**, so the blanking is the feature, not an optimisation.
- **`Analysis::with_text` hands out the blanked view too, and forgetting that half is the easiest bug
  to miss.** Ten of the eighteen requests parse the document themselves rather than reading the graph
  — the outline, the folds, the scope walk under highlight and rename, the semantic tokens, the
  cursor under a typed receiver — and every one would otherwise be handed markup to parse as Ruby.
  The graph half looked completely correct while `semanticTokens` returned one token for a template
  with four identifiers. The *buffer* stays exactly what the editor sent, because that is what
  incremental edits apply to; `with_source` is the one accessor reaching it, and only `completion`
  needs it.
- **The byte rule is right for rubydex and was silent about the client.** An LSP position arrives as
  a count of **UTF-16 units of the text the editor has**, and `Analysis::with_text` converted it
  against the *blanked view*, where a 3-byte `“` in the markup has become three spaces. So the cursor
  was displaced left by (bytes − UTF-16 units) of every non-ASCII character in the markup before it
  on the same line, and every span answered back displaced right by the same amount. Every request
  taking a cursor in a template was affected: hover, definition, completion, highlight, rename,
  semantic tokens, code actions, selection ranges. It is a **wrong** answer, not only a missing one,
  which `a_cursor_in_a_template_is_where_the_editor_put_it` reproduces rather than illustrates: point
  the conversion back at the view and the fixture's middle row jumps to `class Story` instead of the
  constant the caret is on, seven units right.
- **The fix is not in this file, and the byte rule is why.** Blanking stays byte-for-byte because
  rubydex's offsets have to be the template's own; what moved is the *conversion*.
  `TextDocument::blanked` carries the source beside the view — a document is **read** as one and
  **addressed** as the other, and the byte offset is the coordinate they share. `with_text` is the
  only caller; `core-invariants.md` has the rule.
- **The reach is dozens of templates over six corpora, and two of the six are provably zero.** A scan
  knowing nothing of the view context finds every identifier behind non-ASCII markup on its own line;
  it is concentrated in one or two corpora and **absent from two, neither of which writes a non-ASCII
  character outside a Ruby string in any of its templates**. Only a small fraction of all templates
  hold one, and the largest displacement in any corpus is a few units. Over every affected position in
  the corpora swept, asked with `definition` and `prepareRename` against a **ground truth** rather than
  a baseline, the server names the word under the caret in **most cases where it named none before**,
  and answers a span not containing the caret **nowhere**, against many before. A few still answer a
  wider span and are right to: `response[:published_time]` is one element-reference call. A few go from
  an answer to a silence and cost nothing — the old span was displaced at both ends and overlapped the
  caret by accident, and what is silent now is a gem this machine has not installed.
- **The displaced set is confirmed by a second instrument sharing no code with the first.**
  Intersected with the template call sites a view-context sweep asks about, **every one of them
  answered nothing**; re-running that sweep against a fixed binary moves exactly those from `nothing`
  to `derived`, **worse nowhere**. The handful that still answer nothing are the sweep over-collecting
  inside `%i[]`, `%r()` and a `#` in a tag. **Nothing outside a template can move**: a non-template
  `TextDocument` has no source beside its text, so every conversion is the one it always was.
- **`completion` is the only request that needs to know it is in markup, and that is a measurement.**
  Every other positional request finds no identifier under a cursor in blanked markup and already
  answers nothing. Completion does not need a token under the cursor to be meaningful: with the gate
  removed, a caret in an `<h1>` is offered a page of the workspace's constants.
  `every_request_asked_inside_a_tag_and_in_the_markup_beside_it` is that whole table, drawn side by
  side, because what has to be legible is *where the answers stop*.
- **`foldingRange` is declined by returning `null`, and it is the one decline that is real.**
  `ranges.md` wrote the mechanism down before ERB was on the table: a client with a folding provider
  stops guessing from indentation, so an empty array takes the fallback away *and* puts nothing in
  its place, while `null` hands it back. The walk sees the Ruby and nothing else — two folds for the
  `<% %>` blocks and none for a five-line `<div>` — so what the editor does unaided is better.
  `documentSymbol` and `rename` were both proposed for declining and neither needed it: the outline
  is `null` for a blocks-only template and correct for one declaring a class, and `rename` already
  reaches a block local across two tags and a constant into its declaring file. Refusing `rename`
  would have meant writing code to break something that works.
- **The editor and the server must agree which extensions are templates, and neither compiler sees
  the other side.** `erb::is_template` and the manifest's `erb` language contribution are held
  together by `tests/vscode_manifest.rs`. A file the manifest claims and the server does not blank is
  indexed as Ruby, markup and all; one the server blanks and the manifest does not claim opens as
  plain text and starts no server. `is_template_uri` delegates to `is_template` rather than testing
  the URI's suffix, for the same reason `Workspace::indexes` and `Workspace::discover` share globs.
- **`index.include` gained `**/*.erb` as a default, which is a visible settings change.** It is also
  why the canary's exact file count moved: it added every template the canary repository does not
  gitignore. Gems cannot be dragged in by it: `gems.rs`'s walk has its own
  hard-coded `.rb` filter, so a gem's Rails generator templates stay out of the graph however wide
  the workspace include gets.
