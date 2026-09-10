---
paths:
  - "tests/**"
  - "src/main.rs"
  - "src/server/**"
---

# Benchmarking and manual smoke tests

How a claim about speed or degradation gets checked. None of this is needed to edit the crate.

`make canary` automates one run: it opens a pinned commit of a real Rails application and asserts
own-code indexing, diagnostics and a ceiling on the cold index, on every push. It does **not** cover
gems (that needs a `bundle install`), so gem directories, degraded Ruby versions and the steady
resolve are still done by hand, and a green canary says nothing about them. See `canary.md`.

## The corpora

Six real Rails applications, cloned into this repository's own `tmp/`:

| corpus | where it goes | why it is in the set |
|---|---|---|
| lobsters | `tmp/lobsters` | small, plain Rails; also the canary clone |
| chatwoot | `tmp/corpora/chatwoot` | an `enterprise/` overlay beside the app |
| discourse | `tmp/corpora/discourse` | the largest; plugins, engines, `isolate_namespace` |
| forem | `tmp/corpora/forem` | a whole vendored dummy application under `vendor/` |
| mastodon | `tmp/corpora/mastodon` | the most nested models and concerns |
| solidus | `tmp/corpora/solidus` | an engine monorepo, and no `db/schema.rb` at all |

Rules that hold for all of them:

- **Clone into `tmp/`, never elsewhere.** `tmp/` is gitignored and inside the repository, which is
  what keeps a corpus off the licence ledger (`canary.md`: cloned, never vendored) and lets a
  harness use paths relative to the repository root.
- **Pin the checkout and leave it alone.** Every count a sweep reports is a property of a commit. A
  stray hand edit shifts line numbers and reads as a regression on the next diff — check the corpus
  is clean *before* measuring, not after a result looks strange.
- **The six differ in shape, and that is the point.** A rule that joins paths rather than matching a
  suffix silently measures a different part of solidus than of the other five; a stratum has to be a
  *list* of spellings, because a background job is `app/jobs` in three of them and `app/workers` in
  two. A change that looks free on lobsters is not measured until it has been asked of a corpus
  whose shape it did not anticipate.
- **Bundles are only partly installed, and that state changes between sessions.** Re-check what is
  actually installed before quoting any absolute number, and never compare an absolute taken today
  against one taken on another day. A *diff* between two binaries in one pass survives this; an
  absolute does not.
- **Do not sweep the largest corpus.** It costs a large multiple of the others for the same verdict.
  Measure a change over the other five; a static count over all six is cheap and fine.

## Tier sweeps

A change to what ya-lsp can type is measured as "positions that change tier": one
`textDocument/hover` at every `.member` position of a corpus, asked of the binary before the change
and the binary after. The harness lives in `tmp/bench/` and is gitignored:

```bash
bash tmp/bench/run.sh <tag> tmp/corpora/<corpus> 1 <before> <after>
python3 tmp/bench/tierdiff.py tmp/bench/<tag>-<before>.json tmp/bench/<tag>-<after>.json <corpus>
python3 tmp/bench/atpos.py <binary> <corpus> '<relpath>:<line>:<col>:<member>'
```

- **A hover round trip costs about as much as the hover.** A client asking one at a time spends most
  of its wall clock waiting. Keep a window of requests in flight and match replies **by id** —
  reading until you see the id you want and dropping the rest is a bug that only shows up once
  anything is pipelined.
- **Sweep every binary in one pass.** One file walk, one settle, and — correctness, not speed — both
  sides provably see the same bytes. Sharding across processes is sound and bought nothing on a
  loaded machine; it also makes the settle harder, because `n * sides` servers walk a cold workspace
  at once.
- **`.member` is not the only kind of position; an item measured at the wrong ones measures zero.**
  The default pattern asks after a `.`; route helpers and class-body macros are *receiverless* calls
  it never reaches, so each has its own pattern (`--helpers`, `--macros`). Before believing a zero,
  check the sweep asks where the change acts — a concern-edge change moves a few hundred `.member`
  positions and several times that on the pattern written for it.
- **A completion list is not a card, so it needs its own scale and harness.** A hover answers with a
  tier; a list answers with a *set* in an *order*, and a gap can be that the set is empty.
  `csweep.py` asks the question the corpus marks for itself — the file wrote `validates :title`, so
  with `vali` typed the answer is whether `validates` comes back and at what **rank** (1 = top,
  0 = absent). It shares `sweep.py`'s `Client`, `settle` and `positions`, so there is one settle
  rule. **The typed prefix is what makes it affordable**: an unfiltered list is capped at 512 rows
  and tens of kilobytes per position; four characters cut it to a few dozen.
- **Observe the settle; do not sleep it.** `scripts/canary.py`'s quiet-period rule works here
  unchanged: read until the server has said nothing for a few seconds. Every corpus settles in a
  fraction of the time the sweeps used to sleep.
