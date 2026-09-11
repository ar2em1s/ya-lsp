---
paths:
  - "scripts/audit/**"
  - "audit/**"
  - "Makefile"
---

# The audit

`scripts/audit/` opens the six sweepable corpora, asks a stratified sample of real cursors, and
scores the answers. `make audit` runs it.

**It measures whether an answer is right, and never how fast one arrives.** Those are different
questions with different failure modes, and the reason a timing harness is not this one is that a
fast wrong answer passes a benchmark. Nothing here times anything except the budget.

**It is a measurement, not a gate.** No target fails on a number. A gate needs a threshold, a
threshold is an opinion about how much wrongness is acceptable, and this harness has not earned one
yet: the ledger is 20 rows deep. What it owes today is a number that moves when the server moves,
and a diff that says which way.

## The two committed files, and the licence rule

`audit/ledger.json` and `audit/baseline.json` are the only things this package writes into the
repository, and `corpora.md`'s rule governs both:

> **No corpus source text is ever committed into this repository. Not a line, not a fragment, not
> as a fixture, not as a ledger key.**

Four corpora are copyleft and one has a proprietary subtree; ya-lsp is MIT. Reading them is
unrestricted, copying **out** of them is not. So:

- A ledger row carries `sha256(line)[:16]`, never the line, and **not the identifier under the
  cursor either** — an identifier is a fragment and the rule is blanket.
- A baseline row carries integers, a git SHA, and each finding's `audit.site`. Never a `detail`
  string: the detail is what a person reads in a terminal and it carries the word.
- **A counter's *name* is committed too, and `residue-signatures` is the one whose name is built
  rather than written.** `shape/tier/places` composes three closed vocabularies — the six shapes in
  `shapes.SHARES`, the four tiers `answers.tier` can return, and four fixed buckets — so no corpus
  text can reach it. A counter keyed on anything a corpus chooses (a word, a path, a class name)
  would break this rule through the key rather than the value, which is the way it is easiest to
  break by accident.
- Both writers rebuild every row from a named field list (`ledger.FIELDS`, `baseline.FIELDS`), so
  the guarantee is structural rather than a habit. **A new field goes in that list or it is not
  written**, and adding one is the moment to ask what it could carry.
- A path and a line number are not source text. They are how a person goes and looks, which is the
  whole cost the rule imposes and the whole of it.

The terminal is not committed. The identifier under the cursor reaches it, because a finding nobody
can go and look at is not a finding.

## `server.toml`, and why the audit writes nothing into a corpus

`scripts/audit/server.toml` is what every server the sweep starts is configured with. It is sent
as `initializationOptions`, the way an editor sends its settings, and it is **not** a workspace
`ya-lsp.toml`.

- **Nothing may be written into a clone.** A corpus is pinned to a commit and `check_clean`
  refuses to measure one whose working tree is dirty, so a `ya-lsp.toml` dropped into
  `tmp/corpora/<name>/` would either be refused or — worse — be quietly gitignored on the corpora
  that happen to ignore it and not on the ones that do not. The settings travel over the wire
  instead and the six clones are untouched.
- **`deny_unknown_fields` does not degrade.** One key that is not a real setting rejects the whole
  layer rather than itself, so every other setting in the file silently stops applying. The
  spellings are the server's own (`workspace::config::PartialConfig`), not the editor's camelCase.
- **`[log] file_path` is resolved against the ya-lsp checkout, not the corpus.** A relative path
  would be relative to the *workspace* root, which is the clone. Both readings keep a corpus clean
  — all six gitignore `tmp` — but only this one cannot stop being true when a corpus' own
  `.gitignore` changes upstream. One file for six servers; `O_APPEND` and the pid on every line
  are what make that readable, and they are the two properties `logging.md` requires of the sink.
- **One run, one log.** `cmd_score` empties the file before the first server starts, and says
  where it is. The *server* never truncates — a second editor window on one project is a second
  process writing to the same file, and throwing away the session somebody is trying to report is
  the one thing a log must not do — but a sweep is the opposite case: one instrument, six servers
  it started itself, nothing else writing there, and ~25 MB a run to lose. Without it a reader has
  no way to tell this run's lines from last week's.
- **The file log is on, and it was measured rather than assumed.** lobsters, one release binary,
  777 positions on 2026-09-14: **27.6 s with the file against 27.1 s without**, for 2.4 MB and
  13,716 lines of which 6,494 are a request's own pair. Half a second of a 360 s budget buys the
  thing a finding could never carry — what the server did with the position, rather than only
  what it answered. Turn it off there if a corpus makes that untrue, and record the number.

**A server that reports a dead subsystem only on the log channel used to look healthy.** `client`
counted `window/showMessage` at error and warning severity and discarded `window/logMessage`
outright, so a server running with half its knowledge switched off was indistinguishable from one
that was not. `logMessage` counts now, at **error severity only** — that channel carries `warning`
for things a sweep has no opinion about — and the **first line** is kept rather than the whole
message, capped, because a backtrace is not a warning: what a reader needs is the sentence naming
what died.

## `audit.site` — one spelling for one position

`path:offset`, relative to the corpus. A lane-2 finding, a lane-1 finding, a ledger row and a
baseline entry are all keyed on it, and they agree **by being the same string** rather than by two
functions that happen to match today.

**Never a draw index.** An index only means something against the list it indexes, and the Rails
key draws its own rows: as indices, its findings and the sample's shared a number space they had no
business sharing, and lane 3 struck off whichever sampled position happened to share a number —
one per `rails-wrong`, which was 11 on the sweep that found it.

**Never `path:line`.** Two shapes can be drawn on one line, so a line identity merges them and
subtracting one drops both.

**An offset here is a *code point* index, and the encoding on the wire has to be the one that
agrees with that.** `shapes.find` runs its patterns over a decoded `str` and `ruby.at_offset` counts
elements of one, so every `column` a request is posed at and every `offset` in an `audit.site`
counts code points. `client.start` asks for **`utf-32`** and refuses any server that answers
otherwise, because UTF-32's code unit *is* the code point — so a column is legal on the wire and
still legal as a slice of a decoded line, which `lane2.footnotes` takes.

**It asked for `utf-8` until 2026-09-14, on a belief about this package that was not true.** The
comment there reasoned correctly that LSP's default UTF-16 character would shift every cursor after
an accented string, and then picked the wrong encoding to fix it, on the strength of *"the sampler
finds a position by scanning bytes"* — it does not. Under `utf-8` a cursor after a multi-byte
character was posed that many bytes to the **left**, landing inside the previous word, where the
server answers a different question perfectly correctly. That is invisible in exactly the way that
matters: lane 2 then reads a right answer to the wrong question as a defect.

Measured over the whole draw: **2 of 5,523**, both on mastodon, both on one line of
`app/helpers/filters_helper.rb` — and **both were producing a false finding**, which is a higher
rate than two positions has any right to. Offset 441 posed two bytes early put the cursor inside the
receiver rather than on the member, turning an empty prefix into a seven-character one, so the
name-based list came back 128 rows deep instead of declined and check 6 read that as a class. Offset
432 was raising a `disagree` — **and that one is in the committed baseline**, so at least one
finding a person has been diffing against for the whole of v0.5.0 is not real. Under `utf-32` both
cursors land where the sampler meant them to and both findings go away.

**A one-line handshake change and not a conversion pass, and the reason is worth keeping.** The
alternative — recording byte offsets in the sampler — meant converting at every request site, in a
package where forgetting one is precisely the bug being fixed, and it would have moved every
`audit.site` after a multi-byte character, which is ledger and baseline churn for nothing. Asking
for the encoding this package already counts in moves no site, no ledger hash and no `line_key`.
**Found by a check, not by reading the code**, which is the argument for lane 2 in miniature.

## Three lanes, and which one a new check belongs in

The lanes divide by **what a violation means**, not by what it costs to run.

| Lane | Says | Needs |
|---|---|---|
| 1 — keys | *wrong* — the corpus itself writes the right answer down | the source |
| 2 — checks | *inconsistent* — two of the server's answers contradict each other | nothing |
| 3 — ledger | *a person looked* | a person, once |

