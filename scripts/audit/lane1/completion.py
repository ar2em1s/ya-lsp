"""Lane 1's `completion` key: **the member the corpus already wrote down.**

The corpus wrote `story.title` and the code runs, so `title` is a member of whatever `story` is. Put
the cursor where the member begins (the offset an editor sends the instant `.` is typed), and the
right answer is *a list with that name in it*. Nobody hand-writes the key, and no server can argue
with it.

**A key, not a check.** Lane 2 compares ya-lsp with itself and can only say *inconsistent*. Here the
truth comes from the corpus, so this can say *wrong*. `ASKS = True`, because `completion` is not one
of lane 2's methods and it carries a `context` nothing else sends.

**It re-uses the sample's `member` cursors instead of drawing its own.** The hover card for the same
cursor is already in `answers`, so an absent member is reported with the tier the server claimed for
its receiver. That separates a list that lost a candidate from a receiver that was never typed.

**The card is the member's, and over an empty list a root's member claims nothing about the
receiver** (`claim`). `Object`, `Kernel` and `BasicObject` are every object's ancestors, so a card
naming `Object#present?` resolves on a receiver nobody typed. That empty list gets its own bucket,
`empty-root`, and raises no finding. The receiver's own card is no better witness: a constant's
names the constant, not what it holds. **A list that answered is not excused**: it typed the
receiver, so a root's member missing from it (`self.class` in a module's `def`) is still
`completion-absent`.

**What is thrown away** (each filter would otherwise score spelling, not knowledge):
- **A setter.** At `order.total = 1` the method is `total=`, and servers disagree about offering
  `total` or `total=`. That is a naming convention.
- **Not templates.** A member cursor in a template answers like anywhere else: what the view context
  gates is a **bare word**, and `person.` in a template lists `person`'s members.
- **A cursor answered with no list** is counted as `empty`, not scored: a server that offered
  nothing has not offered the wrong thing, and the fixes differ. It is **bucketed by tier**, because
  ya-lsp declines where it cannot type the receiver. A *Resolved* card over an empty list is the
  server naming the receiver's class in one request and failing to type it in the next, so
  `completion-declined` reports it at the position.

**`present` is an upper bound, and says so.** The test is that *the name* is in the list, and a long
list from a receiver typed as the wrong class can hold someone else's `title`. The rank buckets show
that without pretending to fix it: rank 400 of 500 is not the server knowing the receiver. The floor
needs no asterisk: an **absent** name cannot be rescued by any ranking or client filter.

**It marks no position `covered`, on purpose.** Lane 3 subtracts a key's `covered` sites from the
residue. This key grades a *different request* at the same cursor, and a member in the list says
nothing about where `definition` went, so the position is still residue. The exception is a position
it raises a finding at: that leaves the residue like every lane's findings do.

**Every counter is an int, on purpose.** `score` sums a key's integers across corpora, and the
baseline flattens them one level; a median allows neither and moves with the sample. The rank
buckets are the questions a person has: is it first, is it on screen, is it reachable at all.
"""

import re

from audit import site
from audit.answers import card_of, tier
from audit.client import uri

NAME = "completion"
FINDINGS = ("completion-absent", "completion-declined")
TOTAL = "asked"
# Poses its own request (same cursor, different method), so it needs a live server and must run
# before `ask_rebased` moves every sampled line.
ASKS = True

METHOD = "textDocument/completion"
# What an editor sends the instant `.` is typed: an explicit trigger character, not an invoked
# completion. They are different requests, and this is the one a developer makes thousands of times
# a day.
CONTEXT = {"triggerKind": 2, "triggerCharacter": "."}
IN_FLIGHT = 32
# Every object's ancestors. A member only these declare is found whatever the receiver is.
ROOTS = ("Object", "Kernel", "BasicObject")
# The card's first line names the member: `Owner#name` or `Owner.name`, owner first.
MEMBER = re.compile(r"^```ruby\n([A-Z][\w:]*)[#.]")
RUNGS = ("resolved", "guessed", "no-card")


def bucket(rank):
    """The rank, as one of four names. A closed vocabulary, for `lane3.places`' reason."""
    return ("rank-1" if rank == 1 else "rank-2-10" if rank <= 10
            else "rank-11-50" if rank <= 50 else "rank-51+")


