---
paths:
  - "src/analysis/code_actions.rs"
---

# Refactorings: the second module that writes

`code_actions::at` builds four refactorings from one buffer and a Prism tree. The fifth action,
*show generated RBS*, lives in `requests::generated_actions` (`synthesized.md`). It writes nothing,
so none of the gates below apply to it.

## The three gates, all required

1. **Offer nothing where the file doesn't parse.** Recovery spans don't nest, so an edit would land
   in the wrong place.
2. **Every guard refuses; none approximates.** The bar is `renaming.md`'s: refactor *exactly* or
   not at all. That is why the module is in `COVERAGE_FLOORS` at 100: an untested guard is an action
   offered where it should not be.
3. **Apply every action to a copy and parse the result before offering it.** On a real app this
   catches shapes nobody anticipated, such as a `do … end` with `ensure` (an RSpec `around`), and two
   string literals joined by `\`.

## Per action

- **Toggle `{ }` ↔ `do … end`:** braces bind to the nearest call, and `do … end` to the outermost
  command. Decline when the block's own call is a command (no parentheses, at least one argument),
  or when a command encloses it in its arguments.
- **Generate `attr_reader`:** offer it only where `@x` belongs to an instance. `scopes::accessor_site`
  decides that and returns where the declaration goes.
- **Extract to variable, where placement is the whole job:**
  - Between the selection and its statement, there must be no construct that decides whether or how
    often its child runs.
  - The statement must own its lines, which refuses `a ? b : c` and `x if y`.
  - A node kind nobody has classified defaults to `Opaque`, which declines.
- **Extract to method:**
  - Decline when the extracted code writes a local that the rest of the method reads.
    `scopes::crossing` decides that from the scope stack, not from names.
  - Mirror the source method (`def self.x` → `def self.`), and write the new method after its `end`.
    Decline in `def obj.x` and in an endless `def`.
  - Refuse a multi-line string in the run: re-indenting it changes the string and still parses.
- **Pick fresh names by a whole-word search of the file**, deliberately over-strict. A name
  mentioned in a comment also moves on to `extracted_2`.

## The ancestor chain

- **Build it in pre-order**: every node that contains the selection, in visit order, is the path.
- Hook the 13 typed visits that the generic hook never fires for. `ranges::Selection` names the
  same 13.
- Dedupe on classification plus span. A `ProgramNode` and its statement list have the same bytes.

## Settled

- **Declined refactorings are silent.** A code-action menu opens on its own, so no `showMessage`.
  Titles are part of the answer and do not go in `messages.rs`.
- **Honour `only`, using the dotted hierarchy and matching on segment boundaries.** `refactor`
  matches `refactor.extract` but not `refactorings.x`. `CodeActionKind::EMPTY` is advertised, so a
  `quickfix`-only request must get no refactorings.
- **A stale keystroke doesn't settle for `codeAction`, but a cold server does.** Settling costs
  about 300 ms on a large file just for the fifth action. The split is `requests::settles` versus
  `requests::needs_the_graph`.
- **No actions in a template.** Every action writes a whole line, and in ERB a line belongs to the
  markup.
- **There are no `quickfix`es.** Most diagnostic rules are rubydex giving up, which has no fix.
  RuboCop serves autocorrect over its own `codeAction`.
- **Tests show the rewritten Ruby.** The exception is
  `an_action_is_a_title_a_kind_and_a_list_of_spans`, which pins the shape once.
