"""Lane 1's third key — **the member the corpus already wrote down**, scored on `completion`.

`neutral` and `rails` grade `definition`. This one grades the other request a developer leans on
all day, and the key is `neutral`'s own idea one step over: the corpus wrote `story.title`, the
code runs, therefore `title` is a member of whatever `story` is. Put the cursor where the member
begins — the offset an editor sends the instant `.` is typed — and the right answer is *a list
with that name in it*. Nobody had to hand-write that key and no server can argue with it.

**Why it is a key and not a check.** Lane 2 compares ya-lsp against itself and can only say
*inconsistent*; the truth here comes out of the corpus, so this scale can say *wrong*. It is
`ASKS = True` because `completion` is not one of lane 2's three methods and the request carries a
`context` no other position in this harness sends.

**It re-uses the sample's own `member` cursors rather than drawing its own.** `rails` has to draw,
because the macro positions it scores are a shape the sample does not target; a member after a dot
is the shape the sample draws *most* of. Re-using them buys the thing a separate draw could not:
the hover card for the same cursor is already in `answers`, so an absent member is reported with
the tier the server claimed for its own receiver — which is the difference between a list that
lost a candidate and a receiver that was never typed.

**What is thrown away, and every filter is here because it would score spelling, not knowledge.**

  - **A setter.** At `order.total = 1` the method is `total=`, and servers disagree about whether
    to offer `total` or `total=`. Scoring that disagreement measures a naming convention.
  - **Not a template.** ERB was filtered out in the first version of this key on the strength of
    *"completion is gated"* in the crate's own description, and that reading was wrong — measured
    on `_threads.html.erb`, a member cursor in a template answers with 125, 364 and 512 items and
    the right member is on the list. What is gated there is what a **bare word** offers, through
    the view context; `person.` in a template is `person`'s members like anywhere else. The filter
    threw away 75 of 286 lobsters positions, in the files where the audit has found most of its
    defects.
  - **A cursor the server answered with no list at all.** That is `empty`, counted beside the
    others: a server that offered nothing has not offered the wrong thing, and the two failures
    want different fixes. **It is bucketed by tier for the same reason `absent` is**, and that
    became necessary rather than tidy: `empty` was 45 of 2,143 when this key was written and is
    now the commonest outcome, because ya-lsp declines where it cannot type the receiver. A
    decision hides what an absence used to reveal — a *Resolved* card over an empty list is the
    server naming a receiver's class in one request and failing to type it in the next — so the
    finding follows the shape and `completion-declined` says it at the position.

**`present` is an upper bound and this scale says so out loud.** The test is that *the name* is in
the list, and a list of 500 members drawn from a receiver typed as the wrong class can hold a
`title` belonging to something else. The rank buckets make that visible without pretending to fix
it — a name found at rank 400 of 500 is not the server knowing the receiver. What the scale
measures without an asterisk is the floor: the name is **absent**, and no ranking and no client
filter can rescue a list that does not contain the answer.

**It marks no position as `covered`, and that is deliberate.** Lane 3 subtracts a key's
`covered` sites from the residue, and `neutral` fills it because a position whose `definition` it
graded needs no person. This key grades a *different request* at the same cursor: the member being
in the list says nothing about where `definition` went, so a position it scored is still residue.
The one exception is a position it raises a finding at, which leaves the residue the way every
other lane's findings do — a named defect is not something to adjudicate again.

**Every counter here is an int on purpose.** `score` sums a key's integers across corpora and the
baseline flattens them one level; a median would do neither, and it moves with the sample besides.
Four rank buckets diff, and they are the three questions a person actually has — is it first, is
it on screen, is it reachable at all.
"""

from audit import site
from audit.answers import card_of, tier
from audit.client import uri

NAME = "completion"
FINDINGS = ("completion-absent", "completion-declined")
TOTAL = "asked"
# Poses its own request — the same cursor, a different method — so it needs a live server and
# must run before `ask_rebased` moves every sampled line.
ASKS = True

METHOD = "textDocument/completion"
# What an editor sends the instant `.` is typed: an explicit trigger character rather than an
# invoked completion. The two are different requests and only one of them is the one a developer
# makes thousands of times a day.
CONTEXT = {"triggerKind": 2, "triggerCharacter": "."}
IN_FLIGHT = 32


def bucket(rank):
    """The rank, as one of four names. A closed vocabulary, for `lane3.places`' reason."""
    return ("rank-1" if rank == 1 else "rank-2-10" if rank <= 10
            else "rank-11-50" if rank <= 50 else "rank-51+")