Ask in this order:

1. **Can the corpus be made to state the right answer, mechanically?** A schema column, a
   `belongs_to`, a `def` with a unique name. Then it is a **key**, in `lane1/`, and it may say
   *wrong*.
2. **Do two answers about the same cursor contradict each other?** `hover` and `definition` naming
   different spans; a card counting places `definition` does not return; the same cursor answering
   differently after an untouching edit. Then it is a **check**, in `lane2/`.
3. **Neither.** Then it is not a check at all. It is residue, and lane 3's ledger is where a person
   rules on it once and the run reuses the verdict.

**A check compares two answers and may only compare answers to the same question.** Check 4's
`Defined in N places.` is the count `hover` drew its card from against the count `definition`
returned — one number computed twice, until `definition` at an instance variable began answering
its *assignments* while `hover` went on answering the class the variable was *typed* as. The first
sweep after that change fired `places-differs` **64 times, every one of them at an `ivar` cursor
and none anywhere else**, which is the shape of a broken pairing rather than the shape of a defect:
a class reopened 54 times, held against a jump that names one line. Those claims are now counted
under `places-of-a-variable` and printed under the check's own line, because a check narrowed
silently is indistinguishable from a check that has gone quiet on its own.

**A check that needs an opinion is not a lane 2 check.** Lane 2's whole claim is that a violation
is a self-contradiction — no judgement anyone has to make twice, which is the property that would
let it run unattended one day. "This answer is unhelpful", "this guess is too broad", "a card
should have said more" are all opinions, and putting one in lane 2 converts a self-contradiction
count into somebody's taste with a number attached. Two places take an opinion: a **key**, where
the opinion is *the corpus writes this down and here is the pattern that reads it*, and the
**ledger**, where it is written down as a verdict with a reason and a person's name on the commit.

The same rule forbids a direction table in the report. A finding is a defect a check already named,
so *new* means regression; a counter going up may be good or bad depending on the counter, the
answer lives in the check that owns it, and a second copy here would be a second copy that drifts.

**Over-blocking is safe in a key and unsafe in a filter on the draw.** A key that throws a position
away removes a question from its own denominator, which costs precision and nothing else. The
sampling filters — `routes.non_helpers`, `sample.SKIP` — remove the question from the sample, and a
shape filtered too hard is a shape the audit stops being able to see. `non_helpers` is deliberately
the weaker of the two for that reason.

## Adding a check, a key, or a corpus

Adding a **check** is a file in `lane2/` and one name in `lane2.CHECKS`. Nothing in `report` or
`commands` enumerates checks by hand, and the registry's order *is* the numbering — so a new check
is **appended**, because the numbering is what a reader of last week's report is holding in their
head. A check may read any reply on the `Row`, including the one lane 1's `completion` key posed;
what decides the lane is what a violation *means*, never who sent the request. The file owns:

```
counters() -> dict                 the counter keys it owns, fresh
check(row, place, counts, findings)  one position; append (kind, site, detail)
line(counts) / summary(counts)     its line in one corpus' report, and over all five
under(counts)      optional        extra lines under its own line
breakdown(counts)  optional        extra lines in the trailing block, after both lanes
FINDINGS                           the kinds it raises, in the order to print them
```

Adding a **key** is a file in `lane1/` and one name in `lane1.KEYS`, plus `NAME`, `TOTAL`, `ASKS`
and the same reporting hooks. `ASKS` decides *when* it runs and it is not a preference:

- `ASKS = False` — `grade(corpus, drawn, answers)` reads replies lane 2 already collected. No
  server, and it can be re-run against a recorded transcript. (`neutral`)
- `ASKS = True` — `ask(corpus, client, seed, opened, drawn, answers)` poses its own request, so
  it needs a live server **and must run before `ask_rebased`**. (`rails`, `completion`)

An asking key is handed the run's draw and the replies to it, and the two use that differently on
purpose. `rails` ignores both and draws its own cursors, because Rails macro sites are a shape the
sample does not target. `completion` takes both: it asks a **second request at the sample's own
cursors**, and the hover reply already collected for each one is what lets it report an absent
member beside the tier the server had just claimed for that receiver.

**A key that asks a second question does not fill `covered`.** Lane 3 subtracts a key's `covered`
sites from the residue because a position `neutral` graded needs no person. A position whose
*completion* was scored is still residue: the member being in the list says nothing about where
`definition` went.

Adding a corpus is `scripts/corpora.toml` and nothing here. This package never names a corpus: the
pin table's `role` says which may be swept, and `static-only` is the role for one that may be
counted and never asked. Nothing holds that role today.

- **Check 5 asked across a settle, and until 2026-09-15 it was barely asking at all.** The pass
  sent every `didChange` and then asked every position pipelined — and the first deferred answer
  that comes back empty makes the server settle and re-ask, which catches the graph up, so
  everything after it was answered by a graph that had been **re-indexed and re-resolved** rather
  than by `Rebase`. Measured over one draw of 5,523 positions: **139 of 11,046 requests reached
  `Rebase`, and 1 of 1,792 on discourse.** The resolve debounce is not the cause and holding it open
  for an hour moves nothing — the settle that contaminates the pass is the *retry* the check's own
  first miss triggers. The edits go **in the stream** now, each immediately before the requests that
  need it, which puts the figure at **99.2%**; `client.ask_rebased`'s docstring carries the protocol
  and the plan's note for defect 11 carries what it then found.
- **A differing answer is still not necessarily a rebase.** A finding that crosses a settle is one
  the graph changed under, and the pass cannot prevent a settle it did not cause. The 13 this check
  reported on discourse before the rewrite are the standing example: one over-broad declaration the
  cold index built and the incremental re-index corrects, which is defect 25's residue and not a
  `Rebase` defect. The findings are worth having and still have to be read before they are believed.
- **discourse was excluded and is not any more, and the reason it was is worth keeping.** The
  exclusion came from an *exhaustive* sweep of every member position, where it is 641,537 of them
  and over an hour a side. This package draws a stratified sample, and there it is unremarkable: **896
  positions over 104 files**, between solidus' 753 and mastodon's 1,074, settling in 5.5s against
  mastodon's 5.3s. What it costs is per-position latency — 50.9 ms against 4.8–13.2 for the rest,
  which is a graph about as large as the other five put together — so it adds **77s to a 245s
  run**. What it buys: it is 16% of the positions and 33–39% of the findings, it is the only
  corpus with `plugins/`, `migrations/`, `script/` and `vendor/` in the `where` buckets, and it was
  the only corpus on which **check 5 had ever fired** — which, until that check was rewritten
  2026-09-15, said more about the protocol than about discourse. Adding it perturbs nothing: the other five
  come back byte-identical in a six-corpus recording, so their baseline blocks stay valid.

- **A `def` that exists only under `vendor/` blocks the name rather than answering it.** The
  neutral key's rule is *exactly one `def` in the tree*, and this is that rule applied to the one
  case where the walk is narrower than reality: a gem checked into `vendor/cache/` is **also**
  installed under the bundle's gem home, outside the corpus, and the installed copy is the one a
  language server indexes and resolves to. Expecting the vendored path states a unique definition
  that does not exist, and no correct server can satisfy it. Measured on forem: `following?` and
  `following_by_type`, both scored `wrong` against a path under `vendor/cache/acts_as_follower-…`
  while the answer given was that same file under the bundler gem home. It **blocks** rather than
  matching on the library's own tail, deliberately — accepting any path that ends the same way
  would let a wrong copy of a genuinely duplicated name pass, which is the failure this key exists
  to catch. Losing the question is the conservative direction, the same one `patterns.HASH_KEY`
  already takes, and it costs `knowable` two positions on one corpus.

## The strata, and the one that had to be a stratum rather than a glob

