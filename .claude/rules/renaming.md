---
paths:
  - "src/analysis/rename.rs"
---

# Rename

- **This is the only module in the crate that writes.** Every other wrong answer shows the user
  something unhelpful; a wrong answer here edits their files. The organising question is not "how
  much can be renamed" but "what can be renamed *exactly*" — locals through `scopes`, constants
  through the graph. If a change widens what is renameable, test whether the new case is exact or
  merely usually right.
- **The guard is "every definition of this name is somewhere ya-lsp will edit."** It exists for
  `class String` reopened in the user's code beside Ruby's signatures, where renaming would rename
  half a name. A declaration ya-lsp *generated* fails the same test, so a `class Story` generated
  from `db/schema.rb` makes renaming the model declined out loud. That is the safe answer, not a
  gap: the alternative reaches through the side table and edits `db/schema.rb`, a generated file
  whose column is not renamed by rewriting it. Reopening this needs its own decision and test —
  `synthesized.md`.
- **Every span is read back and confirmed to hold only the old name before any edit is emitted; one
  failure refuses the whole rename.** A rename that changed most of the places a name is written
  leaves code that no longer runs. The confirmation fires on ordinary Ruby, not as padding: rubydex
  records the name span of `Error = Class.new(StandardError)` as the **entire assignment**, so
  trusting the span replaces the class with the new name. `narrow` recovers by finding the one
  *whole-word* occurrence inside the span, and refuses when there are two
  (`Registry = Class.new { include Registry }`), since either could be the one being defined.
- **A name followed by a single `:` is refused; `::` is not.** Three ordinary spellings put a
  variable's name somewhere it means more than itself: a keyword parameter, whose name is the
  method's interface, and Ruby 3.1's `{ host:, port: }` and `connect(host:)`, where the word is the
  key *and* a read of the local — replacing it renames the key with the value, changes the hash, and
  **still parses**. `mod::CONST` is a constant looked up on a local and is the commonest legitimate
  name-before-a-colon, hence the second half of the test. It declines a small fraction of real
  parameters.
- **Prism decides what a Ruby name is, asked of both the new name and the old.** `is_name` parses
  `<candidate> = 1` and checks the assignment Prism made is the kind being renamed and that its name
  span spells the candidate exactly. That knows `Ünicode` is a constant while `é` is a variable
  (Ruby's rule is Unicode's case, not ASCII's), that `nil`, `_1`, `self` and `__FILE__` cannot be
  assigned, and that `first second` — which parses with **no error** — is a call rather than a name.
  A regex gets the first and last wrong; a keyword list goes stale the next time Ruby adds one.
  Asking it of the old name also rules out every fabricated name, `Person::<Person>` included.
- **Every edit is in the user's own code, and a constant is refused unless *every* place it is
  written is.** "Is any of it mine?" is the wrong test: a class of the user's reopening a gem's has
  one definition in each, and renaming their half leaves the gem defining the old name. A
  declaration written down nowhere — the `Ghost` of `Ghost::Thing = 1` with no `module Ghost` — is
  refused for the same reason.
- **A refusal is a `window/showMessage`, not an error response and not a bare `null`.** `null` is the
  protocol's "not here" and the editor needs it, but alone it makes the editor say only that nothing
  can be renamed. These are the one place a message is about a single request rather than the
  workspace; pressing a key earns it. See `messages.md`, which records the exception.
- **`prepareRename` runs the whole plan, confirmation and all.** Answering with a range promises the
  rename will go through, so a prepare that said yes and a rename that refused would put the refusal
  after the user typed the new name. It costs one file read per file the name appears in, on a
  deliberate keystroke.
- **The offsets and the wire range come from one read.** `Replacement` carries both, because the two
  things done with a span are a containment test against the cursor and a conversion; deriving the
  second from the first again, in the other handler and possibly from a re-read file, is two chances
  to disagree about one span.
- **No cap, deliberately.** `references` caps because a name-based method match can be unbounded and
  a truncated list is merely a wrong answer; a truncated *rename* is a corrupted file. An exact
  constant match is as large as the truth and bounded by the user's codebase: the widest real one
  measured runs to thousands of edits across hundreds of files, answered without a perceptible
  wait.
- **The test drawing is the renamed Ruby.** A span one byte out writes visibly broken code — a name
  run into the one beside it, a hash key changed with its value, an `end` eaten — where a list of
  `{line, character}` pairs shows nobody anything. Untouched files are not drawn, so an expected
  block holds exactly what the rename claims to change, and the control cases (`Other::Person`, the
  positional `host`) are visible rather than asserted separately.
