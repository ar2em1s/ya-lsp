---
paths:
  - "src/analysis/code_actions.rs"
---

# The refactorings, and the second module that writes

- **This is the second module that edits the user's files; it inherits `renaming.md`'s bar.** The
  question is "what can be refactored *exactly*", not "how much". The naive extract-to-method
  slices the selection out verbatim and passes no parameters, so any extraction touching a local
  assigned outside it produces a method that raises `NameError` on its first call. Every guard here is a *refusal*, which is why the module is on `COVERAGE_FLOORS` at
  100:100 — an untested guard is an action still offered where it should not be, invisible until
  somebody applies it.
- **A refusal here is silent; that is the difference from a rename.** A rename is a deliberate
  keystroke, so declining earns a `window/showMessage`. A code action is a menu the editor opens on
  its own; an absent action is the whole of what needs saying, and a notification per cursor
  movement would be intolerable. `messages.rs` is not involved, and neither are the titles: a title
  is part of one request's answer, like a hover card or a completion label.
- **Three gates, and the last is not decoration.** (1) Nothing is offered where the file does not
  parse — recovery hands out spans that do not nest, and an edit placed by one lands elsewhere in
  the buffer. (2) Every guard refuses rather than approximating. (3) **Every action is applied to a
  copy and the result parsed before it is offered.** Swept over a real application, gate 3 removes
  actions every guard above it allowed, and both families it caught are spellings nothing here
  anticipated:
  a `do … end` block carrying an `ensure`, which a brace block cannot; and two adjacent string
  literals joined by a line continuation, which are one string with two nodes. The first is what an
  RSpec `around` hook looks like.
- **`{ }` and `do … end` do not bind the same way, and the guard also catches a syntax error.** With
  braces the block belongs to the nearest call; with `do … end` to the outermost command. So a
  toggle is declined when the block's own call is a command (no parentheses, at least one argument)
  or when a command call encloses it *in its arguments*. `puts show [1, 2].map { |n| n * 2 }` prints
  `[2, 4]` and its `do … end` spelling prints an `Enumerator`; it parses and it runs. The same test
  refuses `it "works" do … end`, whose brace form is a syntax error.
- **`attr_reader :count` reads an *instance's* `@count`, so the accessor is offered only where the
  variable is an instance's.** Inside `def self.count` or a `class << self` it would read a different
  variable, return `nil`, and look right. `scopes::accessor_site` answers it, because the algebra
  that knows the difference is `SelfContext`'s level and a second copy in the caller would drift. It
  also hands back where the namespace body begins, which is where the declaration goes.
- **Placement is the whole of extract-to-variable.** Hoisting an expression out of a branch, a loop
  or the right of an `&&` changes how often it runs. The rule: nothing between the selection and its
  statement may be a construct deciding whether or how many times its child is evaluated — **and the
  statement has to own its lines**. That second half is semantic, not cosmetic: `a ? b : c` puts each
  arm in its own statement list and so does `foo.bar if baz`, so in both the "statement" found is a
  fragment that neither begins nor ends its line, and one textual test refuses both. The
  classification table's default is `Opaque`, not a value: a node kind nobody has thought about
  declines both questions, and Prism has 150 of them.
- **The chain is the ancestor chain because pre-order says so, not because it is sorted.** A node
  containing the selection is on the path to it, and a walk announces a parent before its children,
  so recording every containing node in visit order *is* the path. Two things it needs that a naive
  walk does not: the **thirteen typed hooks the generic one never fires for** (`ranges::Selection`
  names the same thirteen), and a dedup comparing the *classification* as well as the span, because
  a file's `ProgramNode` and the statement list inside it are the same bytes and dropping the second
  leaves top-level code with no statement list.
- **A local the extraction writes and the rest of the method reads has no single value to hand back,
  so it is declined.** `scopes::crossing` answers both halves — which locals a range reads before
  writing, and whether anything it writes is touched afterwards — from the scope stack rather than
  from names, so a block's `n` and the method's `n` are not confused. Two locals spelled alike are
  **not** a case; working that out removed a guard rather than adding one, since a run lies inside
  one scope, an inner `n` shadows an outer one throughout, and a block inside the run writes its own
  before reading it.
- **The extracted method mirrors the one it came from, written after its `end`.** `def self.x`
  extracts into `def self.` or the call does not resolve; `def obj.x` is an island and is declined;
  an endless `def` has no `end` to write beside. Inserting after the enclosing method's `end` also
  keeps the lexical context — inside a `class << self` the sibling lands inside it too, with no rule
  about that written anywhere.
- **A multi-line string literal in the run is refused; it is the one failure the parse gate cannot
  see.** The body is re-indented on the way out, and re-indenting a string changes what it says while
  leaving a file that still parses. The heredoc half is caught by the same rule's other half — an
  opener without its body does not parse alone.
- **Names are made free of the file by a whole-word search over the source, deliberately
  over-strict.** A mention in a comment moves on to `extracted_2`. A name nothing in the file spells
  cannot shadow a local, a receiverless call, or anything else — a property of the search rather
  than of a list it would otherwise have to enumerate.
- **Declined in a template, for a reason no other request has.** Every action writes a **line**, and
  in a template a line belongs to the markup. `erb::ruby_view` keeps the offsets so everything that
  reads answers unchanged, and there is nothing it can do about a line starting `<td>`.
- **The test drawing is the rewritten Ruby** (`renaming.md`'s rule, same reason): a span one byte out
  writes visibly broken code where a list of `{line, character}` pairs shows nobody anything. The one
  exception is `an_action_is_a_title_a_kind_and_a_list_of_spans`, which pins the shape the caller
  converts, once.
- **No `quickfix`, and the reason is the diagnostic table's shape.** Two of its ten rules are
  statements about the user's code; the other eight are rubydex saying *it* gave up. A fix needs a
  rule that knows what the code should say instead, and "the indexer could not follow this" does not.
  The autocorrect half of `codeAction` is RuboCop's, served over its own `textDocument/codeAction`
  from 1.89, so this composes rather than competes.