`sample.STRATA` is eleven rows of *(name, directories, files)*, and a row is a **list** of the
spellings one concept takes — a background job is `app/jobs` in three corpora and `app/workers` in
two, so either name alone is empty half the time. A stratum matches a **suffix** rather than
joining onto the corpus root, because solidus is an engine monorepo whose models live in
`core/app/models/`: a rule that joins measures a different part of that repository than of the
other five.

**`specs` is a stratum and not a widened glob, and the difference is what the rows mean.** mastodon
keeps `spec/lib/`, so a glob that merely admitted the test tree files somebody's specs under the
**`lib`** stratum — and a stratum meaning "library code" in three corpora and "library code plus
its specs" in two is one whose counts cannot be compared across the rows they print in. So the
exclusivity rule runs *before* the suffix match: a directory under `TEST_ROOTS` belongs to the test
stratum and to no other, and every other stratum may claim only directories outside them. `SKIP`
goes on naming the test roots because `lane1.closures` imports it to decide which files its own
scan reads — an RSpec file is wall-to-wall `describe … do` at two-space indentation, which is
exactly what its `OPENS` pattern matches — so the draw's own exclusion is `NOT_SAMPLED`, which is
`SKIP` minus those roots. They were one tuple only because nothing sampled a spec.

**Why it was added.** Five of the six corpora keep more Ruby under `spec/` than anywhere else,
counted through `candidate_files` itself, which is the only count that means anything here: a
root-level `find` undercounts solidus by a factor of five. **specs** 128 / 913 / 1,238 / 958 /
1,416 / 4,430 against **models** 48 / 375 / 248 / 179 / 167 / 547, in the order the pin table lists
them — so a draw that skipped the tree whole was leaving out the largest thing in every one of
them. It is also the only stratum `environment.rs` fences: a cursor *inside* `spec/` reads the
wider cursor list and is entitled to answers a cursor in `app/` may not be given, and nothing in
the draw exercised that. **Check 2 cares most** — a *Resolved* card landing in a test tree is its
whole subject, and until this stratum existed no drawn cursor was in one.

20 files, rather than a share of how large those trees are: the point is to reach the shape, not to
weight the draw by how many specs a project happens to write. `lib` went 12 -> 20 in the same
change for the neighbouring reason — it is the **only non-Rails Ruby in the draw**, reaching 2,081
files on discourse and 75 on mastodon, and it is what keeps the sample from being a statement about
Rails alone. Only lobsters falls short of either number, and a stratum that cannot fill shrinks the
draw rather than borrowing from one that can.

## The completion key, and the defect it was built to settle

`completion` is lane 1's third key and the only one that scores a request other than `definition`.
The key is `neutral`'s idea one step over: the corpus wrote `story.title`, the code runs, therefore
`title` is a member of whatever `story` is — so at a cursor on `title` the right answer is *a list
with that name in it*. It re-uses the sample's own `member` cursors, so the hover reply already
collected for each one is beside it, and an absent member is reported with the tier the server had
just claimed for that receiver.

**It exists because a comparison report blamed the wrong thing and nothing could re-measure it.**
The claim was that `MAX_COMPLETION_ITEMS = 512` truncates before ranking and so loses the answer.
`completion.rs`'s `take_best` partitions by the same comparator and truncates after, so that
mechanism is not real. Measured 2026-09-11 over 500 member positions with the cap raised to
100,000, the answer turned out to be neither the report's nor the two this project had written
down:

- **74% of the missing members are candidates** — and they sit at a **median rank of 4,070**, in a
  list whose median size is 29,801.
- Among those huge lists there is **exactly one distinct size per corpus** — 26,073 on lobsters,
  45,953 on chatwoot. Every one of them is the same set: the corpus' entire name universe.
- `hover` at 112 of those cursors: **101 name-match cards, 11 no card, none naming a type.**

So where ya-lsp cannot type the receiver it answers with everything it knows, and the cap sliced
that to an alphabetical 512, which is what read as *"the answer is missing 36% of the time"*. The
cap was hiding the defect rather than causing it.

**What the key does not score, and it has cost a defect twice.** It asks *is the member present and
where does it rank* at a cursor whose receiver ya-lsp typed. It never asks anything about the
**other** rows — not where they were declared, not whether the project can call them — so a list
can be a third unusable and every counter in this lane can be right. A completion defect that does
not move the answer's rank is invisible here **by construction**: the ranking defect closed in
v0.5.0 needed a measurement of 500 member positions the key could not supply, and the test-tree rows
fenced out of the list later moved **0 counters over six corpora** while being 13.6% of what one
of them offered at a bare word. Both were found by a measurement written for the occasion. If a
change is about the *composition* of a list rather than about one row's rank in it, this key will
report nothing and that is not evidence.

**It posed only `member` cursors until 2026-09-14, and defect 26 lived in the gap that left.** A
bare word had no list for anything to be held against — the same fact check 6 states from the
other side when it reads `row.listed` and not `row.offered`. So `hover` answering a receiverless
call from one scope while `completion` answered from another was outside every lane: the sweep
that closed it moved **0 counters and raised 0 findings** over six corpora while a probe written
for the occasion counted 90 cursors changing. Two keys close it and they are described below.

**And three requests are not asked at all.** The sweep drives `hover`, `definition`,
`documentHighlight`, `references` and `completion`; nothing in it sends `workspace/symbol`,
`typeHierarchy/subtypes` or the call hierarchy. So a change to the picker's ranking or to a subtype
list cannot move a counter here **whatever it does** — the third such case in v0.5.0 reordered the
picker on six corpora, 1,304 rows fell out of the cap and every one of them was under a test tree.
That change's own sweep read *0 findings new, 0 gone, 0 counters moved* while the probe written for
it counted 1,304 rows moving. Read a quiet lane as *this instrument was pointed elsewhere*, never as
*nothing happened* — and still run it, because a quiet sweep is the only evidence that a change
aimed at an unasked surface did not break an asked one. Measure the unasked list with a probe
written for it, two binaries from one tree, and record the numbers in the rule that owns the
surface.

**And a population can be too small for the draw to reach it, which is the fourth blind spot and
the one with no counter at all.** The sample is a few thousand cursors over six applications, so a
defect whose whole population is a handful of sites is represented by one of them or by none.
v0.5.0's last row is the case: a `Class.new(base) do … end` body written inside a `def` answered
against the wrong side of `self` at **14 of the 14** such sites the six applications contain, and
the draw reached **one** — so the report moved a single finding for a defect that was wrong
everywhere it occurred. Nothing in the sweep says so, because a stratified draw has no way to
report that a stratum was a dozen sites wide to begin with. **Where a shape has a countable population, count it and
write a fixture**; the sweep is the wrong instrument for it and a quiet lane is not evidence. The
tell to look for is a row whose report and whose truth differ by an order of magnitude.

**And the population to count is what a declaration installs, not where it is written.**
The same release skipped a concern spelling on the strength of **two** occurrences in the six
applications — which was every place the applications write it and no place it matters. The
framework writes it nine times, two of which put `model_name` and `human_attribute_name` on
every model, and those two lines answer **372** cursors in the same six applications. A mixin,
a macro and a generated declaration all have this shape: the sites that declare them are
countable and few, the sites that call them are neither, and only the second number says what
the row is worth.

Over the whole draw the key reported it as a standing number: **2,143 member cursors, 1,160 present
of 2,098 answered, and 874 of the 938 absences under a *Guessed* card.**

**Since the server began declining that list rather than answering it, the same draw reads
differently and the key had to change shape with it.** An untyped receiver now gets no rows at all
above its own ceiling, so the outcome moved out of `absent` and into `empty`: 1,019 present of 1,186
answered, 167 absent (132 of them *Guessed*), and **957 empty — 920 *Guessed*, 31 no-card, 6
*Resolved***. `truncated` did not move and cannot: a decline sets `isIncomplete` too, because that
is what lets the list come back as the word narrows.

**What it counts, and the two lines that are not counters.** A finding is raised only where the
card is **Resolved**: the server asserting it knows the receiver's class and then not offering a
member that class demonstrably has. A *Guessed* card with no member on the list is the name rung
doing what its label says, so it is counted and not reported — the same discipline `rails-wrong`
keeps. `present` is an **upper bound**, stated in the module: the test is that the name is on the
list, and a list drawn from the wrong class can hold it by coincidence.

