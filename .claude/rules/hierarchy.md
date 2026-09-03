---
paths:
  - "src/analysis/hierarchy.rs"
---

# Type hierarchy

- **rubydex maintains a reverse index and the v0.3.0 plan said it did not.** `Namespace::
  descendants()` is filled as `Resolver::resolve` linearizes — `propagate_descendants` handles the
  cached path too, so it is correct incrementally: a file added later put its class into the
  descendant set of every ancestor without a rebuild. That is what turns `typeHierarchy/subtypes`
  from the capped whole-graph scan the plan budgeted for into a set lookup, and it is why ya-lsp
  answers a request ruby-lsp returns `nil` for with a comment saying its index cannot.
- **A declaration is in both its own `ancestors()` and its own `descendants()`**, and in
  `ancestors()` it is **not always first**: a `prepend`ed module precedes it, exactly as Ruby
  reports. So the drop is of the entry that *is* this declaration, never of the head of the list.
  A fixture with no `prepend` in it cannot tell the two apart.
- **`descendants()` can name a declaration the graph no longer holds.** `Graph::delete_document`
  removes the deleted class from the descendant set of each of *its own* ancestors, but some of
  those had their ancestors cleared first, so the removal misses them — measured: deleting a file
  defining `Leaf < Middle < Base` left `Leaf` in `Object`'s set and correctly removed it from
  `Base`'s. Every id is looked up rather than trusted, which is also what covers a stale id
  arriving from the client.
- **Both directions are the whole chain, deliberately.** Supertypes are the linearization because
  it is what Ruby means by `ancestors` and rubydex has already computed it; subtypes are its mirror
  because a chain one way and a generation the other is a tree whose two directions disagree about
  what a level means. The cost is that a subtype appears at more than one depth, and it is stated
  in the README rather than filtered — as is `Comparable` being a supertype of `String`, which is
  correct Ruby and reads as a bug to anyone expecting single inheritance.
- **The cap is on the rows, not on the search.** Finding subtypes is a set lookup; *drawing* one
  means reading the file each row lives in. Measured on solargraph with Ruby's signatures indexed:
  0.07 ms median over 899 of its own namespaces, 4.3 ms for `StandardError`'s 458 rows, 25.1 ms
  for `Object`'s 1,978. `MAX_SUBTYPES` is 2048 because 458 is a real question with a real answer —
  that is what ruled out 512 — and because a Rails bundle's three roots are an order of magnitude
  past solargraph's.
- **`lsp-types` 0.97 has no `typeHierarchyProvider` field, and 0.97 is the newest published.**
  `capabilities::Advertised` flattens `ServerCapabilities` and adds the one field beside it, so the
  contract stays typed rather than becoming a `serde_json::Map` insertion. The test asserts a
  second, ordinary provider alongside it: a flatten that stopped flattening would announce the type
  hierarchy and nothing else.
- **An unresolved ancestor is a row, and it is placed where the name is *written*.** A partial
  propagates down the chain — `Leaf`'s ancestors carry the `Missing::Thing` that `Middle` inherits
  from — so `mention` searches every member of the chain rather than only the class being expanded.
  The kind comes from how it was written, because nothing else knows: `<` takes a class and
  `include` takes a module. Its name is walked out of `Name::parent_scope` iteratively and keeps an
  explicit `::`, since `::Foo` failing where `Foo` would have resolved is frequently the reason and
  this row exists to be read.
- **`Object`, `Kernel` and `BasicObject` are in the chain only when signatures are.** rubydex
  carries the five of them in a synthetic document called `rubydex:built-in`, which `DocUri::
  from_uri_str` rejects for every request alike — so with `[rbs] enabled = false` those rows are
  dropped rather than sent as somewhere an editor cannot open. `workspace/symbol` and
  goto-definition have always behaved this way; the hierarchy inherits it rather than deciding it.
- **Which definition of a reopened class a row points at is `locator::preferred_definition`**, and
  whether any of them is the user's is `locator::declared_in`. Both were `search.rs`'s and are now
  shared, because a class that opens in one file from the outline and another from the hierarchy is
  a bug and not a style choice. The subtype ranking is `search::rank`'s decision for its reason:
  own code first, then the name, never the `DeclarationId` — that is a hash, and a tree that
  reshuffles between runs is unreadable.
- **The rows are asserted as a drawing**, `class Base — base.rb` a line, like the signature card
  and the highlight map. The keyword makes the kind visible, which is where `module Comparable` in
  a list of supertypes stops looking like a mistake, and the file name is what separates a
  project's two rows from a bundle's eight.
