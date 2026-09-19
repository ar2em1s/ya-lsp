---
paths:
  - "src/analysis/rename.rs"
---

# Rename

This module edits the user's files, so a wrong answer corrupts code. Ask "can this be renamed
*exactly*?", never "how much can be renamed?". Locals go through `scopes`, and constants through the
graph.

## Must: every rename

1. **Edit only the user's own code, and refuse a constant unless *every* place it is written is
   theirs.** A user class that reopens a gem's class (or `class String`) is refused. A generated
   document does not count as a place: `decide` filters it out first. Held by
   `a_class_a_generated_document_also_declares_renames_in_the_files_that_hold_it`.
2. **Read every span back, and confirm it holds only the old name before emitting any edit.** One
   failure refuses the whole rename. rubydex gives `Error = Class.new(StandardError)` a name span
   that covers the whole assignment. `narrow` finds the one whole-word match, and refuses if there
   are two.
3. **Refuse a name followed by a single `:`; allow `::`.** A keyword parameter, `{ host:, port: }`
   and `connect(host:)` would rename the key along with the value, and the result still parses.
4. **Let Prism decide what a name is, for both the old and the new name** (`is_name`). It parses
   `<candidate> = 1`, which handles `Ünicode` vs `é`, keywords, and `first second`. Never use a regex
   or a keyword list.
5. **`prepareRename` runs the whole plan, including the confirmation.** Answering with a range
   promises that the rename will succeed.

## Refusals

- **Refuse with `window/showMessage`, not an error and not a bare `null`** (the exception is
  explained in `messages.md`).
- **Two empty cases, two messages:**
  - A generator declared the name (`Story::ActiveRecord_Relation`): `rename_refuses_generated`,
    which says to rename the source instead.
  - Nothing declares it (`Ghost` in `Ghost::Thing = 1`): `rename_refuses_foreign`.

## File moves (`workspace/willRenameFiles`)

1. **`moved` turns a move into a cursor and a new name, then hands both to `plan`.** No separate
   decision about what may be edited is made there.
2. **Follow a move only when all four rules hold. Otherwise stay silent**, because a drag is not a
   keystroke:
   - the file's name spells the class inside it
   - the old path is under an autoload root and spells the name exactly
   - the new path is a constant in the same namespace
   - the derived name is valid Ruby, which `camelize` alone doesn't guarantee, so it also goes
     through `is_name`
3. **Find loosely, write exactly.** `rails::same_constant` (underscores and ASCII case ignored)
   finds `APIKey` in `api_key.rb`. Writing requires the name to equal exactly what the path spells,
   because the acronym table is Ruby we cannot run.
4. **Only a plan that was made and then failed speaks** (`renaming`).
5. **Gate on `rails.enabled`; the capability itself is always advertised** (`features.md`).

## Settled

- **One move is one `WorkspaceEdit`**, so the user gets one undo step. A move that changes nothing
  answers `null`.
- **`Replacement` carries both the offsets and the wire range from one read**, so they cannot
  disagree.
- **No cap.** A truncated rename corrupts files. Real maximums (thousands of edits) return with no
  perceptible wait.
- **Tests draw the renamed Ruby.** Untouched files are not drawn. Control cases (`Other::Person`, a
  positional `host`) stay visible.
