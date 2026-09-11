---
paths:
  - "src/analysis/hierarchy.rs"
---

# Type hierarchy

- **Subtypes rank the application's own subclasses above the suite's doubles, and drop neither.**
  `(own, loadable, name)` — the second term is `environment::placement`'s and the argument is the
  cap: a base class the project subclasses twice is subclassed twenty times by doubles under
  `spec/`, and alphabetical order puts `FakeStore` above `Warehouse`. It sinks and never drops,
  because the double really does inherit and this list is read to find out what does. `supertypes`
  does not ask at all — a module a spec prepends really is in the chain.
- **`incomingCalls` is never fenced by a test tree, and a test says so.** It answers *where is this
  used*, so a call from a spec is a call; `a_call_from_a_spec_is_an_incoming_call_and_this_list_is_never_fenced`
  exists to fail if anyone wires the fence in. `environment.md` carries the whole table.
- **Subtypes are a set lookup, not a graph scan.** rubydex fills `Namespace::descendants()` as
  `Resolver::resolve` linearizes, and `propagate_descendants` handles the cached path too — so a
  file added later lands in every ancestor's descendant set without a rebuild. It is what makes
  `typeHierarchy/subtypes` answerable at all.
- **A declaration is in both its own `ancestors()` and `descendants()`, and in `ancestors()` it is
  not always first**: a `prepend`ed module precedes it, as Ruby reports. So drop the entry that
  *is* this declaration, never the head of the list. A fixture with no `prepend` cannot tell them
  apart.
- **`descendants()` can name a declaration the graph no longer holds.** `Graph::delete_document`
  removes the deleted class from its *own* ancestors' descendant sets, but some of those had their
  ancestors cleared first, so the removal misses them — measured: deleting a file defining
  `Leaf < Middle < Base` left `Leaf` in `Object`'s set and correctly removed it from `Base`'s.
  Look every id up rather than trusting it; that also covers a stale id from the client.
- **Both directions are the whole chain.** Supertypes are the linearization (what Ruby means by
  `ancestors`, already computed); subtypes are its mirror, because a chain one way and a generation
  the other is a tree whose two directions disagree about what a level means. Cost: a subtype
  appears at more than one depth. Stated in the README rather than filtered, as is `Comparable`
  being a supertype of `String`.
- **The cap is on the rows, not on the search.** Finding subtypes is a set lookup; *drawing* one
  reads the file each row lives in, so cost tracks rows drawn. With Ruby's signatures indexed an
  ordinary namespace is instant and only the very widest roots cost anything. `MAX_SUBTYPES` is
  2048: everything below `StandardError` is a real question with a real answer and ruled out a
  smaller cap, while a Rails bundle's widest roots are an order of magnitude past a plain Ruby
  project's.
- **`lsp-types` 0.97 has no `typeHierarchyProvider` field, and 0.97 is the newest published.**
  `capabilities::Advertised` flattens `ServerCapabilities` and adds the field beside it, keeping
  the contract typed rather than a `serde_json::Map` insertion. The test asserts a second, ordinary
  provider alongside it: a flatten that stopped flattening would announce the type hierarchy and
  nothing else.
- **An unresolved ancestor is a row, placed where the name is *written*.** A partial propagates down
  the chain — `Leaf`'s ancestors carry the `Missing::Thing` that `Middle` inherits — so `mention`
  searches every member of the chain, not just the class being expanded. The kind comes from how it
  was written (`<` takes a class, `include` a module); nothing else knows. The name is walked out of
  `Name::parent_scope` iteratively and keeps an explicit `::`, since `::Foo` failing where `Foo`
  would have resolved is frequently the reason.
- **`Object`, `Kernel` and `BasicObject` are in the chain only when signatures are.** rubydex keeps
  the five in a synthetic document `rubydex:built-in`, which `DocUri::from_uri_str` rejects for
  every request — so with `[rbs] enabled = false` those rows are dropped rather than sent as
  somewhere an editor cannot open. `workspace/symbol` and goto-definition already behaved this way.
- **Which definition of a reopened class a row points at is `locator::preferred_definition`**; whether
  any is the user's is `locator::declared_in`. Both were `search.rs`'s and are now shared — a class
  that opens in one file from the outline and another from the hierarchy is a bug. Subtype ranking
  is `search::rank`: own code first, then the name, never `DeclarationId`.
- **The rows are asserted as a drawing** — `class Base — base.rb` per line, like the signature card
  and the highlight map. The keyword makes the kind visible, which is where `module Comparable`
  among supertypes stops looking like a mistake; the file name separates a project's two rows from a
  bundle's eight.

# Call hierarchy

- **One containment question, asked from both sides.** Which `def`'s body holds this offset —
  innermost, which is the greatest start among those that contain it, because a nested definition
  starts after its parent and ends before it. `incomingCalls` asks it of a call site to find the
  caller; `outgoingCalls` asks it to *exclude* a call that belongs to a nested `def` rather than to
  the method being expanded. Two answers to it would let a call be both.
- **The bucket is a definition and never a declaration.** A class reopened in two files has two
  bodies, and `fromRanges` are drawn against the row's own file — bucketing by declaration would
  put one file's offsets on another file's text. It is also why `outgoingCalls` is addressed by the
  *position* in the item the client sends back rather than by its `data`: the declaration cannot
  say which of the two bodies was expanded, and an incoming row points at the body the calls were
  found in rather than at whichever `preferred_definition` prefers.
- **Incoming is matched by name, and every row says so.** rubydex links no method reference to a
  declaration, so `references::calls_to` matches the two spellings and nothing else — a caller of
  `call` is a caller of something spelled `call`. `textDocument/references` makes the same trade in
  a flat list; a *tree* implies an exactness it has not got, so the detail column of every caller
  ends in `by name`.
- **Outgoing is drawn only from `locator::precise_call`**, the gate `signatureHelp` fires from, and
  the asymmetry with incoming is the point: a work list may be over-broad and still be useful, an
  edge may not. The name rung would answer `person.name` with every method spelled `name` and draw
  forty edges out of one call site. The redirect is kept, so `Foo.new` is an edge to
  `Foo#initialize` — the method that actually runs.
- **A call written inside no `def` is attributed to the file.** A model's `has_many`, a class body's
  `include`, a `Rakefile`'s top level. The row is `SymbolKind::FILE` with no `data`, so expanding it
  answers nothing, and its span is the start of the file rather than the whole of it — a range
  covering the file makes an editor select all of it on click. Dropping those calls would empty the
  most useful answer this gives on a Rails application; attributing them to the enclosing class
  would say a class called something, which is not what an edge in a call graph means.