**`empty` is bucketed by that same tier, and that became necessary rather than tidy.** It was 45 of
2,143 when this key was written — servers with nothing to say at a handful of cursors, reasonably
counted without a tier. It is now the commonest outcome, because ya-lsp *decides* to answer with
nothing where it cannot type the receiver, and **a decision hides exactly what an absence used to
reveal**: a *Resolved* card over an empty list is the server naming a receiver's class in one
request and failing to type it in the next. `completion-declined` is that finding, and it is the
same defect `completion-absent` used to catch at those cursors — six of them, which stopped firing
the day the list they were absent from became no list at all. **When a server stops answering, a
counter that keys on the answer stops measuring; the fix is to key on the claim instead.**

**A list the server said was incomplete cannot witness an absence, and for two releases this key
let one.** `isIncomplete` is set at exactly one moment — the cap dropped rows — so the server has
already said *this is not the whole answer, ask again as the word grows*. Scoring a member missing
from such a list measures `MAX_COMPLETION_ITEMS` rather than the resolution, and the two want
opposite fixes: the ceiling is a number with a measurement behind it, a resolution defect is a
defect. The row that settled it survived the privacy gate that took every other `absent-resolved`
to zero, because it was never one of them: discourse
`app/models/site_setting_localization.rb:32`, `SiteSetting.respond_to?`, 512 items, `isIncomplete`,
on a member `vendor/rbs/core/kernel.rbs:2986` declares **public**.

So an absence from a truncated list is **counted and not ruled on** — `cut-short`, printed under
the `truncated` line rather than dropped silently, because a question the key declines is a fact
about the sample. Losing the question is the conservative direction, the same one
`lane1.neutral` takes for a name defined only under `vendor/`. Recorded 2026-09-16 at 1 / 9 / 14 /
13 / 14 / 35 over the six corpora against a `truncated` of 228 / 283 / 376 / 264 / 318 / 292 — so
the great majority of truncated lists still hold their member and still score.

**`empty` is deliberately *not* gated on the same flag**, and the asymmetry is the point. A
decline sets `isIncomplete` too, but `completion-declined` is a finding *about* the decline — a
*Resolved* card over no list at all — so reading the flag there would delete the check rather than
narrow it. The flag says a list was cut short; only a list that exists can be.

**The key scores one direction of the pair and the other one hid a defect for two releases.**
`completion-declined` is a *Resolved* card over an **empty** list. The mirror of it — a **guessed**
card over a list that is a real class's members — was deliberately not reported, on the sound
grounds that a name-rung card with the member missing is the label doing its job. But the card had
two sentences hiding inside one: *the receiver's type is unknown* and *the receiver is a `User` and
has no such method* were both printed as the first, and only the second is compatible with a list
built from `User`. So the pair was self-contradicting at every one of those cursors and no counter
keyed on it. Measured 2026-09-14 with a probe written for the occasion, one draw asked of two
binaries from one tree: over 4,200 `@ivar.member` cursors, **207 of 1,506** cards saying the type
was unknown sat over a class's own member list; over 3,600 `Const.member` cursors, **180 of 209**.
After the split those read **2** and **0**, and the two left are `a.b += c`, where the reference
sits on the operator and the two surfaces are reading different cursors.

**The fix for it swept byte-identically.** 5,523 positions, 0 findings new, 4 gone, 85 counters
moved — the same comparison block as the run before it, character for character. That is the
strongest form of the reading above: not *this lane was pointed elsewhere* but *this instrument
cannot see this at all*, and it is the fourth time on the completion side after 24's ranking, 28's
composition and 32's scoring a regression as an improvement.

**That check is now check 6, and the answer to where it lives is that lane 2 learned to read a
reply another lane asked for.** *The card says the receiver has no type and the list beside it is a
class's members* needs no corpus source and no opinion, which is lane 2's definition — but lane 2's
three `METHODS` are what lane 2 sends, and the completion reply is posed by a lane **1** key. The
two ways out were a key growing a finding of a kind lane 1 does not otherwise raise, or a `Row` that
can see a reply it did not ask for; the second is right, because **what a lane is has nothing to do
with who sent the request**. Lane 1 says *wrong against the source*, lane 2 says *inconsistent with
itself*, and this claim is the second — putting it in the key would have filed a self-contradiction
under a scale that can say *wrong*, which is the distinction the whole table is built on.

So `lane1.completion.ask` writes its replies into the run's own `answers` rather than into a local
dict, and `lane2.context.Row` reads `(index, "textDocument/completion")` out of it. `completion`
stays out of `lane2.METHODS` on purpose: it is 45% of the draw and the largest single cost in the
budget, and a second copy sent by lane 2 would double it to own a request that is already being
sent at those exact cursors, one pass earlier, in coordinates the rebase has not touched.

**`listed` and `offered` are two facts and the check dies if they are one.** A cursor the key never
posed — a bare word, a setter it drops, a run with `--no-key` — has no list to disagree with, and
reading that absence as an empty list would report the whole draw as agreeing. Check 5 keeps
`deferred` apart from its replies for the same reason and it is the same mistake.

**The discriminator is exact, and it is exact because of the corpora rather than by theorem.** At an
empty prefix `by_name` declines above `MAX_UNTYPED_CANDIDATES` = 512, and an untyped receiver's
candidate set is the corpus' whole name universe — 26,073 rows on lobsters, 45,953 on chatwoot. So
here a non-empty list at a bare dot *is* a typed receiver. A seventh corpus small enough to fit under
512 would be answered from the name-based list at a bare dot and every guessed card in it would read
as a contradiction: re-read this before adding one.

**The mirror is counted and not reported.** A card *naming* a class over an **empty** list is the
same disagreement read the other way, and it is the weaker claim — a typed receiver's list can be
emptied by a fence or a filter, where an untyped receiver's list cannot be conjured — so it is
`receiver-named-empty` under the check's own line rather than a finding. `completion-declined` is
already the top-tier version of it.

**What it reads on the six: 0 of 1,012**, out of 2,475 cards with a list beside them; the mirror
is **39 of 179**. It read **2 of 1,014** and **39 of 177** for the few hours defect 34 was open, and
the two numbers moved by exactly the two cursors: `receiver-unknown` fell by two and
`receiver-named` rose by two, which is the same two cards no longer saying the type is unknown.
Nothing else in the check moved, which is the arithmetic a fix to one cause should produce.

The 1,012 is the healthy population — a receiver nothing typed, a card saying so, and a list that
correctly declined — and the fix that preceded this check is what moved 387 cards out of it. The 39
is the one to look at next: a card naming the receiver's class over no list at all is the server
typing a receiver for `hover` and not for `completion`, at better than one in five of the cursors
where it names one. **A zero here is not the check being useless.** It found a server defect on its
first run; what it reads now is that the defect is gone.

**Both of the two were real, and the first run raised a third that was this harness's own bug.**
The two are chatwoot, `enterprise/app/models/concerns/toolable.rb` at 582 and 843 — one concern, two
members on the same receiver, which was read as a plain method call and is not one: it is a local
holding `self`, read inside a block that rebinds `self`. The card said the type was unknown over 529
and 3 name matches while `completion` at the same byte answered 92 rows of a real class's members.
Both confirmed by hand against a release binary, then **traced and fixed the same day** — a `self`
is now placed by where it was written rather than by where the cursor is, and the same two cursors
answer what the identical ones at method scope always did. The third finding was the check reading a
cursor posed in the wrong place — see `audit.site` above, where that is now fixed.

**A first run that raised three findings, of which one was a defect in the instrument, one was a
defect in the server and one was both, is the check working.** It is also the first time this
project has found a server defect by asking two of its own answers to agree rather than by asking
one of them to be right, which is the whole argument for lane 2 and had until now been an argument
rather than a result.

