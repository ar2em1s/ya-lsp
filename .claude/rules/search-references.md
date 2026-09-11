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
  `(own, tier, loadable, simple_len, name_len, name)`. Never by `DeclarationId`: it is a hash and
  would shuffle between runs.
- **`loadable` sinks a declaration only the suite loads, and it sits *below* the tier.** The term
  is `environment::placement`'s, taken from the same walk that answers `own` — two questions of one
  declaration's definitions, because asking them apart is two walks over six figures of candidates.
  Below the tier and not above it: a picker is how a name is *looked up*, so burying an exact match
  because it is a spec would break the only way of finding a spec helper, while below the tier it
  only rearranges candidates the query matched equally well. There is no cursor to turn it off
  with — `workspace/symbol` carries no document — which is exactly why this surface ranks instead
  of dropping. `environment.md` has the table. It sinks a **generator's template** by the same
  term and for a stricter reason — a spec at least loads under RSpec — and that half of
  `environment::Trees` is read of the whole graph rather than of `own`, because a gem's template
  is the common case. Measured over 247 picker queries on six corpora: **6 lists changed**, 5
  template rows sank, 2 fell past the 256-row cap with 2 real rows coming in behind them, and no
  real row was lost. `subtypes` over 480 lists moved **0 rows**: its only template rows are a
  template-only base class and its template-only subclass, which is the residue shape where
  sinking has nothing to sink below.
- **A generated row the file's own declaration already covers is dropped, and only a generated
  one.** ActiveRecord's query interface is declared once per relation class and once per base, and
  since each carries the place Rails really wrote, a query for `annotate` offered
  `ActiveRecordRelation#annotate`, `ActiveRecord::Base.annotate` and `Story.annotate` above
  `ActiveRecord::QueryMethods#annotate` — four rows opening one line, with the real declaration the
  one pushed out of the top ten. Over 360 queries on six corpora that was **489 repeated files
  inside a top ten becoming 708**; with the clause the picker is byte-identical to what it was
  before the query interface had places at all, **0 rows lost and 6 gained**.
  <br>**"Already covered" and not "already seen"**, which is narrower and the difference is a
  defect the first version had for ten minutes. A `scope` is declared **twice on purpose** —
  `Story.recent` and `Story::Relation#recent`, the same `scope :recent` line, two different
  receivers — and rubydex records no declaration at a `scope` call, so nothing else claims that
  span and both copies are the answer. Dropping by first-seen took **44 real rows** over the six
  corpora, every one of them a scope. The key is the file, the span **and the bare name**, because
  `attr_accessor :x` is `x` and `x=` at the same offsets.
- **`references` is never fenced by it, and a test says so.** A work list that quietly omitted the
  suite is a rename that breaks the suite, which is the same reason this request does not follow a
  derived receiver. `a_use_in_a_spec_is_a_use_and_this_list_is_never_fenced` exists to fail if
  anyone wires the fence in here.
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