def names(item):
    """Every spelling of *this item inserts that name*, most authoritative first.

    - `label` is what the user reads. It is usually the name, but servers also decorate it (a
      signature, an owner suffix).
    - `filterText` is what the client matches on.
    - `textEdit.newText` is what lands in the buffer.
    A name missing from `label` still counts if either of the others spells it.
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
        # `title(...)`, `title arg`, `title # Story`: cut at the first character that cannot be part
        # of a Ruby method name, but **after** a trailing `?` or `!`, which can.
        for stop in ("(", " ", "\t", "#", ":"):
            if stop in value:
                value = value.split(stop, 1)[0]
        if value:
            cleaned.append(value)
    return cleaned


def rank_of(items, word):
    """1-indexed position of `word` in the server's own order, or None if it is not there.

    The server's order, not a re-sort: ya-lsp sends its array already ranked, so re-sorting on
    `sortText` would measure the harness instead of the server.
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


def claim(card):
    """What the member's card claims about the receiver of an **empty** list: its tier, `root`, or
    `no-card`.

    `root` is a sure card for a member only `ROOTS` declare, which any receiver reaches, typed or
    not. A guess stays a guess. Not asked of a list that answered (see the module docs).
    """
    rung = tier(card) or "no-card"
    found = MEMBER.match(card or "")
    if rung == "resolved" and found and found.group(1) in ROOTS:
        return "root"
    return rung


def counters():
    return {"asked": 0, "declined": 0, "answered": 0, "empty": 0, "present": 0, "absent": 0,
            "truncated": 0, "cut-short": 0,
            "rank-1": 0, "rank-2-10": 0, "rank-11-50": 0, "rank-51+": 0,
            **{f"absent-{rung}": 0 for rung in RUNGS},
            **{f"empty-{rung}": 0 for rung in ("root", *RUNGS)}}


def ask(corpus, client, seed, opened=None, drawn=None, answers=None):
    """Ask `completion` at every drawn `member` cursor and score the list against the word.

    `drawn` and `answers` are the run's own: the cursors lane 2 just scored, and the hover replies
    it holds. Without them this would re-draw and re-ask, and score a different sample than the one
    the report prints beside it.
    """
    counts, findings = counters(), []
    rows = [(index, row) for index, row in enumerate(drawn or []) if row[1] == "member"]
    if not rows:
        return counts, findings
    # Nothing is in flight when this runs: `ask_all` drains to empty before returning, and the Rails
    # key asks through `ask_all` too, so every reply arriving here was posted below.
    texts, posed, replies = {}, [], {}
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
        card = card_of((answers or {}).get((index, "textDocument/hover")))
        rung = tier(card) or "no-card"
        if not items:
            counts["empty"] += 1
            counts[f"empty-{claim(card)}"] += 1
            # **An empty list is bucketed by tier, like an absent member.** ya-lsp *decides* to
            # answer with nothing where it cannot type the receiver, and a decision hides what an
            # absence would reveal: a **Resolved** card over an empty list is the server naming the
            # receiver's class in one request and failing to type it in the next.
            if rung == "resolved":
                findings.append(("completion-declined", site(path, offset),
                                 f"[{rung}] `{word}` — hover finds the member on the receiver's "
                                 f"class and completion answered no list at {path}:{line + 1}"))
            continue
        counts["answered"] += 1
        rank = rank_of(items, word)
        if rank is not None:
            counts["present"] += 1
            counts[bucket(rank)] += 1
            continue
        # **An `isIncomplete` list cannot witness an absence.** The flag is set exactly when the cap
        # dropped rows, so the server has said this is not the whole answer and the client should
        # ask again as the word grows. Scoring a member missing from it measures the ceiling, not
        # the resolution, and the two need opposite fixes: one is a number backed by a measurement,
        # the other is a defect.
        #
        # **Counted, not dropped**, in its own bucket: `cut-short` is where an absence the key
        # declines to rule on stays visible. Losing the question is the conservative direction, the
        # same one `lane1.neutral` takes for a name defined only under `vendor/`.
        if incomplete:
            counts["cut-short"] += 1
            continue
        counts["absent"] += 1
        counts[f"absent-{rung}"] += 1
        # **Reported at the position, not only counted, and only for the top tier.** A *Guessed*
        # card with the member missing is the name rung doing what its label says. A **Resolved**
        # one is the server claiming to know the receiver's class, then failing to offer a member
        # that class has: the same broken promise `rails-wrong` names, one request over.
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
    """The two shapes a total hides: where the present ones sit, and what the server claimed about
    the receiver at the ones it lost.
    """
    if not counts.get("asked"):
        return []
    out = ["rank " + "  ".join(f"{name.split('-', 1)[1]} {counts[name]}" for name in
                               ("rank-1", "rank-2-10", "rank-11-50", "rank-51+"))]
    for shape, rungs in (("absent", RUNGS), ("empty", ("resolved", "root", *RUNGS[1:]))):
        lost = [(rung, counts[f"{shape}-{rung}"])
                for rung in rungs
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