**One change to `hover` did not touch this check at all, and the measurement says so twice over.**
The sentence a card carries for an anonymous receiver moved from *the receiver's type is unknown* to
*the receiver is the class object `Class.new`, which has no such method*. That falls inside the
existing `NAMED` prefix, so no edit to `receiver.py` was needed — and the sweep then moved **0
counters and 0 findings on all six corpora**, run against run, because **no cursor in the 5,523 has
an anonymous receiver at all**. The reclassification this check would have performed is real and
was never exercised.

Two lessons, and the second is the useful one. A wording change that lands inside a prefix the check
already reads is the cheap case — the expensive one is the paragraph below. And **a fix the draw
cannot see is not a fix that was not needed**: the shape exists, the card was saying something false
about it, and the only reason the audit is silent is that the stratified sample did not land on one.
A check reading zero is evidence about the draw as much as about the server.

**The footnote coupling now has a second reader, and it is the same hole one level deeper.**
`report.report` refuses to be believed when the tier vocabulary stops matching `hover.rs`; check 6
reads *which of the four* guessed sentences a card carries, and nothing outside that module does. A
rewording there reads as a clean zero rather than as a break, so the check says so itself: cards
with a list beside them and not one receiver sentence among them prints a line naming the cause.
Said and not raised, because a narrow draw can honestly hold no receiver miss at all.

**Adding it cost no re-baseline, and that is already written down twice.** A counter absent on one
side is not zero on that side — it prints as `new counter`, which is the paragraph above this
section — and the four counters the `completion` key added went in on exactly that footing with the
baseline left alone. The baseline is re-recorded when a *fix* moves the numbers it should move, and
that has happened twice inside item 12, each time with the sweep described in the commit that
blessed it. Neither of those was ever a reason to defer a check.

**Two filters, and one of them was wrong once already.** A setter is dropped, because at
`order.total = 1` the method is `total=` and servers disagree about which to offer — that scores
spelling. ERB was dropped too, on the strength of *"completion is gated"* in the crate's own
description, and that reading was wrong: measured on a real template, a member cursor answers with
125, 364 and 512 items and the right member is on the list. What is gated in a template is what a
**bare word** offers. The filter had thrown away 75 of 286 lobsters positions, in the files where
this audit has found most of its defects.

**Cost.** The sweep goes from 107s to **206s of the 300s budget** — one more request at 45% of the
draw. That is the largest single thing in the budget and it is worth it; if it ever stops fitting,
the honest move is to raise the budget rather than to sample the key.

## `audit prefix`, and the thing a word-start sample cannot see

**Every cursor this harness draws is at a word's start**, which is the right place to ask whether
an answer is *correct* and the wrong place to ask what a prefix is worth. The two ceilings on the
name-based list — `MAX_UNTYPED_CANDIDATES`, above which a receiver with no type is answered with no
rows at all, and `MAX_UNTYPED_COMPLETION_ITEMS`, how many rows are sent when it is — only bite
mid-word, so no standing counter can say whether either sits in the right place. `make audit-prefix`
is the probe that can, and it is **not part of the sweep**: it records nothing, diffs nothing, and
is run by a person setting a constant.

Run once against a binary with the admission ceiling raised, it is what split one ceiling into two:
the word the corpus wrote sits inside the first 128 rows of **every** measured list of up to 512
candidates, 430 of 430, and in 152 of 157 up to 1,024. The ranking is trustworthy where the row
count is not.

- **The classification is free and needs no card.** At an empty prefix a receiver the graph can
  type answers with its own members and one it cannot is over every corpus' universe and declined.
  So the empty answer *is* the classification, and only those cursors are followed outwards.
- **It is censored above the bound, and says so in its own output.** A list that comes back is
  under the ceiling, so its size is exact; a declined one reports "over" and never by how much.
  That answers *should this be smaller* and cannot answer *should it be larger* — the second needs
  a binary built with the constant raised, which is how the 512 cap was measured.
- **A median is printed here where the `completion` key keeps every number an int.** That key's
  counters are summed across corpora and diffed against a baseline, and neither operation means
  anything on a median. This one is read once by a person, so the statistic that answers the
  question is the one to print.
- **Cost is about 100s for the default 150 cursors a corpus**, because one cursor is up to seven
  requests. It is outside `BUDGET_SECONDS` for that reason and `-n 0` follows the whole draw.

## `audit rank`, which is the same probe pointed at the other path

`audit prefix` follows an **untyped** cursor outwards because the two ceilings on a guess only bite
mid-word. `audit rank` follows a **typed** one, and the reason is different: here the ceiling bites
at the word's *start*, which is where an editor sends the request the instant `.` is typed and where
nothing has been typed to rank by. `make audit-rank` is the target; like `prefix` it records nothing,
diffs nothing, and is run by a person setting a constant.

- **The classification is `prefix`'s, taken the same way and shared with it.** A list that comes back
  at an empty prefix is a receiver this server typed; an empty one is the untyped decline. One probe
  keeps the cursors the other throws away.
- **It is censored above the ceiling in exactly the same direction**, so the same rule applies: a
  word ranked past the ceiling arrives as `absent` and cannot be told from one the list never held.
  *Should this be lower* is answerable from one run; *is it losing answers* needs a second binary
  built with the constant raised.
- **`--steps` is the cost control.** Following a cursor three characters into its word is four
  requests; `--steps 0` asks only at the start, which is what a size distribution needs and a
  quarter of the work.
- **`same-owner` is the diagnosis beside the count**, read off each row's `detail`. A rank made
  almost entirely of the answer's own owner's rows is the alphabet inside one band, which no
  ordering of the bands can improve. A rank made of *other* owners' rows is a band that sorted above
  the one holding the answer, which is a ranking question and has a fix.
- **Both ceilings' evidence lives in `completion.md`**, not here: this file says what the instrument
  can see, that one says what it saw and what was done about it.

## The ordering in `measure()`, which is a constraint

`commands.measure` is the one pipeline both `score` and `adjudicate` run, because two copies of an
ordering this particular are two copies that drift.

```
ask_all           the sample's own questions, on a shared `opened` set
lane1.asked       every ASKS key — BEFORE the rebase pass
ask_rebased       re-asks every position over a graph one edit behind it, editing in-stream
client.stop()
lane2.run         every check
lane1.graded      every reading key
lane3.run         last
```

- **Keys that ask run before `ask_rebased`.** That pass shifts every offset in every sampled
  document, and a model file a key also reached would answer off by however many lines the pass has
  inserted into it. That is one line per *position* pointing into the document rather than one per
  document, which is also why the differ subtracts an `answers.Shifts` count taken at the position's
  own index instead of a single total per file.
- **One `opened` set for the whole server.** A second `didOpen` for a document the client already
  holds is not a legal message, and the Rails key reaches model files the sample also drew from.
- **Lane 3 is last by definition.** "A position no rule decides" is defined by what the other two
  lanes just did: it subtracts every site a key covered and every site a finding named.

## The three tiers, which are what the audit is for

*Resolved* — the code names the type. *Derived* — a signature, an assignment, or a convention, and
a footnote says which. *Guessed* — matched on the method name alone.

**A chain is only as strong as its weakest rung.** One card can carry a Derived footnote and a
Guessed one at once, and `answers.tier` reports `guessed`. A card that reads Resolved is one with
no caveat on it at all — the word itself is never printed, so the tier is what the card does *not*
say, and `answers.GUESSED`/`DERIVED` are the sentences `hover.rs` actually writes.

That coupling is load-bearing and it is unpinned: reword a footnote in `hover.rs` and every card
here reads Resolved, check 2 reports a flood or a zero, and neither means anything. `report.report`
refuses to be believed instead — no derived and no guessed cards in a run with hover answers prints
`BROKEN` and stops.

**The `BROKEN` guard does not cover a *new* footnote, and that hole cost a sweep on 2026-09-12.**
A rung added to `locator` wrote a sentence `answers.DERIVED` had never heard of; the other five
footnotes still appeared, so nothing printed `BROKEN`, and every card the new rung answered was
counted **resolved** — a tier the card does not claim and the rung does not give. So: **a new
provenance footnote is a new entry in `answers.DERIVED` or `GUESSED`, in the same change**, and the
list to check it against is `the_three_tiers_of_answer_drawn_side_by_side`, which pins every
footnote `hover.rs` writes, whole, in one string.

