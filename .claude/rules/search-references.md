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
- **`Ranges` exists because a project-wide answer names many spans in few files.** Converting a
  span means reading and line-indexing its whole file; do that per span and a file with fifty
  references is read fifty times.
