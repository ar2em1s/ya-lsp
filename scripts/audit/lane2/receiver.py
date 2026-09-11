"""Check 6 — a card that says the receiver has no type, beside a list of a class's members.

`hover` and `completion` are asked at the **same byte**: the offset an editor sends the instant
`.` is typed, which is where every member cursor in this draw sits. Two answers about one
receiver, so the pair is lane 2's shape — *inconsistent*, with nothing for anyone to have an
opinion about.

**The list is an exact discriminator and not a heuristic.** At an empty prefix
`completion::by_name` refuses whenever the candidate set is larger than `MAX_UNTYPED_CANDIDATES`,
and a receiver the graph cannot type has the corpus' entire name universe as its candidate set —
26,073 rows on lobsters, 45,953 on chatwoot, against a ceiling of 512. So on these six corpora a
**non-empty list at a bare dot means `receiver_for` typed the receiver**, and a card saying the
type is unknown is the same server answering the same question both ways.

  That soundness is a property of the corpora, not a theorem, and it is the one thing to re-read
  before pointing this check at a seventh. A corpus whose whole name universe fits under 512 would
  be answered from the name-based list at a bare dot, and every guessed card in it would read as a
  contradiction. All six are over the ceiling by a factor of fifty.

**It exists because the pair self-contradicted for two releases with no counter on it.** `hover`
printed one sentence where the server knew two different things — *the receiver's type is unknown*
and *the receiver is a `User`, which has no such method* — and only the second is compatible with a
list built from `User`. Measured 2026-09-14 with a probe written for the occasion: **207 of 1,506**
`@ivar.member` cursors and **180 of 209** `Const.member` ones. The fix for it swept
**byte-identically** — 0 findings new, 4 gone, the same comparison block character for character —
which is the fourth time a completion-side change has been invisible to this harness. This check is
what makes the fifth one visible, and what keeps the sentence from merging back.

**The mirror is counted and not reported, and the asymmetry is deliberate.** A card that *names*
a class over an empty list is the same disagreement read the other way, and it is the weaker
claim: a typed receiver's list can be emptied by a fence or a filter, where an untyped receiver's
list cannot be conjured. Lane 1's `completion-declined` already reports the top-tier version of
it. So this check counts `receiver-named-empty` and prints it under its own line — narrowed
visibly, the way check 4 prints the place counts it stopped reading.
"""

# The whole sentence, not the clause `answers.GUESSED` matches on. All four guessed footnotes open
# with *Matched on the method name alone*, which is what makes them one tier; what this check reads
# is which of the four followed it.
UNKNOWN = "Matched on the method name alone — the receiver's type is unknown."
# The two that name a class. `is` is the plain miss — the receiver typed and the member is on no
# ancestor of it; `was guessed from the name` is the same miss where the type came off the name
# rung. Both are compatible with a list, and both are counted so the denominator is visible.
NAMED = ("Matched on the method name alone — the receiver is ",
         "Matched on the method name alone — the receiver was guessed from the name ")

FINDINGS = ("receiver-contradicted",)


def counters():
    return {"receiver-asked": 0, "receiver-unknown": 0, "receiver-contradicted": 0,
            "receiver-named": 0, "receiver-named-empty": 0}


def check(row, place, counts, findings):
    # `listed` and not `offered`: a cursor the completion key never posed — a bare word, a setter,
    # or a run with `--no-key` — has no list to disagree with, and reading its absence as an empty
    # one would report the whole draw as agreeing.
    if not row.card or not row.listed:
        return
    counts["receiver-asked"] += 1
    if UNKNOWN in row.card:
        counts["receiver-unknown"] += 1
        if row.offered:
            counts["receiver-contradicted"] += 1
            findings.append(("receiver-contradicted", row.site,
                             f"{row.at} -> card says the receiver's type is unknown, completion "
                             f"at the same byte offers {len(row.offered)} rows"))
        return
    if any(mark in row.card for mark in NAMED):
        counts["receiver-named"] += 1
        if not row.offered:
            counts["receiver-named-empty"] += 1


def line(counts):
    if not counts["receiver-asked"]:
        return "no completion lists to hold a card against"
    return (f"{counts['receiver-contradicted']} of {counts['receiver-unknown']} "
            f"'type is unknown' cards sit over a list, of {counts['receiver-asked']} cards "
            f"with one beside them")


def summary(counts):
    if not counts["receiver-asked"]:
        return "no completion lists to hold a card against"
    return (f"{counts['receiver-contradicted']} of {counts['receiver-unknown']} "
            f"'type is unknown' cards contradicted by the list beside them")


def under(counts):
    """The other direction, and the one way this check goes quiet without failing."""
    out = []
    # **A reworded sentence reads here as a clean zero, and that is the failure this check was
    # built to catch one level down.** `report.report` refuses to be believed when the tier
    # vocabulary stops matching `hover.rs`; the same coupling holds for *which of the four*
    # guessed footnotes a card carries, and only this module reads that. Cards with a list beside
    # them and not one receiver sentence among them is the shape. Said rather than raised: a
    # narrow draw can honestly hold no receiver miss at all, and a finding kind that fires on a
    # small `-n` would be a finding nobody can read.
    if counts["receiver-asked"] and not (counts["receiver-unknown"] + counts["receiver-named"]):
        out.append("no card carried a receiver sentence — if this is not a narrow draw, the "
                   "four in hover.rs have been reworded and this check is reading nothing")
    if counts["receiver-named"]:
        out.append(f"{counts['receiver-named-empty']} of {counts['receiver-named']} cards naming "
                   f"the receiver's class got no list — counted, not reported: a typed receiver's "
                   f"list can be emptied, an untyped one's cannot be conjured")
    return out