## The two keys at a bare word, and the prefix that decides whether they work

`lane1.calls` poses `completion` at the draw's **848** receiverless calls and `lane1.closures`
scans the corpora for a shape the draw does not target. Both are lane 1 and not lane 2, and that
is the lane rule working rather than a convenience: the corpus wrote the call, the code runs,
therefore the name is callable from whatever `self` is there — so a list without it is **wrong**,
which is a stronger claim than the card contradicting the list beside it. The card is still read,
to bucket an absence by the tier the server had claimed.

**The prefix is the word, and that is the one decision either of them makes.** The member key
poses *the instant `.` is typed*, so its prefix is empty by construction. A bare word has no
trigger character, and at an empty prefix every one of these lists is the 512-row ceiling —
measured at 90 closure cursors over six corpora, every list `isIncomplete` at exactly
`MAX_COMPLETION_ITEMS`, and 17 of them missing a member that is on the list the moment a character
arrives. A key posed there scores the cap and calls it the server. The cursor therefore goes at
the **end** of the word, which is where a reader's own is when they stop typing and look, and
where the claim is strong: `completion.rs` collects candidates for the receiver and then filters
by `tier`, so the candidate set is not a function of the prefix and a name absent at a full word
is absent from the set outright.

**`calls` keeps its replies out of the run's `answers`, and that is not a detail.** `Row.listed`
is *(index, completion) is in answers*; a call cursor appearing there would move `receiver-asked`,
an existing counter, so that check 6 could count a card with no receiver sentence in it. A key
that quietly redefines another lane's denominator is not additive, whatever else it measures.

**`closures` draws its own cursors, which only `rails` had done before it.** The reason is the one
`rails` gives: the draw holds 848 receiverless calls out of the hundreds of thousands the corpora
write, and this shape has **90** sites in them — a sample that size lands on none. Its scan is a
**floor and not a census**, reaching a macro call that opens a block at two-space indentation in a
file whose first construct is a `class` or `module`. That is Rubocop's layout and not Ruby's
grammar, and it is deliberate: **the filter is the server's own sentence.** A card carrying *Found
on an instance of* is `locator` saying the closure rung fired, which by construction means the
class object holds no such name, so a candidate the scan invents costs one pipelined hover and is
dropped. No mask is run over the file for the same reason — `ruby.masked` is a character walk over
every byte of every `.rb` in six corpora, and a word inside a string is a candidate the footnote
throws away anyway.

First run, 2026-09-14: **1,710 candidates, 90 closure cards, 90 of 90 with the member on the list**
— and **0 of 90** against a binary built from the same tree with `add_closure` reverted, which
raised 14 findings on lobsters alone. That is the key catching the defect it was written for, on
a shape no draw of this size reaches. `calls` reads **638 present of 758 answered of 848 asked**,
632 of them first, and raised **one** finding on its own first run: a `Class.new(base) do … end`
body offering one row where the card resolves, which is defect 34's file one request over.

**They cost 16.8 s**, one release binary over six corpora, 381.4 s against 398.2 s — which is what
the budget below was raised to match.

## The residue, and where each kind of it was ruled

Lane 3 counts the residue two ways. `residue-shapes` is the six cursor shapes. **`residue-signatures`
is `shape/tier/places`** — the cursor's shape, the tier of the card it got, and how many places
`definition` named, in four buckets (`0`, `1`, `2-5`, `6+`). That is what a person actually sees at a
position, compressed to a string that carries no corpus text and is therefore safe to commit.

**A signature is a measurement and never a verdict.** It cannot say a position is right; that is
what a person and the ledger are for. What it buys is the one question that otherwise costs a whole
session: *has a kind of position appeared that nobody has looked at?* A signature absent from the
baseline reads as a **new counter** in `audit report`, the same way a new finding does, and the
existing rule that an absent counter is not zero is what makes that legible.

**Resist the temptation to turn this into a classifier that reports "everything is ruled".** It was
proposed and rejected: a rule like *guessed + local receiver -> correct* is a verdict compiled into
a heuristic, which is a third place for an opinion to live besides a key and the ledger — and the
worst of the three, with no corpus evidence and nobody's name on it. It would also go stale in the
direction that hides work: positions move between signatures when the server changes, get swallowed
by whatever rule now matches, and "0 unruled" keeps printing over behaviour nobody inspected.

What is durable is the map below: each kind of residue, and the one ledger row that says why. The
counts are deliberately **not** here — they live in `audit/baseline.json`, for the reason the rest
of this file keeps its numbers there.

**A sweep that ran across the machine sleeping is not a measurement, and it reads exactly like a
regression.** 2026-09-15, the same binary swept twice half an hour apart: the second run reported
**542 findings new** over forem and solidus, `check 5` firing 329 times, and `definition` down 245.
Nothing had changed but `audit/ledger.json`, which no server reads. The tell is the **per-corpus
seconds**, printed beside every corpus and against the budget on the totals line: 46 / 94 / 136 /
125 / 168 / 218 s in the good run, against 39 / 1,713 / 6,993 / 5,794 / 11,041 / 12,755 s in the
bad one — a laptop that slept with six servers suspended on it, whose clients then timed out and
recorded the missing answers as answers that are not there. Read that line before reading the
diff: a run whose summed seconds are wildly past `BUDGET_SECONDS` is a run to throw away, and
`corpora.md`'s rule about comparing numbers taken on different days has a smaller sibling here —
**a number taken while nothing was running is not a number.** The corpus that finished before the
machine slept was byte-identical to its own previous run, which is what identifies the cause
rather than merely the symptom.

**A ledger row goes out of date in a way `stale` cannot see, and the signature for it is
`verdicts.wrong` going *up*.** `audit ledger` calls a row stale when the **corpus line** it was
ruled on has changed, which is the only thing it can check without a person. What it cannot check
is whether the ruling still describes the *answer*: fixing the defect leaves the corpus line
untouched, so the row stays live and its verdict goes on being counted. The move to watch for is a
position leaving the findings — a lane 1 or lane 2 check stops deciding it — and falling back into
the residue, where a ruling written about the old answer applies. 2026-09-15:
`app/views/login/index.html.erb:1410` was ruled *wrong — template ivar read: `@referer` is set in
`login_controller.rb:33` and `:116` and the template gets nothing*, and now answers both writes;
the diff read `0 findings new, 39 gone` with `verdicts.wrong` 12 -> 13. **So a fix that closes a
defect is the moment to re-read the ledger rows that named it**, and re-ruling one is a person's
job for the reason the lane exists at all.

## Two measurements that are not checks

- **`shape/tier/places` says how many places an answer named and never which came first**, so
  re-ordering a place list moves no counter at all — and a card that prints an internal id instead
  of a name leaves both the tier and the count exactly where they were. A whole class of change is
  therefore invisible to every lane, which is how a fix can ship against a green sweep that is
  digit-for-digit identical to the one before it.
- **`first-place`, `cards-anonymous` and `def-places` close that, and they are counters rather than checks.** They
  are kept for every position the way `tiers` is, they raise no finding, and no line attaches a
  verdict — `lane3.signature`'s standard, for `lane3.signature`'s reason. `first-place` buckets an
  answer of two or more places by where its **first** place came from against where the rest came
  from: `one-library`, `majority`, `minority`. `cards-anonymous` counts cards naming a declaration
  rubydex identified by number.
- **`def-places` is the third, and it asks about the other end of an answer.** Both lanes read
  what the server said at the *cursor*; nothing asked whether the place it offers to send a reader
  to is one it can describe. It can fail to — a declaration's definition list can hold a
  definition `locator::locate` does not attribute back to it, and the jump then lands on a real
  `def` the server has nothing to say about. `places.offered` collects every distinct `def` line
  the draw's answers name and `places.ask` hovers each **once**, with no `didOpen`: a place is
  somewhere the reader has not been, and opening a hundred documents the sample never drew would
  change what a later request sees. Two restrictions, both measured: only a `def`, because a
  schema, route or macro line is a generated declaration pointing at its real source and hover is
  right to say nothing there — 1,132 of discourse's 1,556 silent places are exactly that; and once
  per line rather than once per offering, because a name-matched list of forty offers the same
  forty lines at every cursor that asks.
