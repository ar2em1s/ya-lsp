---
paths:
  - "src/analysis/rename.rs"
---

# Rename

- **This is the only module in the crate that writes, and that is the whole design.** Every other
  wrong answer shows the user something unhelpful and they look elsewhere; a wrong answer here
  edits their files. So the question the module is organised around is not "how much can be
  renamed" but "what can be renamed *exactly*", and the two things that can are the two the
  earlier items already made exact — locals through `scopes`, constants through the graph. If a
  change here starts to widen what is renameable, the test to apply is whether the new case is
  exact or merely usually right.
- **Every span is read back and confirmed to hold only the old name before any edit is emitted,
  and a failure refuses the whole rename.** Not part of it: a rename that changed most of the
  places a name is written leaves code that no longer runs, which is worse than one that changed
  none. The confirmation fires on ordinary Ruby rather than being defensive padding — rubydex
  records the name span of `Error = Class.new(StandardError)` as the **entire assignment**, so
  trusting the span would replace the class with the new name. `narrow` recovers that by finding
  the one *whole-word* occurrence inside the span, and refuses when there are two
  (`Registry = Class.new { include Registry }`), because either could be the one being defined.
- **A name followed by a single `:` is refused, and `::` is not.** Three ordinary spellings put a
  variable's name somewhere it means more than itself and all three end in a colon: a keyword
  parameter, whose name is the method's interface; and Ruby 3.1's `{ host:, port: }` and
  `connect(host:)`, where the one word is the key *and* a read of the local — replacing it renames
  the key with the value, which changes the hash and **still parses**, so no test that only checks
  that the result compiles would catch it. `mod::CONST` is a constant looked up on a local and is
  the commonest legitimate name-before-a-colon, which is why the second half of the test exists.
  Costs 6 of 400 real parameters, measured on solargraph.
- **Prism decides what a Ruby name is, asked of the new name and of the old one.** `is_name` parses
  `<candidate> = 1` and checks that the assignment Prism made is the kind being renamed and that
  its name span spells the candidate exactly. That is what knows `Ünicode` is a constant while `é`
  is a variable (Ruby's rule is Unicode's case, not ASCII's), that `nil`, `_1`, `self` and
  `__FILE__` cannot be assigned to, and that `first second` — which parses with **no error at
  all** — is a call rather than a name. A regular expression gets the first and last of those
  wrong; a hand-written keyword list goes stale the next time Ruby adds one. Asking it of the old
  name is also what rules out every name the graph fabricated, `Person::<Person>` included.
- **Every edit is in the user's own code, and a constant is refused unless *every* place it is
  written is.** `declared_in`-style "is any of it mine?" is the wrong test here: a class of the
  user's that reopens one a gem defines has one definition in each, and renaming their half alone
  leaves the gem defining the old name. A declaration written down nowhere at all — the `Ghost` of
  `Ghost::Thing = 1` with no `module Ghost` — is refused for the same reason, since its one real
  definition is somewhere the index cannot see.
- **A refusal is a `window/showMessage`, not an error response and not a bare `null`.** `null` is
  what the protocol has for "not here" and it is what the editor needs, but on its own it makes
  the editor say only that nothing can be renamed. These are the one place in the crate a message
  is about a single request rather than about the workspace, and pressing a key is what earns it —
  see `messages.md`, which records the exception.
- **`prepareRename` runs the whole plan, confirmation and all.** Answering with a range is a
  promise the rename will go through, so a prepare that said yes and a rename that then refused
  would put the refusal after the user had typed the new name. It costs a file read per file the
  name appears in, once, on a deliberate keystroke.
- **The offsets and the wire range are produced from one read.** `Replacement` carries both
  because the two things done with a span are a containment test against the cursor and a
  conversion, and deriving the second from the first a second time — in the other handler, from a
  file possibly read again — is two chances for them to disagree about one span.
- **There is no cap, deliberately.** `references` caps because a name-based method match can be
  unbounded and a truncated list is merely a wrong answer; a truncated *rename* is a corrupted
  file. An exact constant match is as large as the truth and bounded by the user's own codebase:
  the widest real one measured is `Solargraph`, 2,877 edits across 354 files, in 140 ms.
- **The test drawing is the renamed Ruby.** A span one byte out writes code that is visibly
  broken — a name run into the one beside it, a hash key changed with its value, an `end` eaten —
  where a list of `{line, character}` pairs shows nobody anything. Files no edit touched are not
  drawn, so an expected block holds exactly what the rename claims to change, and the control
  cases (`Other::Person`, the positional `host`) are visible in it rather than asserted separately.
