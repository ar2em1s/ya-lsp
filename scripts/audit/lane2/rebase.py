"""Check 5 — the same cursor answers the same after an untouching `didChange`.

`position::Rebase`'s stated contract, as a measurement: *a deferred answer is never less than an
eager one*. Three ways it can be broken and they are counted apart, because they are three
different bugs — the answer disappearing, the answer moving, and the answer keeping its target
while changing what it claims to know about it.

The edit itself is `client.ask_rebased`'s business: newlines at the very start of the cursor's
own document and of every sampled document its eager answer pointed into, re-issued immediately
before each position's requests. Every construct is untouched and every offset moves by exactly
the number of lines its own document has taken. **Editing once up front instead measured almost
nothing** — 139 of 11,046 requests reached `Rebase`, 1 of 1,792 on discourse — because the first
empty deferred answer settles the graph and every position after it is answered by a re-indexed
one. A zero here was a property of the harness until 2026-09-15.
"""

from audit.answers import targets, tier

FINDINGS = ("rebased-lost", "rebased-differs", "rebased-tier")


def counters():
    return {"rebased": 0, "rebased-lost": 0, "rebased-differs": 0, "rebased-tier": 0,
            "rebased-fewer": 0, "rebased-more": 0}


def check(row, place, counts, findings):
    """The three tests, and they are **not** an `elif` chain.

    They were, and the chain hid the worst firing this check has produced. At
    `FollowMigrationService.new.call` on mastodon the eager answer is one target on a *Resolved*
    card carrying the signature; after the untouching edit it is 692 targets on a *Guessed* card
    reading "677 possible definitions". The target sets differ, so the chain reported it as
    `moved` and stopped — and `rebased-tier`, the counter that says the answer stopped knowing
    what it was, read 0 for the whole run. Three bugs counted apart has to mean a position can
    be scored for all three, or the two later tests only ever see positions the first two missed.
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
    # Both answered, and they are not the same answer. `fewer` is the contract — *never less
    # than an eager one* — being broken outright; `more` is the other way it goes wrong, and it
    # is not harmless: an answer that grows from one place to 692 has stopped being an answer.
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