- **It cost 5.1 s of 320.5 s uncapped, which is why `PLACES_CAP` is 0.** Measured 2026-09-12 over
  six corpora: 31,924 distinct places asked, **126 answered nothing** — lobsters 14, forem 15,
  chatwoot 17, mastodon 20, solidus 21, discourse 39. Verified against a recording of the same
  commit without the pass: **0 corpora perturbed**, so it adds its own counter and moves nothing
  else.
- **Neither is entitled to call its number a defect, and the wording has to keep saying so.**
  `minority` is not wrong: a method name spread over forty gems has no meaningful majority, and the
  `member` shape is full of exactly that. What these two are for is **movement** — a list that
  re-orders or a name that stops being an id has to show up somewhere, and a counter that moves is
  the honest form of that. A check asserting *the first place must be the file named after the
  constant* would restate the server's own rule back to it, which is a regression test, and this
  file's last line says why that is not what this instrument is.
- **A library is one installed gem, one Ruby, or the whole corpus checkout** — coarser than a
  directory and finer than `answers.where`'s single word `gem`, which folds a whole bundle into one
  name. The corpus counts as one library however many directories it has: an engine monorepo writes
  one namespace across `core/`, `admin/` and `api/`, and calling those three libraries would report
  a project reopening its own module as a project disagreeing with itself.
- **A counter kept as a dict of counts is skipped by the totals loop**, so anything the totals line
  prints has to be named in `commands.cmd_score`'s merge list. Two of lane 3's breakdowns are there
  for the same reason.
- **A counter moving the wrong way is worth chasing to a position, and no check will do it for
  you.** Item 40 left lobsters at `tiers.guessed 221 -> 223` with **0 findings new**: every lane-2
  check compares ya-lsp's answers against each other, and two answers that agree on being a guess
  are consistent. The instrument is a per-position tier diff between two binaries over one tree —
  draw every call cursor, ask both, print each position whose tier moved by name. It found
  `self.primary_key = :id` in lobsters' `StoryText` in one run, and the cause was a generator
  declining a **writer** silently. **Both spellings of a guess have to be read**: `answers.GUESSED`
  is `("Matched on the method name alone", "Type guessed from the name")`, and a diff that tests
  only the first scores a receiver guess as an answer.
- **A generator that declines is invisible to every lane.** `Namespaces::spellable` refusing an
  owner and `syntax::spellable` refusing a name both produce *fewer declarations*, which is a
  smaller list and a lower tier — never an inconsistency, never a wrong key. Item 40 shipped two
  such declines and both were found by a counter, not by a check: one by `completion-call/absent`
  moving `+3` on mastodon, the other by the tier diff above. When a change adds a decline, the
  question to ask the sweep is *which counters fell*, not *which checks fired*.

| what the position looks like | verdict | the row that says why |
|---|---|---|
| one place, and the card names the type | correct | lobsters `app/models/comment.rb:78` |
| an ivar's places, all of them its assignments in one file | correct | lobsters `app/controllers/invitations_controller.rb:19` |
| a local's name is all there is, and the card says so | correct | lobsters `app/views/inbox/_message.html.erb:8` |
| the receiver is a local; the list is long but the tier is right | correct | lobsters `app/views/moderations/_table.html.erb:8` |
| a chain breaking at the first unannotated return | correct | lobsters `app/models/search.rb:96` |
| a bare call whose `self` is not statically a named class | correct | solidus `api/app/controllers/spree/api/line_items_controller.rb:32` |
| a closure in a class body, the name found in the instance scope | correct | mastodon `app/lib/scope_parser.rb:5` — **the rung now answers it**: `locator::in_a_closure`, derived |
| a generator template tree, where nothing reaches the helpers | correct | solidus `storefront/templates/app/controllers/locale_controller.rb:15` |
| a `symbol` at its own declaration site | wrong | lobsters `app/models/story_text.rb:6` |
| a generated declaration carrying no source mapping | wrong | lobsters `app/controllers/filters_controller.rb:14` |
| a wide namespace's declaration list, unranked | wrong | mastodon `app/workers/publish_scheduled_status_worker.rb:4` |
| an implicit namespace resolving to nothing | wrong | lobsters `app/controllers/mod/mails_controller.rb:1` |
| an ivar read that is silent while its assignment resolves | wrong | lobsters `app/controllers/inbox_controller.rb:27` |
| an ivar name-guess downgrading a chain that was derived | wrong | lobsters `app/controllers/inbox_controller.rb:22` |
| the name list reached although the receiver is derivable | wrong | solidus `backend/app/views/spree/admin/adjustments/_adjustments_table.html.erb:4` |
| a chain broken on a framework singleton whose return is fixed | wrong | lobsters `app/jobs/restic_job.rb:11` |
| a bare call in a view context falling to the name list | wrong | solidus `backend/app/views/spree/admin/adjustments/_adjustments_table.html.erb:16` |
| an anonymous namespace printed as its internal id | wrong | solidus `legacy_promotions/app/views/spree/promotion_code_batch_mailer/promotion_code_batch_finished.text.erb:2` |
| an `.rbs` signature offered as a place | wrong | lobsters `app/models/mastodon_app.rb:13` |
| a gem's generator template tree offered as a declaration | wrong | chatwoot `app/policies/label_policy.rb:1` |
| `a.b ||= c` — the member loses its own answer | wrong | lobsters `app/models/concerns/token.rb:6` |
| `a.b::C` — the member is never indexed | wrong | mastodon `app/models/admin/base_action.rb:21` |

The last two are **upstream**, in rubydex's `ruby_indexer.rs`, and they are different bugs rather
than one: `||=` records the reference at `operator_loc()` instead of `message_loc()`, so the
reference exists at the wrong offset and hovering the operator finds it; `::` never descends into a
constant path's parent, so no reference is recorded at all and no offset in the expression answers.
The `+=` spelling works only because its reference lands on the call operator one byte before the
message, where `locator`'s end-inclusive `covers()` picks it up — one defect propping up another.

## The baseline, and what may be diffed

`audit score --record PATH` writes a run. `audit report PATH` diffs it against
`audit/baseline.json`. `make audit` is both; `make audit-baseline` blesses the last sweep.

**Two runs are comparable or they are not, and there is no third answer.** `baseline.why_not`
gates every comparison on the corpus SHA and on the four facts that decide which questions were
asked — seed, `--per-file`, `-n`, `--eager-only`. A pin bump moves every offset in every edited
file, so every counter moves and every finding reads as both new and gone. A corpus that fails the
gate is reported as **not compared**, never compared anyway. This is `corpora.md`'s "never compare
absolute corpus numbers taken on different days" made mechanical.

**The baseline and the sampler move together, or neither moves.** `baseline.why_not` gates on the
corpus SHA, the seed and the four draw flags — and it **cannot see `STRATA`**. So a change to the
strata changes which questions were asked in a way no gate will catch, and the diff then reads as
findings appearing or vanishing with no server change behind them. Committing the two in one commit
is the whole protection. Measured when the `specs` stratum and the `lib` widening landed: lobsters
went from 777 drawn positions to 1,081, and the six corpora's findings from 174 to 289 — every one
of those 115 a question the old draw never asked, and not one of them a regression. Recording a
baseline against a sampler that is **not** committed is the same failure from the other side, which
is why the sweeps taken while this change sat in the working tree were diffed against each other
rather than against the committed file.

A counter absent on one side is **not zero** on that side. A check added since the baseline reads
`0 -> 38` under that reading, which says a regression happened and none did.

`--save` **merges**: a run over one corpus keeps the other four's recorded numbers, and says which
it carried over unmeasured. A carried-over row is a number nobody measured today and a reader has
to be told which those are.

