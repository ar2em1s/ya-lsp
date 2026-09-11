"""Lane 2 — the checks that need **no key at all**.

They compare ya-lsp's answers against each other, so a violation is a self-contradiction rather
than a disagreement with somebody's opinion. That is the whole reason the lane exists: it is the
part of the audit that could one day run unattended, because nothing in it is a judgement anyone
has to make twice.

**One module per check, and the registry below is what numbers them.** A check owns four things
and they all live in its own file: the counters it keeps, the test itself, the line it prints,
and the finding kinds it raises. Adding a sixth check is adding a file and one name to `CHECKS`;
nothing in `report` or `commands` enumerates checks by hand.

    counters()                 -> a fresh dict of the counter keys this check owns
    check(row, place, counts, findings)
                               -> run it over one position; append `(kind, site, detail)`
    line(counts)               -> the `check N` line in one corpus' report
    summary(counts)            -> the same number over every corpus
    under(counts)   optional   -> extra lines indented directly under this check's line
    breakdown(counts) optional -> extra lines in the trailing block, after both lanes
    FINDINGS                   -> the finding kinds it raises, in the order to print them
"""

from audit.answers import first_place, names_an_id
from audit.lane2 import footnotes, highlight, rebase, receiver, resolved, spans
from audit.lane2.context import Corpus, Row

# The order is the numbering: `check 1` is the first entry. It is also the order findings print
# in, so a reader who has just read `check 2`'s line meets `check 2`'s findings next.
# Appended rather than slotted in: the numbering is what a reader of last week's report holds in
# their head, and a check inserted in the middle renumbers every one after it.
CHECKS = (highlight, resolved, spans, footnotes, rebase, receiver)

# What lane 2 needs asked of every position. `hover` and `definition` are the pair every lane
# reads; `documentHighlight` is check 1's alone, and check 5 asks the first two a second time.
# **`completion` is deliberately not here.** Check 6 reads it, but lane 1's key is what sends it —
# at 45% of the draw and the largest single cost in the budget, a second copy of that request is
# not a thing to own twice. See `lane2.context.Row`.
METHODS = ("textDocument/hover", "textDocument/definition", "textDocument/documentHighlight")


def run(corpus, drawn, answers, rebased=None, shifted=None, places=None):
    """Every check over one corpus' answers. Returns `(counts, findings)`.

    `findings` is `[(kind, site, detail)]` and the caller decides how many to print. The `site`
    is `audit.site` — `path:offset`, carrying no word — and it is a finding's identity in two
    places: lane 3 subtracts it from the draw, and a committed baseline records it so a later
    run can say which findings are new. The `detail` is for a person and does carry the word.

    `score` writes nothing to disk: the identifier under the cursor reaches the terminal because
    a finding nobody can go and look at is not a finding, and what the licence rule governs is
    what gets **committed** — the ledger and the baseline, neither of which holds a word.
    """
    # **Two measurements beside `tiers`, and neither is a check.** They are counted for every
    # position the way the tier is, they raise nothing, and no line here says whether a number is
    # good — `lane3.signature`'s standard, reached because a whole class of change is invisible
    # without them. `shape/tier/places` says how many places an answer named and never which came
    # first, so re-ordering a list moves no counter at all; and a card that prints an internal id
    # instead of a name leaves the tier and the count exactly where they were.
    counts = {"positions": len(drawn), "hover": 0, "definition": 0, "highlight": 0,
              "tiers": {"resolved": 0, "derived": 0, "guessed": 0},
              "first-place": {"one-library": 0, "majority": 0, "minority": 0},
              "cards-anonymous": 0,
              # Counted per distinct place rather than per position, so it is handed in already
              # summed rather than accumulated in the loop below.
              "def-places": places or {"described": 0, "undescribed": 0, "not-asked": 0}}
    for check in CHECKS:
        counts.update(check.counters())
    place = Corpus(corpus, shifted)
    findings = []
    for index, drawn_row in enumerate(drawn):
        row = Row(index, drawn_row, answers, rebased, place)
        if row.card:
            counts["hover"] += 1
            counts["tiers"][row.tier] += 1
            if names_an_id(row.card):
                counts["cards-anonymous"] += 1
        if row.found:
            counts["definition"] += 1
        where_first = first_place(place.corpus, row.found, place.library)
        if where_first:
            counts["first-place"][where_first] += 1
        if row.lit:
            counts["highlight"] += 1
        for check in CHECKS:
            check.check(row, place, counts, findings)
    return counts, findings