def names(item):
    """Every spelling of *this item inserts that name*, most authoritative first.

    `label` is what the user reads and is usually the name, but it is also where a server puts
    decoration — a signature, an owner suffix. `filterText` is what the client is told to match
    on and `textEdit.newText` is what lands in the buffer, so a name missing from `label` is
    still a hit if either of those spells it.
    """
    out = []
    for key in ("label", "filterText", "insertText"):
        value = item.get(key)
        if isinstance(value, str):
            out.append(value)
    edit = item.get("textEdit") or {}
    if isinstance(edit, dict) and isinstance(edit.get("newText"), str):
        out.append(edit["newText"])
    cleaned = []
    for value in out:
        value = value.strip()
        # `title(...)`, `title arg`, `title # Story` — cut at the first character that cannot be
        # part of a Ruby method name, but **after** a trailing `?` or `!`, which can.
        for stop in ("(", " ", "\t", "#", ":"):
            if stop in value:
                value = value.split(stop, 1)[0]
        if value:
            cleaned.append(value)
    return cleaned


def rank_of(items, word):
    """1-indexed position of `word` in the server's own order, or None if it is not there.

    The server's order and not a re-sort: ya-lsp sends its array already ranked, so re-sorting on
    `sortText` here would measure the harness rather than the server.
    """
    for index, item in enumerate(items):
        if word in names(item):
            return index + 1
    return None


def setter(text, line, column, word):
    """Is this cursor the `foo` of `foo = 1`, where the method is really `foo=`?"""
    rows = text.split("\n")
    if line >= len(rows):
        return False
    after = rows[line][column + len(word):].lstrip()
    return after.startswith("=") and not after.startswith("==")


def counters():
    return {"asked": 0, "declined": 0, "answered": 0, "empty": 0, "present": 0, "absent": 0,
            "truncated": 0, "cut-short": 0,
            "rank-1": 0, "rank-2-10": 0, "rank-11-50": 0, "rank-51+": 0,
            "absent-resolved": 0, "absent-derived": 0, "absent-guessed": 0,
            "absent-no-card": 0,
            "empty-resolved": 0, "empty-derived": 0, "empty-guessed": 0,
            "empty-no-card": 0}


