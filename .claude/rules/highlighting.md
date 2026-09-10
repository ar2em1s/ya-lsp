---
paths:
  - "src/analysis/highlight.rs"
  - "src/analysis/scopes.rs"
---

# Occurrence highlighting, and the scope walk under it

- **The scopes are Prism's; re-deriving them is the mistake to avoid.** Every local-variable node
  carries a `depth` — how many scopes out the name resolved — so two occurrences are the same
  variable exactly when they share a name and land on the same entry of the scope stack. That one
  indexing (`scopes[len - 1 - depth]`) is the whole logic: shadowing, a block closing over its
  surroundings, and a `def` that cannot see out of itself all fall out of it.
  `x = 1; [1].each { |x| x }` separates without this module knowing the word "shadow". Anything
  that starts to look like reimplementing Ruby's scoping means the depth is being ignored.
- **A superclass, a singleton's receiver and a definee are written *outside* the scope they open**,
  and Prism numbers their depth from the enclosing scope. `class Foo < v` reads the outer `v` at
  depth 0, so visiting it after the push resolves it against the class body and splits one variable
  in two. Hence the four scope openers hand-visit their children instead of deferring to
  `ruby_prism::visit_*_node`. A fixture that never inherits from an expression cannot see this.
  `class << o` and `def o.f` are the same shape.
- **An instance variable is scoped by what `self` is — an algebra, not a lookup.** `SelfContext` is
  a lexical path plus a `level` counting singleton steps above an *instance*: a namespace body is
  the class object (level 1), `class << self` adds a step, a receiverless `def` takes one away.
  That makes `def c` inside `class << self` land back on the class and share its `@v` with
  `def self.b`, while `def a` gets an instance's. Joining those is wrong in a way that reads as
  right. The path is the spelling as written, so a class reopened in one file keeps its instance
  variables together; `class << obj` and `def obj.f` get an island named by offset, because what
  the object *is* needs types.
- **The awkward spellings arrive already reduced, and there is a test rather than a comment.**
  `rescue => e`, a `case/in` capture and a destructured `def f(a, (b, c))` are ordinary target and
  parameter nodes; `_1` is a read of a local the block declares. Each looks like it needs its own
  case, which is why the fixture names them.
- **A keyword parameter's name span carries its colon** (`d:`); no other spelling does. Trimmed once
  in `record`, so the four keyword spellings cannot disagree.
- **`scopes::variable` hands back the name; `occurrences` is it minus the name.** The identity a set
  of occurrences shares already *is* the name, so a caller needing it has no reason to cut it out of
  the source at the first span. `rename` needs it, and needs it to carry an instance variable's `@`.
- **`crossing` and `accessor_site` are here for the same reason `writes_to` is: the algebra, not the
  caller.** `crossing` answers which locals a byte range reads before writing and whether anything
  it writes outlives it — what decides an extracted method's parameters — from the scope stack, so a
  block's `n` and the method's `n` are two variables. `accessor_site` answers where an `attr_`
  accessor for the instance variable at a cursor would go, returning `None` for a `def self.`, a
  `class << self`, an island and the top level, where an accessor would read a *different* variable,
  return `nil`, and look right. Both would otherwise need a second copy of `SelfContext`'s algebra
  in `code_actions`.
- **This module is on `COVERAGE_FLOORS` for rename, not for highlighting.** A scope bug that lights
  up the wrong occurrences is seen immediately; the same bug behind a rename writes over the wrong
  variable — and `code_actions` is a second caller writing off the same algebra.
- **`scopes` never sees a graph and `highlight` never sees syntax**, the same split as `cursor` and
  `completion`: the awkward positions are cheap to enumerate against nothing but a string.
- **The scope walk is asked first because it is the half that can say no.** It claims the cursor only
  when the cursor is really on a variable, and its `None` lets a constant or a call fall through to
  the graph. `@name = 1` is the one span both halves could answer for — rubydex records the
  assignment as a declaration and records no reference to one — and the walk has to win, because the
  graph would answer with the one place the variable is written and none of the reads, which is a
  highlight that looks like it worked.
- **`null`, never `[]`.** An empty list tells a client ya-lsp answered; `null` tells it nothing was
  known, which leaves the client's word matching in play for the comments and strings this
  deliberately says nothing about. Advertising the provider takes that fallback away everywhere
  else, so the two decisions are one.
- **Both test drawings mark the source rather than assert offsets.** A span one column out draws
  under the wrong text; a `{line, character}` pair in an `assert_eq!` shows nobody anything. Lines
  with nothing marked are dropped, so an assertion shows exactly what lit up — and the fixture's
  comment and string literal are visible in it as the two places nothing does.
