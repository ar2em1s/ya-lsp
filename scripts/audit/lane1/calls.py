"""Lane 1's `calls` key: **the bare word the corpus already wrote**, scored on `completion`.

The `completion` key scores a member after a dot; this is the same key one shape over. The corpus
wrote `render` as a statement of its own and the code runs, so `render` is callable from whatever
`self` is there. A list at that cursor without it is **wrong**, and no ranking or client filter can
rescue it.

**A key, not a check.** A card naming the member beside a list that lacks it would be lane 2's
shape, but it is the *weaker* reading: the corpus is the authority here, not the card. The card is
still read, to bucket an absence by the tier the server claimed, as the member key does.

**It covers a blind spot.** Without this key, no lane poses `completion` at a receiverless call. So
`hover` answering a bare word from one scope while `completion` answers from another would move no
counter.

# The prefix is the word: the one design decision here

The member key poses *the instant `.` is typed*, so its prefix is empty by construction. **A bare
word has no trigger character**, and at an empty prefix every one of these lists is the 512-row
ceiling (`isIncomplete` at exactly `MAX_COMPLETION_ITEMS`), some missing a member that appears as
soon as a character is typed. A key posed there would score the ceiling and blame the server.

So the cursor goes at the **end** of the word, where a developer's cursor is when they stop typing
and read the list. The candidate set does not depend on the prefix (`completion.rs` collects for the
receiver and filters by `tier`), so a name absent here is absent from the candidate set outright:
the strong claim, and the one worth reporting.
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
            "absent-resolved": 0, "absent-guessed": 0, "absent-no-card": 0,
            "empty-resolved": 0, "empty-guessed": 0, "empty-no-card": 0}


def ask(corpus, client, seed, opened=None, drawn=None, answers=None):
    """Ask `completion` at every drawn `call` cursor, with the word itself as the prefix."""
    counts, findings = counters(), []
    rows = [(index, row) for index, row in enumerate(drawn or []) if row[1] == SHAPE]
    if not rows:
        return counts, findings
    # Local, not the run's `answers`: the module docstring says why.
    replies, texts, posed = {}, {}, []
    for index, (_, _, path, line, column, offset, word) in rows:
        if path not in texts:
            texts[path] = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
        # A setter is `foo = 1`, where the method is `foo=` and servers disagree about which of the
        # two to offer. Dropped for the member key's reason: scoring that disagreement measures a
        # naming convention.
        if setter(texts[path], line, column, word):
            counts["declined"] += 1
            continue
        posed.append((index, path, line, column, offset, word))
        client.post((index, METHOD), METHOD, {
            "textDocument": {"uri": uri(corpus.dir / path)},
            # The end of the word, not its start. See the module docstring.
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
        # The member key's threshold, for the same reason: a *Guessed* card over a list without the
        # word is the name rung doing what its label says. A **Resolved** one is the server naming
        # the `def` this word calls, then not offering the word.
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
                for rung in ("resolved", "guessed", "no-card")
                if counts.get(f"{shape}-{rung}")]
        if lost:
            out.append(f"{shape} " + "  ".join(f"{rung} {count}" for rung, count in lost))
    if counts["truncated"]:
        out.append(f"{counts['truncated']} lists came back truncated (isIncomplete)")
    if counts["declined"]:
        out.append(f"{counts['declined']} not asked (a setter, where the method is `name=`)")
    return out
