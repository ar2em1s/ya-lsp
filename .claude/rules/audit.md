---
paths:
  - "scripts/audit/**"
  - "audit/**"
  - "Makefile"
---

# The audit

`make audit` opens the six corpora, asks a stratified sample of real cursors, and scores the
answers.

- **It measures whether an answer is right, never how fast.** Only the budget is timed.
- **It is a measurement, not a gate.** No target fails on a number.

## Rule 1: nothing from a corpus is committed

`audit/ledger.json` and `audit/baseline.json` are the only files this package writes into the repo
(`corpora.md`).

1. **Ledger rows carry `sha256(line)[:16]`**: never the line, never the identifier under the
   cursor.
2. **Baseline rows carry integers, a git SHA and each finding's `audit.site`**, never a `detail`
   string.
3. **Counter *names* are built only from closed vocabularies.** `residue-signatures` is
   `shape/tier/places`. Never key a counter on a word, a path or a class name.
4. **Both writers rebuild rows from a field list** (`ledger.FIELDS`, `baseline.FIELDS`). A new field
   goes into that list, or it is not written.
5. **Paths and line numbers are fine.** The terminal may show the identifier.

## `server.toml`: configure over the wire, never in the clone

- **Settings go as `initializationOptions`.** A `ya-lsp.toml` in a clone makes it dirty, and
  `check_clean` refuses to measure a dirty clone.
- **Use the server's spellings** (`PartialConfig`). One unknown key rejects the whole layer.
- **`[log] file_path` resolves against the ya-lsp checkout.** `cmd_score` empties it once per run.
  The server itself never truncates the log.
- **The client counts `window/logMessage` at error severity**, keeping the first line only.

## When a server is ready (`client.settle`)

1. **The server has said something.** Silence before any work is not readiness.
2. **No `$/progress` stream is open**: neither gem indexing nor `ya-lsp/generate`.
3. **Then 3 s of quiet.**

A new stage that can run silently needs a progress stream, not a longer quiet period.

## `audit.site`: one position, one spelling

- **Use `path:offset`, relative to the corpus.** Every lane, the ledger and the baseline key on this
  exact string.
- **Never use a draw index, and never `path:line`.** Two shapes can share one line.
- **Offsets are code points, so the client asks for `utf-32`** and refuses any other encoding.
  Anything else poses cursors inside the wrong word, and lane 2 then reports a correct answer to the
  wrong question.

## Three lanes: which one a new check goes in

| Lane | Says | Needs |
|---|---|---|
| 1: keys (`lane1/`) | *wrong*: the corpus writes the right answer down | the source |
| 2: checks (`lane2/`) | *inconsistent*: two answers contradict each other | nothing |
| 3: ledger | *a person looked* | a person, once |

1. **Can the corpus state the answer mechanically?** Then it is a key.
2. **Do two answers about the same cursor contradict each other?** Then it is a check.
3. **Neither?** Then it is residue, for the ledger.

- **A check compares only answers to the same question.** When a pairing breaks (a jump to an ivar's
  assignments against a card about its class), count the case apart under its own name. Never
  narrow a check silently.
- **A check that needs an opinion is not a lane-2 check.** Opinions live in keys or in the ledger.
- **The report has no direction table.** The check that owns a counter says which way is good.
- **Over-blocking is safe in a key and unsafe in a draw filter.** `routes.non_helpers` is
  deliberately the weaker filter.
- **A parameter is a local only in its own file** (`routes.parameters`, read by `shapes.find`), never
  corpus-wide: solidus' admin component takes `account_path:` while its storefront calls the helper.
  Six `route` rows were a method's or a block's parameter (2026-09-29).

## The checks and keys

| # | Check (`lane2.CHECKS`, in order) |
|---|---|
| 1 | `highlight`: `documentHighlight` and `definition` agree about the cursor |
| 2 | `resolved`: a sure card (any card not saying it guessed) never points outside loaded code |
| 3 | `spans`: `hover` and `definition` agree on the span |
| 4 | `footnotes`: "Defined in N places" is true (ivars counted apart as `places-of-a-variable`) |
| 5 | `rebase`: same answer after an untouching `didChange` (edits sent in-stream) |
| 6 | `references`: vs `documentHighlight` in-file (both ways), vs `definition`'s places, cursor in own list |
| 7 | `margins`: the margin types a binding that the card at a call on it guesses at, with no type `typeDefinition` reaches |
| 8 | `incoming`: every incoming call site is in `references` |
| 9 | `renaming`: the rename box is the lit word (constants only; `rename-qualified` counted apart) |

