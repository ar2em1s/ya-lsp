---
paths:
  - "src/analysis/highlight.rs"
  - "src/analysis/scopes.rs"
---

# Occurrence highlighting and the scope walk

## Locals: use Prism's `depth`, never re-derive scoping

1. **Two local-variable occurrences are the same variable exactly when they share a name and
   `scopes[len - 1 - depth]` is the same entry.** Shadowing, closures and a `def` that cannot see
   out all follow from that. If you find yourself reimplementing Ruby's scoping, you are ignoring
   `depth`.
2. **A superclass, a singleton's receiver and a definee sit *outside* the scope they open.**
   `class Foo < v` reads the outer `v`. So the four scope openers visit their children by hand
   before pushing the new scope.
3. **Trim the colon from a keyword parameter's name span (`d:`) once, in `record`.**
4. **`rescue => e`, `case/in` captures, `def f(a, (b, c))` and `_1` already arrive as ordinary
   nodes.** The fixture names each of them.

## Instance variables: scope by what `self` is

- **`SelfContext` is a lexical path plus a `level`.** A class body is level 1, `class << self`
  adds 1, and a receiverless `def` subtracts 1.
  - So `def c` inside `class << self` shares `@v` with `def self.b`, while `def a` gets the
    instance's `@v`.
  - `class << obj` and `def obj.f` get their own scope, named by offset.
- **`crossing` and `accessor_site` live here so the algebra has one copy.**
  - `crossing` gives the locals a range reads and the writes that outlive it. Extract-method uses it.
  - `accessor_site` says where an `attr_` accessor would go. It returns `None` wherever an accessor
    would read a different variable.

## Who answers a cursor, in order

1. **The scope walk first**, because it can say no. It claims only a real variable. For
   `@name = 1` it must win: the graph would return only the write and none of the reads.
   `definition` and `hover` ask the same walk through `locator::variable_at`, so a jump and a
   highlight never disagree.
2. **Then the graph (`locate`).**
3. **A macro's `:symbol` last.** rubydex does not record call arguments. Add the symbol's own span
   by hand, as a read, when the macro only *names* a method. Otherwise `definition` lands on a span
   that nothing highlighted.

## Settled

- **Answer "nothing" with `null`, never `[]`.** `null` keeps the client's word matching working in
  comments and strings.
- **`scopes` never sees a graph, and `highlight` never sees syntax.**
- **`scopes.rs` is in `COVERAGE_FLOORS` because of rename**, not highlighting. A scope bug behind a
  rename overwrites the wrong variable, and `code_actions` writes through the same logic.
- **Tests draw marks under the source instead of asserting offsets.**