**The numbers live in `audit/baseline.json` and not in this rule.** A second copy of them here
would be stale after the next sweep, which is the failure mode `canary.md` avoids by keeping its
asserted counts in the `Makefile` and nowhere else.

**The noise floor is zero, and that is what makes a diff worth reading.** Measured 2026-09-11:
two full sweeps, run back to back from the same binary at the same pins, produced identical
records — 4,627 positions over the five corpora swept then, and the diff of the
second against the first is 0 new, 0 gone, 0 moved. So a counter that moves is the server moving.
Re-measure this after anything that could make a reply order-dependent, **and after any change to
the draw**: it has been re-measured three times for that reason — once after `ruby.masked` was
fixed, once after the `completion` key added a second request at 45% of the draw, and once after
`definition` and `hover` began answering at an instance variable. A ranked list is the most
plausible thing in this harness to come back in a different order, and it does not. A harness with
a noise floor reports weather, not regressions.

The third of those is the one to read carefully, because the pair was not clean: check 4 was
narrowed **between** the two sweeps, so the honest claim is the smaller one. Of the 512 counters
the baseline now holds, **465 were identical across the two runs** and every one that moved was
check 4's own — plus lane 3's bookkeeping downstream of it, because a position that stops being a
finding goes back into the residue. Nothing the server computes moved. The baseline after it holds
**512 counters and 199 findings**, the second number 218 lower than before that change and every
one of the 218 a `disagree` at an instance variable.

## What the scan hides, and the blind spot that leaves

`ruby.masked` decides which bytes of a file a cursor may be drawn in. It is a **scanner, not a
parser**, and that is the right trade — it runs before the server sees the file, so whatever it
misclassifies it misclassifies for every position equally. What it may not do is misclassify
*asymmetrically*, and every bug it has had did exactly that: a whole residue class that made no
sense, traced back to one construct the walk read wrong.

Ten of them. The first six were each found by asking why a class of positions existed at all; the
last four were found by the invariant below, on 2026-09-14, which is the first time round that way:

| what the walk read wrong | what the sample got |
|---|---|
| a heredoc body, scanned before it was hidden | `OR` inside a SQL string drawn as a **constant**, 17 positions |
| `%w[]` spanning three lines | fourteen words of a string array read as column reads |
| a nested `"` inside `#{...}` | the outer string ended early; `%Y-%m-%d` became a drawn position |
| a regex literal, not masked at all | `/[^A-Za-z0-9]/` offered `A` and `Z` as constants, 5 positions |
| a `"` inside `%r{...}`, because the percent pass ran last | 93% of one file masked as a string |
| a backtick inside a multi-line `/x` regex, and `` $` ``/`$'` | the walk lost its place to the end of the file |
| a **commented-out** heredoc opener, because comments are masked after heredocs | the terminator is commented out too, so the hunt for a bare `SQL` ran to the end of the file — four of the six were migrations somebody had commented out |
| a heredoc opener **inside a string**, for the same ordering reason | `emit "… <<~SQL"` is a generator writing Ruby; the file after it went undrawn |
| **`=begin` … `=end`**, which the walk did not know about at all | prose in one holds an apostrophe, which opened a string nothing closed — the **third** source of an unbalanced quote, where this module used to say there were two |
| a **mixed-case** heredoc tag under a screaming-case pattern | `<<-EndOfTests` matched the `E` alone, then looked for a line reading `E` for ever |

**The last four are one shape and it is the catastrophic one**: a mask that ran off the line it
began on and never came back. They are why `masked` now runs **three** passes rather than two —
block comments, then heredocs, then the character walk — and why a heredoc opener has to be *code*
on its line before it opens anything, which `ruby._code_columns` decides by reading that one line
alone. Widening the tag was the one change that could have gone the other way: `<<-TAG` and
`<<~TAG` take any identifier because nothing else in Ruby spells them, while a bare `<<TAG` keeps
the screaming-case rule because `arr <<item` is the push operator written without a space.

**Fixed 2026-09-14, and the draw did not move**: 24,058 of 24,058 files keep their place, against
24,049 before, and `audit sample` reads the same 5,523 positions over the same files it did with
the bug in. None of the nine is a file the draw had picked — which is luck, not design, and the
reason to run the invariant rather than wait for a residue class to look wrong.

`make audit-sample ARGS=--check` runs the two invariants that would have caught all ten,
and it needs no server:

- **No file loses its place.** For every `.rb` file in the corpora, the last top-level `end` must
  be unmasked. It catches an unbalanced quote, which is the only way this scanner fails
  catastrophically. It read 0 of 12,146 over five corpora; over six it read **9 of 24,058, all of
  them discourse**, which joined on 2026-09-12 after that number was taken. Those nine are the
  last four rows of the table above and they were fixed on 2026-09-14: it reads **24,058 of
  24,058** today.
- **`ruby.EXAMPLES`** — fourteen literal lines, each with the mask it must produce: an
  interpolation, a regex, two divisions that are not regexes, a percent literal, a modulo, an ERB
  tag, a punctuation global, a comment holding an apostrophe. **Every bug above is a row in it**,
  which is the only form of that list worth keeping — one written from imagination would test the
  cases the author already had right.
- **`ruby.BLOCKS`** — four of them need more than one line, because the bug each holds *is* a mask
  running off the line it began on: the commented-out opener, the quoted opener, the `=begin`
  block and the mixed-case tag. Same dot convention, read by the same `check`, which is why the
  count reads 18 and not 14.

**The stated blind spot: a cursor inside `#{...}` is never sampled.** The interpolation is scanned
as code, because it has to be to find the `}` that ends it, and then masked as string, because a
scanner that pretended to read arbitrary nested Ruby would misread it in some new way. Those are
real cursors and the server should answer them; the audit cannot see whether it does. That is a
sampling gap to state, not a claim about the server.

**The dot is part of the `member` shape and the mask applies to it.** `MEMBER`'s `\s*` matches a
newline, so a comment ending in a full stop reached across the line break and drew the next line's
first word — 44 positions over five corpora asking `definition` where `def`, `class` and `module`
are defined. A dot ending a line of *real* code is a line continuation and stays drawn. This is
the general shape of the rule: a pattern's context has to pass the same mask its capture does.

## The budget

**The budget is a serial number and the sweep no longer is.** `score --jobs` defaults to **3**,
so what the totals line prints against the budget is a wall clock three servers shared — 198.2 s
against 384.1 s serial, measured 2026-09-14 with byte-identical counters on both. The figure to
hold against `BUDGET_SECONDS`, and the one `cost` derives `PER_FILE` from, is the **summed**
seconds printed beside the queues; `audit cost` takes no `--jobs` and stays serial for that reason.

`BUDGET_SECONDS = 420` and `PER_FILE = 32`, and the second is **derived from the first by
`audit cost`, not chosen**. The measurement that produced it is recorded in `config.py` beside the
constant: a budget derived once and then forgotten is a budget nobody can check. Raise the budget,
re-run `cost`, and record both. **Raised 360 -> 420 on 2026-09-14 to match a measured 398.2 s, and
`PER_FILE` deliberately did not move with it.** Re-running `cost` will now say the budget buys more
positions; it does not follow that it should. The draw is what every committed counter is a
function of, so resizing it moves all of them at once — a decision to take on its own and not one
to inherit from an arithmetic identity.

**The run can be watched, and until 2026-09-14 it could not be.** It prints a line per corpus as
each one finishes, and one naming the corpus in flight before it starts; `__main__` sets
`line_buffering` on stdout so those escape a pipe. Python block-buffers whenever stdout is not a
terminal, so backgrounded or redirected the whole six-minute run printed nothing until it exited,
which reads exactly like a hang — and the largest corpus is a minute and a half of one process
saying nothing even once the buffer is off, which is what the in-flight line is for.

The draw is **sub-linear** in `PER_FILE` — the thin shapes run out of candidates before the
abundant ones do — so raising it does not scale the sample, and `sample` prints the mix against
`shapes.SHARES`' stated targets so that is visible.