def ask(corpus, client, seed, opened=None, drawn=None, answers=None):
    """Ask `completion` at every drawn `member` cursor and score the list against the word.

    `drawn` and `answers` are the run's own — the same cursors lane 2 has just scored, and the
    hover replies it already holds. A key that took neither would have to re-draw and re-ask, and
    would then be scoring a different sample than the report prints it beside.
    """
    counts, findings = counters(), []
    rows = [(index, row) for index, row in enumerate(drawn or []) if row[1] == "member"]
    if not rows:
        return counts, findings
    # **The replies land in the run's own `answers`, and that is what lets lane 2 read them.**
    # Every other reply in that dict was posed by lane 2; this one is posed here, at the sample's
    # own cursors and before `ask_rebased` moves a line. *The card says the receiver has no type
    # and the list beside it is a class's members* is a contradiction between two of the server's
    # answers, so it is a check and not a key — but the second request only exists because this
    # key sends it. A dict local to this function made that check unwritable without a third
    # request per position, on the largest single thing in the budget.
    #
    # Nothing is in flight when this runs: `ask_all` drains to empty before it returns and the
    # Rails key asks through `ask_all` too, so every key that arrives here is one posted below.
    texts, posed = {}, []
    replies = answers if isinstance(answers, dict) else {}
    for index, (_, _, path, line, column, offset, word) in rows:
        if path not in texts:
            texts[path] = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
        if setter(texts[path], line, column, word):
            counts["declined"] += 1
            continue
        posed.append((index, path, line, column, offset, word))
        client.post((index, METHOD), METHOD, {
            "textDocument": {"uri": uri(corpus.dir / path)},
            "position": {"line": line, "character": column},
            "context": CONTEXT})
        for key, result in client.drain(down_to=IN_FLIGHT):
            replies[key] = result
    for key, result in client.drain():
        replies[key] = result

    for index, path, line, column, offset, word in posed:
        counts["asked"] += 1
        answer = replies.get((index, METHOD))
        items = answer.get("items") if isinstance(answer, dict) else answer
        incomplete = isinstance(answer, dict) and bool(answer.get("isIncomplete"))
        if incomplete:
            counts["truncated"] += 1
        items = items or []
        rung = tier(card_of((answers or {}).get((index, "textDocument/hover")))) or "no-card"
        if not items:
            counts["empty"] += 1
            counts[f"empty-{rung}"] += 1
            # **An empty list is bucketed by tier for the same reason an absent member is, and
            # it did not used to need to be.** `empty` was 45 of 2,143 — servers that had
            # nothing to say at a handful of cursors. It is now the commonest outcome, because
            # ya-lsp *decides* to answer with nothing where it cannot type the receiver, and a
            # decision hides exactly what an absence used to reveal: a **Resolved** card over an
            # empty list is the server naming the receiver's class in one request and failing to
            # type it in the next. Six of those were raised as `completion-absent` until the
            # list they were absent from became no list at all.
            if rung == "resolved":
                findings.append(("completion-declined", site(path, offset),
                                 f"[{rung}] `{word}` — hover names the receiver's type and "
                                 f"completion answered no list at {path}:{line + 1}"))
            continue
        counts["answered"] += 1
        rank = rank_of(items, word)
        if rank is not None:
            counts["present"] += 1
            counts[bucket(rank)] += 1
            continue
        # **A list the server said was incomplete cannot witness an absence, and this key used to
        # let it.** `isIncomplete` is set at exactly one moment — the cap dropped rows — so the
        # server has already said this is not the whole answer and the client's job is to ask
        # again as the word grows. Scoring the member missing from it measures the ceiling rather
        # than the resolution, and the two want opposite fixes: one is a number with a
        # measurement behind it, the other is a defect.
        #
        # **Measured, on the row that made this worth writing.** discourse
        # `app/models/site_setting_localization.rb:32`, `SiteSetting.respond_to?` — 512 items,
        # `isIncomplete`, and `vendor/rbs/core/kernel.rbs:2986` declares the member **public**.
        # It survived the whole of the privacy gate that took the other 89 `absent-resolved` rows
        # to zero, because it was never one of them. It is the last row in the head-to-head that
        # names a defect the server does not have.
        #
        # **Counted and not dropped**, in a bucket of its own: an absence the key declines to
        # rule on is a fact about the sample and `cut-short` is where it is visible. Losing the
        # question is the conservative direction, the same one `lane1.neutral` takes for a name
        # defined only under `vendor/`.
        if incomplete:
            counts["cut-short"] += 1
            continue
        counts["absent"] += 1
        counts[f"absent-{rung}"] += 1
        # **Reported at the position and not only counted, and only for the top tier.** A
        # *Guessed* card with no member in the list is the name rung doing what its label says.
        # A **Resolved** one is the server asserting it knows the receiver's class and then
        # failing to offer a member that class demonstrably has — the same broken promise
        # `rails-wrong` exists to name, one request over.
        if rung == "resolved":
            findings.append(("completion-absent", site(path, offset),
                             f"[{rung}] `{word}` absent from {len(items)} items at "
                             f"{path}:{line + 1}"))
    return counts, findings


def line(counts):
    if not counts.get("asked"):
        return "nothing asked"
    top10 = counts["rank-1"] + counts["rank-2-10"]
    return (f"{counts['present']} present of {counts['answered']} answered "
            f"({counts['absent']} absent, {counts['empty']} empty), of {counts['asked']} asked; "
            f"{counts['rank-1']} first, {top10} in the top ten")


summary = line


def under(counts):
    """The two shapes a total hides: where the present ones sit, and what the server had claimed
    about the receiver at the ones it lost."""
    if not counts.get("asked"):
        return []
    out = ["rank " + "  ".join(f"{name.split('-', 1)[1]} {counts[name]}" for name in
                               ("rank-1", "rank-2-10", "rank-11-50", "rank-51+"))]
    for shape in ("absent", "empty"):
        lost = [(rung, counts[f"{shape}-{rung}"])
                for rung in ("resolved", "derived", "guessed", "no-card")
                if counts.get(f"{shape}-{rung}")]
        if lost:
            out.append(f"{shape} " + "  ".join(f"{rung} {count}" for rung, count in lost))
    if counts["truncated"]:
        cut = counts.get("cut-short", 0)
        out.append(f"{counts['truncated']} lists came back truncated (isIncomplete)"
                   + (f", {cut} of them without the member — not ruled on" if cut else ""))
    if counts["declined"]:
        out.append(f"{counts['declined']} not asked (a setter, where the method is `name=`)")
    return out
