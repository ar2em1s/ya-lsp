---
paths:
  - "src/analysis/search.rs"
  - "src/analysis/references.rs"
---

# Project-wide search and references

- **`is_own_code` is the one definition of "the user's code", and three features turn on it.**
  Diagnostics, `references`, and `workspace/symbol`'s ranking all ask it. It is the workspace URI
  prefix *minus* `gem_prefixes`, because a vendored bundle sits inside the workspace root. Any new
  "is this theirs?" test must go through it rather than re-deriving the prefix.
- **`workspace/symbol` ranks the user's own code above match quality, deliberately.** A project has
  ~3k declarations and its bundle ~150k, so ranking them together fills the picker with gems:
  measured, `"user"` returned four exact-matching gem methods above the project's own `Users`.
  `search::rank` is `(own, tier, simple_len, name_len, name)` — never by `DeclarationId`, which is
  a hash and would shuffle between runs.
- **`rank`'s order is pinned as a whole list, not as a set of rules.** `PICKER` is built so that
  each adjacent pair in its `user` list separates exactly one field — `User`/`Admin::User` only by
  qualified length, `#user`/`#user_name` only by simple length, `USER_LIMIT` only by case,
  `SuperUserPolicy` only by where the match falls — and the gem's exact `UserAgent` sits last,
  under a subsequence match in the project, which is `own` stated as an observation rather than a
  claim. The rows carry the file name for that reason: `own` is the first field the sort reads and
  is invisible in a list of names. A one-letter query gets its own pinned list, because that is
  the query a picker really receives first. Changing any field means re-reading these lists, which
  is the point of pinning them whole.
- **rubydex's fuzzy score is unusable for ranking.** `match_score` returns `query.len()` for every
  match and 0 otherwise, so every hit ties. `declaration_search` is kept for its parallel filter;
  the ordering is `search::tier`.
- **Both caps are load-bearing and measured.** `MAX_WORKSPACE_SYMBOLS` exists because a subsequence
  match on one character hits most of a bundle on every keystroke; `MAX_REFERENCES` exists so a
  name-based match cannot send a multi-megabyte response, and reaching it tells the user, because
  a truncated find-all-references looks exactly like a complete one.
- **`references` costs the size of the workspace, not of the answer** (0.4 ms at 161 files, 53 ms
  at 17,557), and `workspace/symbol` costs the size of the whole graph, gems included.
- **rubydex never links a method reference to a declaration.** `record_resolved_reference` handles
  constants only, so `MethodDeclaration::references()` is always empty. Do not "fix" the
  name-based path by routing it through the resolution — and `Target::Call` deliberately skips the
  resolution entirely, so a `define_method`'d name with no declaration still lists its call sites.
- **The synthetic `<Foo>` references reach `references` too.** A cursor on `class << self` resolves
  to the singleton declaration, which is where rubydex files the fabricated references. Without
  `references::is_synthetic` that answers with the span of `Person.new` in another file.
- **`Reference::write` says where a name is *declared*, and the dedup deliberately ignores it.**
  `references` has no use for the distinction and `documentHighlight` draws it, so it is decided
  once here rather than twice. One span reached both as a declaration and as a reference is one
  place and it is the declaration, which is why the sort puts writes first and the dedup compares
  only the uri and the span — comparing `write` too would emit the same location twice for every
  caller. It is compared as one tuple rather than a chain of `&&` because this file is held at
  100% of branches and three short-circuit arms are three arms to have to reach.
- **`Ranges` exists because a project-wide answer names many spans in few files.** Converting a
  span means reading and line-indexing its whole file; do that per span and a file with fifty
  references is read fifty times.