- **Silence at the *start* is not the end of the work.** A server says nothing while it walks a cold
  workspace, so a quiet-period rule timing from `initialized` reads that silence as "settled". On
  the largest corpus every shard reported `settled after 0.0s` and began asking before the bundle
  was in. The comparison survived — both binaries asked the same question at the same moment — but
  the *absolute* tiers of the earliest files did not: over a thousand positions read as regressions,
  every one in the **first decile of the ask order**, answering identically when re-asked of a
  settled server. Wait for the first thing the server says **and then** for quiet.
- **Better than either: wait for the progress stream to close.** `$/progress` carries an explicit
  `begin` and `end`, so while a token is open the server is working *however long since it last
  spoke* — a statement where quiet is an inference, and one that covers a gap
  `progress::MIN_REPORT_INTERVAL` is free to make. A server that opens no stream falls through to
  first-word-then-quiet rather than waiting for what will not come; waiting for one to *appear*
  would refuse a `gems.enabled = false` workspace to guard a gap that measures as nothing.
- **A measurement that cannot prove it settled must fail, not warn.** The numbers an unsettled sweep
  writes are not obviously wrong — a little low, in whichever files it reached first — and telling
  them from a real regression costs a day. `sweep.py` exits non-zero at the ceiling, naming the
  condition it was waiting on.
- **What the server says about the corpus belongs with the numbers.** A sweep discarding
  `window/showMessage` throws away lines saying how much of a `Gemfile.lock` is actually installed
  anywhere ya-lsp looked. A *diff* survives that; no absolute count does. Warnings print after the
  settle, travel in the JSON, and are reprinted by `tierdiff.py`.
- **Classify by what the answer says, and version the classifier.** `sweep.py`'s `tier()` once read
  `*Defined in N places.*` as the name-based candidate list. It is neither: it counts *places* for a
  declaration already resolved, `hover::card` prints it above the footnotes saying how the type was
  found, and a card at any tier can carry it — so a change giving already-typed positions a second
  place reports every one as a regression on a card that strictly gained a line. Only
  `hover::candidate_list`'s `**N possible definitions**` means the name rung. A sweep stores the
  *verdict*, not the card, so it can never be reclassified afterwards: the JSON carries a `tiers`
  version and `tierdiff.py` refuses two that disagree. Blast radius measured, not assumed — the
  misclassification ran a couple of percent of positions, every one downward.
- **Read a position before believing a diff.** `atpos.py` asks one binary for the card at an exact
  sweep key, the same string `tierdiff.py` prints, so a position a diff calls better or worse can be
  read rather than inferred. It takes **absolute** paths: a relative corpus path builds a URI that
  matches no document the server indexed and prints `nothing` at every key, which looks exactly like
  a feature that does not work.
- **The shell will not split a pair for you.** zsh does not word-split an unquoted variable, so
  `set -- $pair` leaves `$2` empty and the measurement runs against the wrong directory and reports
  a plausible zero. Split explicitly, or pass two arguments.

## Manual smoke test

```bash
cargo build --release
python3 <driver> <repo> target/release/ya-lsp
```

The driver speaks LSP over stdio: `initialize`, `initialized`, collect
`textDocument/publishDiagnostics` until quiet, then `didOpen` real files and ask `documentSymbol` /
`hover` / `definition` at their symbol positions. Read messages on a thread with a timeout — a
blocking read with no timeout hangs the moment the server has nothing left to say. Advertise
`hierarchicalDocumentSymbolSupport` and `definition.linkSupport` in `initialize`, or the server
correctly answers with the flat pre-3.10 shapes.

**Completion: the number is typing latency, not request latency.** Send a `didChange` and a
`textDocument/completion` for every character of a phrase, as an editor does. Measure at least one
of each context — an expression, `Foo::`, `Foo.`, an argument list, a receiver with no type —
because their costs differ and only the last scales with the workspace. Use a buffer the disk has
never seen, since that is the situation completion always runs in.

**Signature-source configurations: the number is the steady resolve**, not end-to-end latency. Run
with `YA_LSP_LOG=ya_lsp=debug`, wait for the `$/progress` end, then send a few dozen `didChange` +
`completion` pairs and take the median of the `resolved in ...` lines. Compare configurations in one
process each (`[rbs] enabled`, `[rbs] stdlib`, `[gems] default_gems`) and warm the page cache first,
or the config that runs first pays for the disk.

**`references` and `workspace/symbol` need two workspaces, not one.** A real Rails app is the
*small* case for `references` — its cost is the size of the user's own code. Build the worst case:
point the workspace root at a whole gem directory (whatever `gem env gemdir` names, then its
`gems/`) with a `ya-lsp.toml` disabling gems and raising `index.max_files`, so every file counts as
the user's own. That is where `MAX_REFERENCES` is reached and the linear scaling is visible.

**Gem indexing:** advertise `window.workDoneProgress`, **answer the server's
`window/workDoneProgress/create` request**, and wait for `$/progress` `end` before asserting a gem
is navigable — it is background work, and a request racing it correctly answers `null`. To test the
"no Ruby" claim honestly, point `PATH` at an empty directory: macOS keeps a stub `ruby` in
`/usr/bin`. Drive against two real Rails apps: one with a fully installed bundle and one whose Ruby
version is *not* installed, which is the degraded path.
