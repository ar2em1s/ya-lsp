---
paths:
  - "src/analysis/tokens.rs"
  - "src/server/capabilities.rs"
---

# Semantic tokens

- **The legend is the wire contract; it is asserted against `Kind`, not against a list.** A client
  reads each token's type as an *index* into the legend sent at initialize, so reordering `LEGEND`
  or `Kind` without the other recolours every token consistently and plausibly. Held by
  `tokens::tests::the_legend_is_the_wire` and
  `the_v0_4_0_semantic_token_legend_is_the_one_the_tokens_are_numbered_against`.
- **Three types, no modifiers.** Everything in the legend must be something the characters cannot
  say: `foo` alone is a local or a call and only a parse knows which. `@ivars`, `$globals`,
  constants, keywords, strings and numbers are lexical — the grammar has them right, and
  re-sending them changes nothing on screen. A modifier nothing sends is a promise nothing keeps.
- **Every call is sent, not only ambiguous ones.** Semantic tokens *replace* the grammar's answer
  wherever sent, so the set sent has to be consistent. `bar` in `foo.bar` coloured in one place and
  not another reads as a bug. Same argument puts `def render`'s name in the list.
- **Skipped: anything with no name to colour.** `a + b`, `list[0]` and `x <=> y` are method calls;
  colouring their operators is true, useless and ugly. The test is on the *first* character, so
  `empty?`, `save!` and `name=` keep theirs.
- **The order is the source's, and `of` sorts for that reason.** The wire format is deltas from the
  previous token, so one entry out of order displaces every colour after it. Prism's walk is close
  to source order and is not it: arguments come after the receiver, `rescue` after its body.
- **Lengths are counted in the negotiated encoding, never bytes.** `имя` is three characters of two
  bytes; `end - start` underlines six units where the client counts three. Conversion goes through
  `position::TextDocument`, which is why `tokens` returns byte spans and `analysis::mod` converts.
  An all-ASCII fixture cannot see this; `a_token_length_is_counted_in_the_encoding_the_client_negotiated`
  is not all-ASCII.
- **No delta, deliberately.** `semanticTokens/full/delta` optimises the wire for an answer computed
  anyway, and costs a per-document response cache plus an id to invalidate on every edit. A full
  answer over a very large file is a few milliseconds in release. Nothing to optimise yet.
- **The number that matters is what it costs the next request.** The run loop answers in order, and
  editors send this on every edit.
  `semantic_tokens_for_a_large_file_and_what_it_costs_the_request_behind_it` measures it by queuing
  a cheap request alongside before the thread sees either. That measurement is the precondition for
  shipping the request.
- **It does not wait for the graph.** `needs_the_graph` is false for it, beside `foldingRange` and
  `selectionRange`: `analysis::tokens` never sees a `Graph`, and the request fires on the first
  keystroke of a newly opened file — exactly when a cold index is still building.
