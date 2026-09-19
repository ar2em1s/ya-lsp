---
paths:
  - "src/analysis/hierarchy.rs"
  - "src/analysis/requests.rs"
---

# Type hierarchy, implementations and call hierarchy

## Shared rules

1. **One ranking: `hierarchy::below`**, used by both `subtypes` and `implementations`.
   - Own code first. Suite doubles are sunk (`environment::placement`), never dropped.
   - A subclass written outside the project is dropped. Capped at `MAX_SUBTYPES` (2048).
   - Held by `a_subclass_written_outside_the_project_is_dropped_where_the_suites_is_only_sunk`.
2. **Never fence `incomingCalls` by test trees.** A call from a spec is a call. Held by
   `a_call_from_a_spec_is_an_incoming_call_and_this_list_is_never_fenced`.
3. **Only the two `prepare`s build a `Fence::uses`**, through `locator::resolve`: the tree fence is
   off and the outside fence is on. The ranked lists and item builders read `indexed::Placed`.
4. **Pick a reopened class's row with `locator::preferred_definition`, and test ownership with
   `declared_in`.** The one outside document allowed in is `hierarchy::rooted_in`, the document the
   request is rooted in (`environment.md`).
5. **Tests draw rows as `class Base — base.rb`**, one per line.

## Type hierarchy

- **Subtypes are a set lookup** (`Namespace::descendants()`), kept current by rubydex as it
  resolves.
- **Remove the entry that *is* this declaration, never the head of the list.** A declaration
  appears in its own `ancestors()` and `descendants()`, and a `prepend`ed module comes before it.
- **Look up every descendant id; never trust one.** `delete_document` can leave stale ids behind,
  and the client can send stale ones.
- **Both directions show the whole chain**: supertypes are the linearization, and subtypes mirror
  it. A subtype can appear at several depths, and `Comparable` is a supertype of `String`. The README
  says both.
- **The cap limits rows drawn, not the search.** Above `MAX_SUBTYPES`, warn
  (`subtypes_truncated`).
- **An unresolved ancestor is a row, placed where its name is written.** `mention` searches the
  whole chain. The kind comes from `<` versus `include`. The name keeps an explicit `::`.
- **`Object`, `Kernel` and `BasicObject` appear only with signatures on.** Without them they live in
  `rubydex:built-in`, which `DocUri::from_graph_uri` rejects, so they are dropped.
- **`lsp-types` 0.97 has no `typeHierarchyProvider`.** `capabilities::Advertised` flattens
  `ServerCapabilities` and adds the field. The test asserts a second provider too, so a broken
  flatten fails.

## Implementations

1. **List the definition first, then the overrides.** Claude Code prints `null` as "No definition
   found … external library". An unoverridden concern method answers with its own `def`.
2. **Look for overrides below the receiver, never below the declaration.** `story.save` is declared
   on `Persistence`, below which sits every model. `Resolution::receiver` is set only on a precise
   answer. Otherwise use the declaration's owner.
3. **A guessed receiver answers `null`.** The gate is `requests::jumpable`, a single `match` on
   `Tier` shared with `typeDefinition` and `declaration`. Hover still answers, with its footnote.
4. **Ask `Foo.new` below `Foo`, not below `Foo::<Foo>`.** The `NEW` arm of `resolve_call` sets
   `receiver` to `attached_class`.
5. **Each goto has its own `linkSupport` flag.** Claude Code sets it for `definition` only.
   `ClientSupport` holds one field per goto, and `requests::goto_response` takes the flag as an
   argument.

- **The member test runs twice, and neither copy is dead.** `admits` runs before ranking, so the cap
  isn't filled by `Object`'s descendants. The second copy projects each descendant to its own
  member.
- **`reachable` is wider than `namespace`**: it also admits singleton and anonymous classes, so
  `def self.build` overrides are found. `Namespace::Todo` is in neither.
- **There is no truncation message.** 2048 overrides of one name has never been reachable.

## Call hierarchy

1. **One containment question, asked from both sides:** which `def` innermost holds this offset.
   Incoming uses it to find the caller, and outgoing uses it to exclude nested `def`s.
2. **Bucket by definition, never by declaration.** A reopened class has two bodies, and
   `fromRanges` belong to one file. So `outgoingCalls` reads the item's position, not its `data`.
3. **Incoming matches by name, and every row's detail ends with `by name`.** `calls_to` passes
   `Spellings::Called`, so alias lines are not callers (`search-references.md`). Held by
   `an_alias_writes_the_name_down_and_is_not_an_incoming_call`.
4. **Outgoing draws only from `locator::precise_call`**, the same gate `signatureHelp` uses. An edge
   must be exact. `Foo.new` → `Foo#initialize`, with `locator::Blocks` passed down so that a gem's
   `class_eval` `initialize` on `Object` is not an edge (`navigation.md`).
5. **A call inside no `def` belongs to the file.** It becomes a `SymbolKind::FILE` row with no
   `data`, spanning the start of the file only.
