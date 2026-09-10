---
paths:
  - "src/analysis/hierarchy.rs"
---

# Type hierarchy

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
