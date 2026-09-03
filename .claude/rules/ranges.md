---
paths:
  - "src/analysis/ranges.rs"
---

# Folding and expand-selection

- **A folding provider *replaces* the editor's indentation guess; it does not add to it.** VS Code
  runs its `IndentRangeProvider` only when no `foldingRangeProvider` answered, so every shape this
  module fails to recognise is folding that quietly stops existing — which is why the walk covers
  what the guess already got right (definitions, blocks, literals) as well as what it cannot see
  (heredoc bodies, `#region`, comment runs, three branches instead of one), and why an empty
  answer is sent as `null`. A `null` can only hand the guess back; an empty array takes it away
  and puts nothing in its place. It is also the argument for `ranges.rs` being in
  `COVERAGE_FLOORS`: a construct nobody wrote a test for is not a visible bug, it is a chevron
  that never appears.
- **Every range ends on the last line of the body, never on the closer.** A collapsed range hides
  the lines *after* its first, so a `def` whose range reached its `end` would hide the `end` and
  read as broken code rather than as folded code. The same rule makes a single-line construct
  produce nothing at all: `def foo; end` would hide nothing and leave a chevron that does nothing
  when clicked. `line_fold` is the one place both are decided, so a fold from syntax and a fold
  from a comment cannot disagree about it.
- **Two node locations lie about where the body ends, and both hide the `end` if believed.** A
  `def`, `class` or block with a bare `rescue` in it is given an *implicit* `BeginNode` whose
  location runs to the enclosing construct's `end`, because that is the keyword closing it; and an
  `ElseNode` carries the `end` keyword inside its own location, so a `case` measured by its last
  branch swallows it too. Both are measured by the line above their closer instead. Neither is
  visible in a fixture whose `case` has no `else` and whose `def` has no `rescue`, which is why
  the drawings have both.
- **Prism's `Visit` announces most nodes through `visit_branch_node_enter` and thirteen through
  their typed method instead**, and for those thirteen the generic hook never fires. Two of them
  are `ArgumentsNode` and `StatementsNode` — "one argument before the argument list" and "a body
  before the block" — so this is not an exotic corner. `selection_chain` overrides twelve; the
  thirteenth, `BlockArgumentNode`, is reachable only from an index assignment carrying a block,
  which Ruby's own parser rejects, so it is left alone rather than written and never run.
- **A selection chain is *defined* by every link containing the one before it**, and Prism's
  recovery hands out locations that do not — the trap `locator::spans` already carries.
  `locator::nests` is the one predicate both ask, because a containment test written twice is one
  that will eventually disagree with itself. The chain is built by filtering rather than by
  trusting the parser, and the sweep that proves it types a file one character at a time and
  checks every position of every prefix.
- **The outermost link is always the buffer**, which is what makes `selection_range` total. LSP
  pairs chains to positions by index and has no spelling for "not this one", so a position Prism
  could not place anywhere still answers. That link is the seed of the fold rather than a
  fallback arm, so there is no unreachable case to test.
- **The steps this adds are the ones Ruby has that a generic walk misses**: a string's contents
  before its quotes, one argument before the list, a body before what opens it, a message and then
  its receiver before the next call in a chain. The name in an assignment — `value` in
  `value = 1` — is deliberately *not* one: it is exactly what a word-based provider finds, every
  editor with an expand-selection command merges one in, and Prism spells it across twenty node
  types with no accessor in common.
- **`foldingRange` and `selectionRange` are the only two requests that do not wait for the graph.**
  `Analysis::serve` settles before dispatching, and `needs_the_graph` exempts these two because
  `ranges` never sees a `Graph`. Measured on solargraph's 1,058-line `api_map.rb`, asking after
  every character of a new method: 14.3 ms a keystroke while settling, 8.0 ms without.
- **One chevron per line is all an editor can draw**, so two ranges opening on the same line are
  one range and one piece of noise. The widest wins — on `obj.items = list.map do |x|` the
  setter's argument list and the block both open there, and the outer one is what a reader
  clicking that line meant.
- **The drawings mark the source rather than assert line numbers**, as the highlight map and the
  signature card do. `┌ │ ┘` down the left of the fixture puts the rule you are most likely to
  break — the closer staying visible — where it can be *seen*: the line holding `end` carries no
  mark. `assert_eq!(range.end_line, 4)` can only be checked against the arithmetic that produced
  it.
