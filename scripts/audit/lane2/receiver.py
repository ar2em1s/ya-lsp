"""Check 6: a card that says the receiver has no type, beside a list of a class's members.

`hover` and `completion` are asked at the **same offset**: where an editor asks the instant `.` is
typed, which is where every member cursor in this draw sits. Two answers about one receiver: lane
2's shape, *inconsistent*, with nothing to have an opinion about.

**The list is an exact discriminator, not a heuristic.** At an empty prefix, `completion::by_name`
refuses whenever the candidate set exceeds `MAX_UNTYPED_CANDIDATES`. An untyped receiver's candidate
set is the corpus' whole name universe, tens of thousands of names against a ceiling of 512. So **a
non-empty list at a bare dot means `receiver_for` typed the receiver**, and a card saying the type
is unknown is the server answering one question both ways.

  That holds because of the corpora, not by proof. Re-check it before adding a corpus: one
  whose whole name universe fits under 512 answers a bare dot from the name-based list, and
  every guessed card in it would read as a contradiction.

**Why it exists:** `hover` once printed one sentence for two different facts, *the receiver's type
is unknown* and *the receiver is a `User`, which has no such method*, and only the second is
compatible with a list built from `User`. The fix was invisible to the rest of the harness. This
check makes a completion-side change like that visible, and keeps the two sentences from merging
back.

**The mirror is counted, not reported, on purpose.** A card that *names* a class over an empty list
is the same disagreement read the other way, and a weaker claim: a fence or a filter can empty a
typed receiver's list, but nothing can conjure an untyped receiver's. Lane 1's `completion-declined`
already reports the top-tier version. So this check counts `receiver-named-empty` under its own
line, narrowed visibly, the way check 4 prints the place counts it stopped reading.
"""

# The whole sentence, not the clause `answers.GUESSED` matches. All four guessed footnotes open with
# *Matched on the method name alone*, which makes them one tier; this check reads which of the four
# followed it.
UNKNOWN = "Matched on the method name alone — the receiver's type is unknown."
# The two that name a class. Both are compatible with a list, and both are counted so the
# denominator is visible.
# - `is`: the plain miss. The receiver was typed, and the member is on no ancestor of it.
# - `was guessed from the name`: the same miss, where the type came from the name rung.
NAMED = ("Matched on the method name alone — the receiver is ",
         "Matched on the method name alone — the receiver was guessed from the name ")

FINDINGS = ("receiver-contradicted",)


def counters():
    return {"receiver-asked": 0, "receiver-unknown": 0, "receiver-contradicted": 0,
            "receiver-named": 0, "receiver-named-empty": 0}


def check(row, place, counts, findings):
    # `listed`, not `offered`: a cursor the completion key never posed (a bare word, a setter, a
    # `--no-key` run) has no list to disagree with. Reading its absence as an empty list would
    # report the whole draw as agreeing.
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
    # **A reworded sentence reads here as a clean zero.** `report.report` refuses to be believed
    # when the tier vocabulary stops matching `hover.rs`. The same coupling holds for *which*
    # guessed footnote a card carries, and only this module reads that. The shape to watch: cards
    # with a list beside them, and not one receiver sentence among them.
    #
    # Said, not raised: a narrow draw can honestly hold no receiver miss, and a finding that fires
    # on a small `-n` would be a finding nobody can read.
    if counts["receiver-asked"] and not (counts["receiver-unknown"] + counts["receiver-named"]):
        out.append("no card carried a receiver sentence — if this is not a narrow draw, the "
                   "four in hover.rs have been reworded and this check is reading nothing")
    if counts["receiver-named"]:
        out.append(f"{counts['receiver-named-empty']} of {counts['receiver-named']} cards naming "
                   f"the receiver's class got no list — counted, not reported: a typed receiver's "
                   f"list can be emptied, an untyped one's cannot be conjured")
    return out
