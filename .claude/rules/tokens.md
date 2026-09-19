---
paths:
  - "src/analysis/tokens.rs"
  - "src/server/capabilities.rs"
---

# Semantic tokens

## Must

1. **Never reorder `LEGEND` or `Kind` on its own.** A client reads a token's type as an index into
   the legend it received at initialize, so a one-sided reorder recolours every token and still
   looks plausible. Held by `tokens::tests::the_legend_is_the_wire` and
   `the_v0_4_0_semantic_token_legend_is_the_one_the_tokens_are_numbered_against`.
2. **Send tokens in source order.** The wire format is deltas, so one token out of order shifts
   every colour after it. Prism's walk is close to source order but differs from it (arguments
   come after the receiver, and `rescue` after its body), which is why `of` sorts.
3. **Count lengths in the negotiated encoding, never in bytes.** `tokens` returns byte spans and
   `analysis::mod` converts them through `position::TextDocument`. Held by
   `a_token_length_is_counted_in_the_encoding_the_client_negotiated`, which is not all ASCII.
4. **Don't wait for the graph.** `needs_the_graph` is false for this request, as it is for
   `foldingRange` and `selectionRange`. The request fires on the first keystroke of a new file,
   which is exactly when a cold index is still building.

## What gets sent

- **Three types, no modifiers.** Send only what the characters cannot tell you: `foo` alone could
  be a local or a call. Leave `@ivar`, `$global`, constants, keywords, strings and numbers to the
  grammar.
- **Send every call, not only the ambiguous ones.** Semantic tokens replace the grammar wherever
  they are sent, so `bar` in `foo.bar` coloured in one place and not another reads as a bug. For
  the same reason, the name in `def render` is sent too.
- **Skip calls that have no name to colour.** `a + b`, `list[0]` and `x <=> y` are skipped. The
  test looks at the first character, so `empty?`, `save!` and `name=` keep their colour.

## Settled

- **There is no `semanticTokens/full/delta`.** It would need a per-document cache, invalidated on
  every edit. A full answer over a large file takes a few milliseconds in release.
- **What counts is the cost to the next request**, because editors send this on every edit.
  `semantic_tokens_for_a_large_file_and_what_it_costs_the_request_behind_it` measures it.
