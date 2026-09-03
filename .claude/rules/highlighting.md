---
paths:
  - "src/analysis/highlight.rs"
  - "src/analysis/scopes.rs"
---

# Occurrence highlighting, and the scope walk under it

- **The scopes are Prism's, and re-deriving them is the mistake to avoid.** Every local-variable
  node carries a `depth` — how many scopes out the name resolved to — so two occurrences are the
  same variable exactly when they share a name and land on the same entry of the scope stack.
  That single indexing (`scopes[len - 1 - depth]`) is the whole of the logic: shadowing, a block
  closing over the locals around it, and a `def` that cannot see out of itself all fall out of it
  without a rule of their own. `x = 1; [1].each { |x| x }` separates with nothing in this module
  knowing the word "shadow". Anything that starts to look like reimplementing Ruby's scoping is a
  sign the depth is being ignored.
- **A superclass, a singleton's receiver and a definee are written *outside* the scope they open**,
  and Prism numbers their depth from the enclosing scope. `class Foo < v` reads the outer `v` at
  depth 0, so visiting it after the push resolves it against the class body and splits one variable
  in two. That is why the four scope openers hand-visit their children instead of deferring to
  `ruby_prism::visit_*_node`, and why a fixture that never inherits from an expression cannot see
  the bug. `class << o` and `def o.f` are the same shape.
- **An instance variable is scoped by what `self` is, which is an algebra rather than a lookup.**
  `SelfContext` is a lexical path plus a `level` counting singleton steps above an *instance* of
  it: a namespace body is the class object (level 1), `class << self` adds a step, and a
  receiverless `def` takes one away. That is what makes `def c` inside `class << self` land back on
  the class and share its `@v` with `def self.b`, while `def a` gets an instance's. Joining those
  is wrong in a way that reads as right. The path is the spelling as written, so a class reopened
  in one file keeps its instance variables together; `class << obj` and `def obj.f` get an island
  of their own named by offset, because what the object is needs types.
- **The awkward spellings arrive already reduced, and there is a test rather than a comment.**
  `rescue => e`, a `case/in` capture and a destructured `def f(a, (b, c))` are all ordinary target
  and parameter nodes; `_1` is a read of a local the block declares. Each of them *looks* like it
  needs a case of its own, which is why the fixture names them.
- **A keyword parameter's name span carries its colon** (`d:`) and no other spelling of the name
  does. Trimmed once in `record`, so the four keyword spellings cannot disagree.
- **`scopes::variable` is the entry point that hands back the name, and `occurrences` is it minus
  the name.** The identity a set of occurrences shares already *is* the name, so a caller that
  needs one has no reason to cut it out of the source at the first span and no absent case to
  handle if it does. `rename` needs it, and needs it to carry an instance variable's `@` — which
  is how it tells one without looking at the syntax a second time.
- **This module is on `COVERAGE_FLOORS` for rename rather than for highlighting.** A scope bug
  that lights up the wrong occurrences is seen the first time anybody looks at it; the same bug
  behind a rename writes over the wrong variable. `coverage.md`'s first question was `no` while
  this only drew, and v0.3.0's item 5 is what turned it.
- **`scopes` never sees a graph and `highlight` never sees syntax**, the same line `cursor` and
  `completion` are split along and for the same reason: the awkward positions are cheap to
  enumerate against nothing but a string.
- **The scope walk is asked first because it is the half that can say no.** It claims the cursor
  only when the cursor is really on a variable, and its `None` is what lets a constant or a call
  fall through to the graph. `@name = 1` is the one span both halves could answer for — rubydex
  records the assignment as a declaration and records no reference to one — and the walk has to
  win, because the graph would answer with the one place the variable is written and none of the
  places it is read, which is a highlight that looks like it worked.
- **`null`, never `[]`.** An empty list tells a client ya-lsp answered; a `null` tells it nothing
  was known, which is what leaves its own word matching in play for the comments and strings this
  deliberately says nothing about. Advertising the provider is what takes that fallback away
  everywhere else, so the two decisions are one decision.
- **Both test drawings mark the source rather than assert offsets**, as the signature card does. A
  span one column out draws under the wrong text and reads as the bug it is; a `{line, character}`
  pair in an `assert_eq!` shows nobody anything. Lines with nothing marked are dropped, so what an
  assertion shows is what lit up — and the fixture's comment and string literal are *visible* in it
  as the two places nothing does.
