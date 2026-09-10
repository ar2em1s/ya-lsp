---
paths:
  - "src/analysis/search.rs"
  - "src/analysis/references.rs"
---

# Project-wide search and references

- **`is_own_code` is the one definition of "the user's code".** Diagnostics, `references` and
  `workspace/symbol`'s ranking all ask it. It is the workspace URI prefix *minus* `gem_prefixes`,
  because a vendored bundle sits inside the workspace root. Any new "is this theirs?" test goes
  through it rather than re-deriving the prefix.
- **`workspace/symbol` ranks own code above match quality.** A bundle declares orders of magnitude
  more names than the project does; ranked together the picker fills with gems — measured, `"user"`
  put exact-matching gem methods above the project's own `Users`. `search::rank` is
  `(own, tier, simple_len, name_len, name)`. Never by `DeclarationId`: it is a hash and would
  shuffle between runs.
- **`rank`'s order is pinned as a whole list, not as rules.** `PICKER` is built so each adjacent
  pair in its `user` list separates exactly one field — `User`/`Admin::User` by qualified length,
  `#user`/`#user_name` by simple length, `USER_LIMIT` by case, `SuperUserPolicy` by where the match
  falls — with the gem's exact `UserAgent` last, under a subsequence match in the project. Rows
  carry the file name because `own` is the first field the sort reads and is invisible in a list of
  names. A one-letter query has its own pinned list, being the query a picker really receives
  first. Changing any field means re-reading these lists.
- **rubydex's fuzzy score is unusable for ranking.** `match_score` returns `query.len()` for every
  match and 0 otherwise, so every hit ties. `declaration_search` is kept for its parallel filter;
  ordering is `search::tier`.
- **Both caps are load-bearing and measured.** `MAX_WORKSPACE_SYMBOLS`: a one-character subsequence
  match hits most of a bundle on every keystroke. `MAX_REFERENCES`: a name-based match would
  otherwise send a multi-megabyte response. Reaching either tells the user — a truncated
  find-all-references looks exactly like a complete one.
- **Cost.** `references` costs the size of the workspace, not of the answer, and scales linearly
  with it. `workspace/symbol` costs the size of the whole graph, gems included.
- **rubydex never links a method reference to a declaration.** `record_resolved_reference` handles
  constants only, so `MethodDeclaration::references()` is always empty. Do not route the name-based
  path through the resolution. `Target::Call` skips the resolution entirely, so a `define_method`'d
  name with no declaration still lists its call sites.
- **`includeDeclaration` for a call lists fewer places than the name-based search finds**, and that
  is the only thing `Target::Call` reads from the resolution. Call sites are still found by name;
  `declaration_sites` takes the resolution's declarations, and for a call on a *class object* those
  are narrowed — a concern's `ClassMethods` resolves exactly, and a candidate owned by a `class` is
  dropped as unreachable (`navigation.md` has the argument). It keeps a work list for `scope` from
  offering a routing method's declaration.
- **The synthetic `<Foo>` references reach `references` too.** A cursor on `class << self` resolves
  to the singleton declaration, where rubydex files the fabricated references. Without
  `references::is_synthetic` that answers with the span of `Person.new` in another file.
- **`Reference::write` says where a name is *declared*, and the dedup ignores it.** `references` has
  no use for the distinction and `documentHighlight` draws it, so it is decided once here. One span
  reached both ways is one place and it is the declaration — hence writes first in the sort, and a
  dedup comparing only uri and span. Comparing `write` too would emit each caller twice. It is one
  tuple rather than a chain of `&&` because this file is held at 100% of branches.
- **`Ranges` exists because a project-wide answer names many spans in few files.** Converting a span
  means reading and line-indexing its whole file; per span, a file with fifty references is read
  fifty times.
