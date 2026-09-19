"""Check 5: the same cursor answers the same after an untouching `didChange`.

`position::Rebase`'s contract, as a measurement: *a deferred answer is never less than an eager
one*. It can break three ways, counted apart because they are three bugs: the answer disappears, the
answer moves, or the answer keeps its target but changes what it claims to know.

The edit is `client.ask_rebased`'s business: newlines at the very start of the cursor's document,
and of every sampled document its eager answer pointed into, re-sent right before each position's
requests. Every construct is untouched, and every offset moves by exactly the lines its own document
has taken. (Editing once up front measures almost nothing; `ask_rebased` says why.)
"""

from audit.answers import targets, tier

FINDINGS = ("rebased-lost", "rebased-differs", "rebased-tier")


def counters():
    return {"rebased": 0, "rebased-lost": 0, "rebased-differs": 0, "rebased-tier": 0,
            "rebased-fewer": 0, "rebased-more": 0}


def check(row, place, counts, findings):
    """The three tests, and they are **not** an `elif` chain.

    One position can break all three: an eager answer of one target on a *Resolved* card can come
    back, after the untouching edit, as hundreds of targets on a *Guessed* card. A chain would
    report that as `moved` and stop, and `rebased-tier` (the answer no longer knowing what it was)
    would read 0. Three bugs counted apart means a position can score for all three.
    """
    if not row.deferred:
        return
    if row.card or row.found:
        counts["rebased"] += 1
    if (row.card and not row.after_card) or (row.found and not row.after):
        counts["rebased-lost"] += 1
        findings.append(("rebased-lost", row.site,
                         f"{row.at} -> "
                         f"{'card' if row.card and not row.after_card else 'definition'} gone "
                         f"after an untouching edit"))
    # Both answered, with different answers. `fewer` breaks the contract (*never less than an eager
    # one*) outright. `more` is the other failure, and not harmless: an answer that grows from one
    # place to hundreds has stopped being an answer.
    if row.found and row.after and \
            targets(row.found) != targets(row.after, place.shifted, row.index):
        was, now = targets(row.found), targets(row.after, place.shifted, row.index)
        counts["rebased-differs"] += 1
        gone, gained = was - now, now - was
        if gone:
            counts["rebased-fewer"] += 1
        if gained and not gone:
            counts["rebased-more"] += 1
        moved = ", ".join(part for part in (f"{len(gone)} lost" if gone else "",
                                            f"{len(gained)} gained" if gained else "") if part)
        findings.append(("rebased-differs", row.site,
                         f"{row.at} -> {len(was)} targets eagerly, {len(now)} after the edit, "
                         f"{moved}"))
    if row.card and row.after_card and row.tier != tier(row.after_card):
        counts["rebased-tier"] += 1
        findings.append(("rebased-tier", row.site,
                         f"{row.at} -> {row.tier} eagerly, {tier(row.after_card)} after the "
                         f"edit"))


def line(counts):
    return (f"{counts['rebased-lost']} answers lost, {counts['rebased-differs']} moved "
            f"({counts['rebased-fewer']} lost targets, {counts['rebased-more']} only gained), "
            f"{counts['rebased-tier']} changed tier after an untouching edit, "
            f"of {counts['rebased']}")


def summary(counts):
    return (f"{counts['rebased-lost']} lost, {counts['rebased-differs']} moved "
            f"({counts['rebased-fewer']} lost targets, {counts['rebased-more']} only gained), "
            f"{counts['rebased-tier']} changed tier, of {counts['rebased']}")
