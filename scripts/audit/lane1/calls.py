"""Lane 1's fourth key — **the bare word the corpus already wrote**, scored on `completion`.

`completion` scores a member after a dot. This is the same key one shape over and the truth is
the same shape too: the corpus wrote `render` as a statement of its own, the code runs, therefore
`render` is callable from whatever `self` is there — so a list at that cursor which does not hold
it is **wrong**, and no ranking and no client filter can rescue it.

**Why it is a key and not a check.** A card naming the member beside a list that lacks it is a
self-contradiction and would be lane 2's shape, but it is the *weaker* reading of the same
position: the corpus is the authority here, not the card. The card is still read, to bucket an
absence by the tier the server had claimed — the member key's own discipline, for its own reason.

**It closes the blind spot that hid defect 26 for as long as it existed.** Until this key, no lane
posed `completion` at a receiverless call, which is what `lane2.receiver` states from the other
side when it reads `listed` and not `offered`. So `hover` answering a bare word from one scope
while `completion` answered from another moved **0 counters over six corpora** — the third change
this instrument could not see, after the ranking defect and the test-tree fence.

# The prefix is the word, and that is the one design decision in this file

The member key poses *the instant `.` is typed*, so its prefix is empty by construction: a dot is
a trigger character and there is nothing typed after it yet. **A bare word has no trigger**, and
at an empty prefix every one of these lists is the 512-row ceiling — measured over the six corpora
at 90 closure cursors, every list `isIncomplete` at exactly `MAX_COMPLETION_ITEMS`, and 17 of them
missing a member that is on the list the moment a character arrives. A key posed there would score
the ceiling and call it the server.

So the cursor goes at the **end** of the word, which is where a developer's own is when they stop
typing and read the list. The candidate set is not a function of the prefix — `completion.rs`
collects for the receiver and filters by `tier` — so a name absent here is absent from the
candidate set outright, which is the strong claim and the one worth reporting.

# Its replies stay out of the run's `answers`

The member key writes into the run's own dict so that `lane2.receiver` can hold a card against the
list. This one deliberately does not: `Row.listed` is *(index, completion) is in answers*, and a
call cursor appearing there would move `receiver-asked` — an existing counter — for a check that
would then count a card with no receiver sentence in it. A key that quietly redefines another
lane's denominator is not additive, whatever it measures.
"""

from audit import site
from audit.answers import card_of, tier
from audit.client import uri
from audit.lane1.completion import CONTEXT, IN_FLIGHT, METHOD, bucket, names, rank_of, setter

NAME = "completion-call"
FINDINGS = ("call-absent", "call-declined")
TOTAL = "asked"
ASKS = True

# The shape this key scores: a receiverless call the corpus wrote as a statement of its own.
SHAPE = "call"


def counters():
    return {"asked": 0, "declined": 0, "answered": 0, "empty": 0, "present": 0, "absent": 0,
            "truncated": 0, "rank-1": 0, "rank-2-10": 0, "rank-11-50": 0, "rank-51+": 0,
            "absent-resolved": 0, "absent-derived": 0, "absent-guessed": 0, "absent-no-card": 0,
            "empty-resolved": 0, "empty-derived": 0, "empty-guessed": 0, "empty-no-card": 0}


def ask(corpus, client, seed, opened=None, drawn=None, answers=None):
    """Ask `completion` at every drawn `call` cursor, with the word itself as the prefix."""
    counts, findings = counters(), []
    rows = [(index, row) for index, row in enumerate(drawn or []) if row[1] == SHAPE]
    if not rows:
        return counts, findings
    # Local, not the run's `answers` — see this module's docstring for why that is not a detail.
    replies, texts, posed = {}, {}, []
    for index, (_, _, path, line, column, offset, word) in rows:
        if path not in texts:
            texts[path] = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
        # A setter is `foo = 1`, where the method is `foo=` and servers disagree about which of
        # the two to offer. The member key's filter, for the member key's reason: scoring that
        # disagreement measures a naming convention.
        if setter(texts[path], line, column, word):
            counts["declined"] += 1
            continue
        posed.append((index, path, line, column, offset, word))
        client.post((index, METHOD), METHOD, {
            "textDocument": {"uri": uri(corpus.dir / path)},
            # The end of the word, not its start. See the docstring.
            "position": {"line": line, "character": column + len(word)},
            "context": CONTEXT})
        for key, result in client.drain(down_to=IN_FLIGHT):
            replies[key] = result
    for key, result in client.drain():
        replies[key] = result

    for index, path, line, column, offset, word in posed:
        counts["asked"] += 1
        answer = replies.get((index, METHOD))
        items = answer.get("items") if isinstance(answer, dict) else answer
        if isinstance(answer, dict) and answer.get("isIncomplete"):
            counts["truncated"] += 1
        items = items or []
        rung = tier(card_of((answers or {}).get((index, "textDocument/hover")))) or "no-card"
        if not items:
            counts["empty"] += 1
            counts[f"empty-{rung}"] += 1
            if rung == "resolved":
                findings.append(("call-declined", site(path, offset),
                                 f"[{rung}] `{word}` — hover resolves the call and completion "
                                 f"answered no list at {path}:{line + 1}"))
            continue
        counts["answered"] += 1
        rank = rank_of(items, word)
        if rank is not None:
            counts["present"] += 1
            counts[bucket(rank)] += 1
            continue
        counts["absent"] += 1
        counts[f"absent-{rung}"] += 1
        # The member key's threshold, for the member key's reason: a *Guessed* card over a list
        # without the word is the name rung doing what its label says. A **Resolved** one is the
        # server naming the `def` this word calls and then not offering the word.
        if rung == "resolved":
            findings.append(("call-absent", site(path, offset),
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
        out.append(f"{counts['truncated']} lists came back truncated (isIncomplete)")
    if counts["declined"]:
        out.append(f"{counts['declined']} not asked (a setter, where the method is `name=`)")
    return out