Check 6 was `receiver` until 2026-09-29: it read which of the card's four guess sentences a card
carried, and the card now has one. The checks after it moved up one.

Keys (`lane1.KEYS`): `neutral`, `rails`, `completion`, `calls`, `closures`, `outline`.

## Adding a check, a key or a corpus

1. **Check:** a file in `lane2/` plus one name **appended** to `lane2.CHECKS`. The order is the
   numbering people remember. Hooks:
   ```
   counters() -> dict                   the counter keys it owns
   check(row, place, counts, findings)  one position; append (kind, site, detail)
   line(counts) / summary(counts)       its report lines
   under / breakdown / fold             optional
   FINDINGS                             kinds it raises, in print order
   POST                                 optional: one request per eligible position
   ask(client, corpus, drawn, answers, opened)  optional: conversations a Post can't express
   ```
   - A `Post` declares its params, legal shapes and stride in the check's own file.
   - `ask` runs after `ask_all` and before `ask_rebased`.
   - Every finding kind must be in `report.kinds()`, or it is silently dropped.
2. **Key:** a file in `lane1/`, plus `NAME`, `TOTAL`, `ASKS`.
   - `ASKS = False`: grades replies already collected.
   - `ASKS = True`: poses its own requests, and must run before `ask_rebased`.
   - A key that asks a second question does not fill `covered`.
3. **Corpus:** `scripts/corpora.toml` only. This package never names a corpus.

## The order in `measure()`: a constraint

```
ask_all        the sample's questions; every sampled document is opened first
lane1.asked    every ASKS key, BEFORE the rebase pass
ask_rebased    re-asks over a graph one edit behind, editing in-stream
client.stop()
lane2.run
lane1.graded
lane3.run      last: residue is what the other lanes left
```

- **Share one `opened` set.** A second `didOpen` is illegal.
- **The differ subtracts `answers.Shifts` at each position's own index.**

## The tiers are what the audit is for

- **Two tiers, as the card has** (decided 2026-09-29): `guessed` where the card carries
  *Guessed from name alone.* (`answers.GUESSED`, quoted from `hover.rs`), and `resolved` (sure)
  everywhere else. A derived answer reads as sure on the card, so it is held to the same standard.
- **A reworded guess line changes `answers.GUESSED` in the same change**, checked against
  `the_two_tiers_a_reader_sees_drawn_side_by_side`. `report.report` prints `BROKEN` when no card in a
  corpus reads as a guess.

## The strata

- **`sample.STRATA` rows list every spelling of one concept** (`app/jobs` and `app/workers`), and
  match by suffix (solidus is `core/app/models/`).
- **`specs` is a stratum of its own.** A directory under `TEST_ROOTS` belongs to that stratum alone.
  The draw excludes `NOT_SAMPLED` (`SKIP` minus the test roots).
- **`answers.py` reads the fence lists out of `environment.rs`**, and fails loudly if they are
  renamed.
- **The declaration stratum** (`sample.declarations`) asks at `def` names for `prepareRename`,
  `prepareTypeHierarchy`, `prepareCallHierarchy`, `implementation` and `declaration`, counting under
  `decl-`. Being its own stratum keeps other counters still.
- **Change the strata and the baseline in one commit.** `baseline.why_not` cannot see `STRATA`.

## The completion key

- **The corpus wrote `story.title`, so `title` must be on the list** at that cursor. Findings are
  raised only under a sure card. `present` is an upper bound.
- **`empty` is bucketed by tier.** `completion-declined` is a sure card over no list: key on the
  claim, not the answer.
