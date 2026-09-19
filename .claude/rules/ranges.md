---
paths:
  - "src/analysis/ranges.rs"
---

# Folding and expand-selection

## Folding

1. **Our folding provider replaces VS Code's indentation guess**, so every shape we miss is a
   fold that stops existing. Cover what the guess already handled (definitions, blocks, literals)
   and what it cannot see (heredoc bodies, `#region`, runs of comments, each branch on its own).
2. **Answer "nothing" with `null`, never `[]`.** `null` hands the fold back to the guess; `[]`
   removes the guess and puts nothing in its place.
3. **End every range on the last line of the body, never on the closer.** A collapsed `def` must
   leave its `end` visible. So a single-line construct produces no range. `line_fold` decides
   both, so syntax folds and comment folds cannot disagree.
4. **Two Prism locations overshoot the body**, and believing them hides the `end`:
   - the implicit `BeginNode` of a `def`, `class` or block that has a bare `rescue`
   - `ElseNode`, whose location includes the `end`

   Measure both from the line above their closer. The fixtures include both shapes.
5. **One chevron per line.** When two ranges open on the same line, keep the widest. On
   `obj.items = list.map do |x|` that is the block.

## Expand-selection

1. **Each link in the chain must contain the previous one.** Prism's error recovery hands out
   locations that don't, so build the chain by filtering with `locator::nests` rather than trusting
   the parser. A sweep types a file one character at a time and checks every position.
2. **The outermost link is always the whole buffer**, so `selection_range` answers every position.
   LSP matches chains to positions by index and has no way to say "not this one".
3. **The Ruby-specific steps:** a string's contents before its quotes, one argument before the
   argument list, a body before what opens it, and a message then its receiver before the next call
   in a chain. The name in `value = 1` is deliberately not a step, because editors already add it.
4. **Prism reports 13 node kinds through their typed `visit_*` method and never through
   `visit_branch_node_enter`.** `ArgumentsNode` and `StatementsNode` are among them.
   `selection_chain` overrides 12 of the 13. The 13th, `BlockArgumentNode`, is reachable only from
   Ruby that the parser rejects.

## Shared

- **`foldingRange` and `selectionRange` don't wait for the graph** (`needs_the_graph`). `ranges`
  never sees a `Graph`, and skipping the wait roughly halves what a keystroke costs while the
  graph is still settling.
- **`ranges.rs` is in `COVERAGE_FLOORS`**, because an untested construct shows up as a missing
  chevron, not as a visible bug.
- **Tests draw on the source with `┌ │ ┘` instead of asserting line numbers.** The line that holds
  `end` carries no mark, so a swallowed closer shows up in the drawing.
