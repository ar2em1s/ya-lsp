---
paths:
  - "src/analysis/search.rs"
  - "src/analysis/references.rs"
---

# Project-wide search and references

## "The user's code" has one definition

- **Ask `is_own_code` and never re-derive it.** The rule is written once, on
  `environment::Layout::is_own`: the workspace prefix, plus `own_prefixes`, minus `foreign_prefixes`
  (a vendored bundle sits inside the root, and a monorepo load path sits outside it).
- Callers: diagnostics, `references`, `workspace/symbol` ranking and `types::from_ancestor`.

## `workspace/symbol`

1. **The ranking is `search::rank`:** own code → tier → loadable → simple name length → full name
   length → name. Own code comes before match quality, because a bundle declares far more names
   than the project does. Never rank by `DeclarationId`, which is a hash and shuffles between runs.
2. **`loadable` ranks below the tier.** It sinks spec-only declarations and generator templates
   (`environment::placement`, `environment::Trees`). It must not bury an exact match, because the
   picker is how people find spec helpers.
3. **Drop exactly two kinds of row:**
   - A declaration written only in a document outside the project. Drop it before
     `select_nth_unstable`, so it frees a cap slot.
   - A generated row that the file's own declaration already covers. Key it on file, span and bare
     name. Never use "first seen": a `scope` is declared twice on purpose (`Story.recent` and
     `Relation#recent`), and `attr_accessor :x` gives `x` and `x=` at the same span.
4. **`PICKER` pins the order as whole lists.** Each adjacent pair differs in exactly one field.
   Changing any field means re-reading those lists.
5. **Never rank by rubydex's `match_score`.** It ties every hit. Keep `declaration_search` for its
   parallel filter, and order with `search::tier`.

## `references`

1. **Never fence it by test or migration trees.** Omitting uses in the suite breaks renames. Held by
   `a_use_in_a_spec_is_a_use_and_this_list_is_never_fenced`.
2. **Its scope is `scope_rooted_in`: `own_documents()` plus the document the question comes from.**
   No other outside document joins it (`environment::Outward::Alone`).
   `callHierarchy/incomingCalls` reads the same function.
3. **List both spellings for a method, and take them from the cursor's word, never from the
   resolution.** A call is `shout`, and the old name of an `alias` is `shout()`. `find` lists both;
   `calls_to` lists only the call.
   - `Foo.new` resolves to `initialize`, so taking spellings from the resolution would list every
     `initialize`.
   - The pair of tests: `an_alias_is_a_use_of_the_name_and_both_spellings_are_listed` and
     `hierarchy::an_alias_writes_the_name_down_and_is_not_an_incoming_call`.
4. **rubydex never links a method reference to a declaration** (`MethodDeclaration::references()`
   is always empty). Find calls by name. `Target::Call` skips resolution, so a `define_method` name
   still lists its call sites. Resolution only narrows `includeDeclaration` (`declaration_sites`).
5. **Filter out the synthetic `<Foo>` references** (`references::is_synthetic`). Otherwise
   `class << self` answers with `Person.new` in another file.

## Caps

- **`MAX_WORKSPACE_SYMBOLS` and `MAX_REFERENCES` are both needed**, and hitting either one tells
  the user. `MAX_REFERENCES` is hit often on real apps.
- **The truncation message names the file where the list stops.** `ordered` sorts by URI, so a cap
  drops every file after one point. Read the boundary from the last place that survives, not from
  the first one dropped.

## Mechanics

- **`Reference::write` marks a declaration, and the dedup ignores it** (URI + span only). Writes
  sort first.
- **`Ranges` reads and line-indexes each file once**, not once per span.
- **Cost:** `references` scales with the workspace, and `workspace/symbol` with the whole graph,
  gems included.