- **The card is the member's, so an empty list under a root's member is its own bucket,
  `empty-root`** (`completion.claim`). `Object#present?` resolves on a receiver nobody typed, and
  raises no finding. The receiver's own card is no better: a constant's names the constant, not what
  it holds. A list that answered typed its receiver, so a root's member missing from it is still
  `completion-absent`.
- **An absence from an `isIncomplete` list is `cut-short`**, counted and not ruled on.
- **`calls` and `closures` pose at the *end* of a bare word.** At an empty prefix every such list is
  the 512 cap. `closures` draws its own cursors, filtered to a sure card naming an **instance**
  member (`Owner#word`) at a bare word in a class body's block: where rubydex answers the class
  object's, only a rung that found the name on an instance does that.
- **`completion` stays out of `lane2.METHODS`**, because it is the largest cost: each key keeps
  its own replies.
- **Dropped:** setters (they score spelling). ERB member cursors are kept.

## Blind spots: read a quiet sweep as "pointed elsewhere"

1. **Unasked requests can't regress here.** Get the real list from
   `grep -rn 'textDocument/' scripts/audit/`. Never trust a prose list.
2. **List composition is invisible.** The keys score presence and rank, not what the other rows are.
3. **Place order and ids are invisible.** `first-place`, `cards-anonymous` and `def-places` are
   counters for movement, never verdicts. `PLACES_CAP` is 0.
4. **Tiny populations are invisible.** When a shape has a countable population, count it and write
   a fixture. Count what a declaration *installs*, not where it is written.
5. **Generator declines are invisible.** They show up as fewer declarations. Ask which counters
   *fell*.
6. **A cursor inside `#{...}` is never sampled.**

Always still run the sweep; it is the evidence that nothing it *does* ask broke. Then measure the
surface directly with two binaries built from one tree.

## Residue and the ledger

- **`residue-signatures` (`shape/tier/places`) is a measurement, never a verdict.** A new signature
  shows as a new counter. Never turn it into a classifier that says "everything is ruled".
- **A ledger row can go stale without `stale` noticing.** Watch for `verdicts.wrong` going *up*
  after a fix. Re-read the rows that named the defect.

| What the position looks like | Verdict | Row |
|---|---|---|
| one place, and the card names the type | correct | lobsters `app/models/comment.rb:78` |
| an ivar's places, all its assignments in one file | correct | lobsters `app/controllers/invitations_controller.rb:19` |
| a local's name is all there is, and the card says so | correct | lobsters `app/views/inbox/_message.html.erb:8` |
| the receiver is a local; a long list at the right tier | correct | lobsters `app/views/moderations/_table.html.erb:8` |
| a chain breaking at the first unannotated return | correct | lobsters `app/models/search.rb:96` |
| a bare call whose `self` is not statically a named class | correct | solidus `api/app/controllers/spree/api/line_items_controller.rb:32` |
| a closure in a class body, found in instance scope | correct | mastodon `app/lib/scope_parser.rb:5` |
| a generator template tree where nothing reaches the helpers | correct | solidus `storefront/templates/app/controllers/locale_controller.rb:15` |
| a `symbol` at its own declaration site | wrong | lobsters `app/models/story_text.rb:6` |
| a generated declaration with no source mapping | wrong | lobsters `app/controllers/filters_controller.rb:14` |
| a wide namespace's declaration list, unranked | wrong | mastodon `app/workers/publish_scheduled_status_worker.rb:4` |
| an implicit namespace resolving to nothing | wrong | lobsters `app/controllers/mod/mails_controller.rb:1` |
| an ivar read silent while its assignment resolves | wrong | lobsters `app/controllers/inbox_controller.rb:27` |
| an ivar name-guess downgrading a derived chain | wrong | lobsters `app/controllers/inbox_controller.rb:22` |
| the name list reached although the receiver is derivable | wrong | solidus `backend/app/views/spree/admin/adjustments/_adjustments_table.html.erb:4` |
| a chain broken on a framework singleton | wrong | lobsters `app/jobs/restic_job.rb:11` |
| a bare call in a view context falling to the name list | wrong | solidus `backend/app/views/spree/admin/adjustments/_adjustments_table.html.erb:16` |
| an anonymous namespace printed as its id | wrong | solidus `legacy_promotions/app/views/spree/promotion_code_batch_mailer/promotion_code_batch_finished.text.erb:2` |
| an `.rbs` signature offered as a place | wrong | lobsters `app/models/mastodon_app.rb:13` |
| a gem's generator template offered as a declaration | wrong | chatwoot `app/policies/label_policy.rb:1` |
| `a.b ||= c`: the member loses its answer (upstream) | wrong | lobsters `app/models/concerns/token.rb:6` |
| `a.b::C`: the member is never indexed (upstream) | wrong | mastodon `app/models/admin/base_action.rb:21` |

