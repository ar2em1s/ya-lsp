---
paths:
  - "src/**"
  - "tests/**"
  - "build.rs"
---

# Crate-wide invariants

## Structure

1. **Only `analysis/` may name a rubydex type.** rubydex's API churns, and this keeps the blast
   radius to one directory.
2. **`analysis/mod.rs` is the thread, and `analysis/requests.rs` is the request layer.** Sibling files
   see each other's private items. Only `Analysis::serve`, `own_documents` and `file_name` are
   `pub(super)`.
3. **A test lives next to the code whose invariant it would break.** `analysis/testing.rs`'s
   `Harness` exposes only plain values (`has`, `declarations_of`, `generated_for`,
   `document_count`). A test that needs more is a test of the analysis thread, and belongs in
   `analysis/`.
4. **Every `mod tests` carries `#[cfg_attr(coverage_nightly, coverage(off))]`** (`coverage.md`).

## Must never

1. **Write to stdout**, except through the LSP transport. A stray `println!` disconnects the editor
   silently. Logs go to stderr through `tracing`.
2. **Call `Resolver::resolve` anywhere but `analysis::resolve`**, or rubydex's indexing entry points
   anywhere but `indexer::index_files` / `indexer::index_source`.
3. **Take a `&mut Graph` except through `indexed::Indexed::graph_mut`.** It drops every side index
   (the member index and `Placed`). `Indexed` implements `Deref`, deliberately not `DerefMut`, so a
   forgotten invalidation fails to compile.
4. **Remove the default panic hook, or set `panic = "abort"`** in any profile. The contained panics
   must still print rubydex's file and line.
5. **Coalesce or reorder `didChange` changes.** Each range applies to the text the previous change
   produced.

## rubydex facts

- **Offsets are always UTF-8 bytes; `Graph::set_encoding` does nothing.** Convert with
  `position::TextDocument`, never with `Offset::to_location`.
- **Method declarations carry parentheses:** `"Person#shout()"`, not `"Person#shout"`. References
  use the bare name, except an `alias`'s old name, which carries them. `locator::member_name`
  reconciles the two. `StringId::from(&str)` is a pure hash.
- **Indexing needs absolute paths.** A relative path fails silently.
- **Pinned by git `rev`, never by branch.** Moving the rev is a port plus a corpus sweep, never a
  bump: upstream once deleted a behaviour and kept its doc comment. Rule severities and config names
  stay ya-lsp's own (`InvalidPrivateConstant` kept its key).

## The three panic seams

| Seam | Where | Failure unit | After |
|---|---|---|---|
| Indexing | `analysis/indexer.rs` | one file | goes on `Analysis::skipped` |
| Resolution | `analysis::resolve` | the graph | `rebuild`, guarded by `recovering` |
| Requests | `Analysis::serve` (settle + dispatch) | one request | `InternalError` (not a `messages::` sentence) |

- **Indexing uses our own pool:** one file at a time, `catch_unwind` around `build_local_graph`,
  and a serial merge. One `catch_unwind` around the whole of `index_files` loses good files. The
  merge is *not* contained.
- **`skipped` survives `rebuild`, and clears itself** once a later index of that file succeeds.
  `record_skip` warns every time and shows a message once.
- **All three seams stay** even though the pinned rev fixed the known inputs. Upstream `main` still
  has the unwraps.

## Documents and URIs

1. **Key every document by `workspace::uri::DocUri`**, which spells URIs the way rubydex does.
2. **Exactly two schemes are documents: `file:` and `untitled:`** (`DocUri::adopt`). Everything else,
   including `rubydex:built-in` and `ya-lsp-generated:`, is refused. An `untitled:` has no path, so
   `to_file_path` returns `None`. Use `is_untitled`.
3. **A template is blanked on every route** (`index_templates`, `index_buffer`, `with_text`), and
   only `completion` reads the markup (`with_source`, `erb.md`).
4. **A template is *read* as the view and *addressed* as the source.** `TextDocument::blanked` holds
   both, and every conversion goes through `coordinates()`. Held by
   `a_blanked_document_is_addressed_as_the_text_the_client_has`.
5. **A span in an unopened file is placed from rubydex's `Document::line_index`**
   (`position::range_in`, `requests::Analysis::indexed_ranges`).
   - Open buffers and templates fall back to reading the file.
   - `.rbs` is not gated.
   - The file's existence is still checked
     (`a_definition_whose_file_is_gone_answers_nothing_rather_than_a_dead_link`).

## Whose code it is

- **Go through `is_own_code`, which delegates to `environment::Layout::is_own`.** It excludes
  `foreign_prefixes` (gem roots, the RBS root, Ruby's library), even inside the workspace, and admits
  `own_prefixes` (`[index] load_paths` outside the root). A gem root must still answer no.
- **`resolve_load_path` is the only spelling of a load path.** The walk, this list and registration
  all read it.

## Telling the client where answers are

- **`Analysis::register_documents` registers gem, stdlib and RBS roots** over
  `client/registerCapability` once the bundle is known.
  - Register one directory per *resolved* gem, never `Gems::roots`.
  - Never register the workspace's own prefix.
- **Every request method appears in `capabilities::DYNAMIC`**, or is ruled out in `NOT_A_DOCUMENT`.
- **Registration ids are fixed strings**, so a reload unregisters first.
- **Response shapes follow client capabilities** (`ClientSupport`, all default false). Each goto
  gets its own `linkSupport` field. A new goto adds a field instead of reusing `definition_links`.

## Resolving a call: two ways, on purpose

- **`locator::resolve_typed`** also uses ya-lsp's own receiver types. `hover` and `definition` use it.
- **`locator::resolve`** has no text. `references`, the type hierarchy and `rename` use it, because a
  work list must not contain a derived guess.

## Walk, predicate and watcher agree

All four share the compiled globs and the walk:

- `Workspace::discover` (the walk)
- `Workspace::indexes` (the predicate)
- `Workspace::admits` (`indexes` plus `db/structure.sql`, strictly wider)
- `workspace::watched_directories`

Held by `the_predicate_answers_exactly_what_the_walk_collected` and
`every_indexed_file_sits_in_a_directory_the_watcher_listens_to`.

## Other

- **Diagnostic defaults are measured.** Run a rule over a corpus before changing its default. Most
  rubydex rules ship `Off`.
- **Clear diagnostics by sending an empty array.** Sending nothing leaves the old squiggles.
- **`position.rs` is held by `proptest` properties** over an alphabet with an accent, CJK, an
  emoji, a combining mark, and `\n`, `\r\n` and a lone `\r`. Add properties freely; never weaken the
  generator.