The last two are rubydex bugs in `ruby_indexer.rs`:

- `||=` records the reference at `operator_loc()`.
- `::` never descends into a constant path's parent.

**The shapes still read `wrong` wherever they recur; the cited rows do not.** Two fixes corrected
every `wrong` row above and re-graded it `correct` (2026-09-28), both rubydex bugs included, which
are worked around. The ledger holds no `wrong` row.

## The baseline

1. **`make audit` = `audit score --record` + `audit report`. `make audit-baseline` blesses the last
   sweep.**
2. **Runs are comparable or not** (`baseline.why_not`: corpus SHA, seed, `--per-file`, `-n`,
   `--eager-only`). A corpus that fails is reported as **not compared**.
3. **An absent counter is not zero.** It prints as `new counter`. A new check needs no re-baseline.
4. **`--save` merges, and names what it carried over unmeasured.**
5. **The numbers live in `audit/baseline.json`, never here.**

- **The noise floor is zero.** Two back-to-back sweeps are identical. Re-measure after any change to
  the draw, or anything that could make replies order-dependent.
- **A sweep that crossed a machine sleep is not a measurement.** Look at the per-corpus seconds
  first: orders of magnitude over `BUDGET_SECONDS` means throw the run away.

## The scanner (`ruby.masked`)

- **It is a scanner, not a parser.** It may err, but only symmetrically.
- **Three passes: block comments, then heredocs, then the character walk.** A heredoc opener must be
  code on its own line (`ruby._code_columns`). `<<-TAG` and `<<~TAG` take any identifier. A bare
  `<<TAG` stays screaming-case.
- **`make audit-sample ARGS=--check` runs the invariants, with no server:**
  - no file loses its place (the last top-level `end` stays unmasked)
  - `ruby.EXAMPLES` (one line each)
  - `ruby.BLOCKS` (multi-line)

  Every scanner bug becomes a row there.
- **A pattern's context passes the same mask as its capture.** A comment ending in `.` must not draw
  the next line's first word.

## Budget and probes

- **`BUDGET_SECONDS = 420` and `PER_FILE = 32`.** `PER_FILE` is derived by `audit cost` (serial)
  from the budget, not chosen. The last serial measurement is `QUEUE_WEIGHT` in `config.py`.
  - Raising the budget does not oblige resizing the draw. Every counter depends on the draw.
- **`score --jobs` defaults to 3.** Only a serial, all-corpus run is compared with the budget
  (`make audit ARGS="--jobs 1"`). Summed seconds grow with contention and are for tuning
  `QUEUE_WEIGHT`.
- **Check 7 and check 9 ask every third eligible position** (`lane2.references.STRIDE = 3`), for
  seconds and memory. Check 9 narrows replies to `(uri, span)` as they arrive.
- **The draw is sub-linear in `PER_FILE`.** `sample` prints the mix against `shapes.SHARES`.
- **Progress:** stdout is line-buffered, with a line per corpus and one for the corpus in flight.
- **Probes outside the sweep** (record nothing, diff nothing; results go in `completion.md`):
  - `make audit-prefix`: untyped cursors followed into the word, for the two name-list ceilings.
    Censored above the ceiling. About 100 s.
  - `make audit-rank`: typed cursors at the trigger. `--steps 0` is the cheap mode. `same-owner`
    diagnoses the bands.
  - `make audit-latency`: one request in flight, member cursors, `--runs 2`. Prints the empty count
    beside every quantile. The first ask after `didOpen` is a discarded `documentHighlight`. It does
    not measure keystroke latency.
