"""Check 4: the card's count of places is true.

`Defined in N places.` is the one line a card carries that makes a claim concrete enough to check
without knowing anything the server knows: `hover.rs` counts the places it could show, and
`definition` answers from the same resolution. Two functions compute the same number, so they must
agree **wherever both answer about the same thing**, which at a variable they do not
(`VARIABLE_SHAPES`).

It needs no key, and it does not judge whether the answer is any good.
"""

import re

# The line naming a count: a claim about places, checkable against `definition` at the same cursor.
PLACES = re.compile(r"Defined in (\d+) places\.")

# **A variable's card counts every write; its jump names the writes nearest the cursor.** So the two
# counts are not one number computed twice, and subtracting them measures nothing.
# - At an `ivar` cursor, `definition` answers the writes in this file, else those of the object's
#   classes (`locator::variable_at`).
# - `hover` counts every place the variable is declared, in every file of its class.
# These claims are counted, not dropped, so what this check stopped reading is a number on the
# report, not a silence.
VARIABLE_SHAPES = ("ivar",)

FINDINGS = ("places-differs",)


def counters():
    return {"places": 0, "places-differs": 0, "places-of-a-variable": 0}


def check(row, place, counts, findings):
    if not row.card:
        return
    claim = PLACES.search(row.card)
    if claim and row.found:
        if row.shape in VARIABLE_SHAPES:
            counts["places-of-a-variable"] += 1
            claim = None
    if claim and row.found:
        counts["places"] += 1
        if int(claim.group(1)) != len(row.found):
            counts["places-differs"] += 1
            findings.append(("places-differs", row.site,
                             f"{row.at} -> card says {claim.group(1)} places, definition gives "
                             f"{len(row.found)}"))


def line(counts):
    return f"{counts['places-differs']} of {counts['places']} 'defined in N places' wrong"


def summary(counts):
    return f"{counts['places-differs']} of {counts['places']} place counts wrong"


def under(counts):
    """The claims this check read past: the only honest way to narrow it."""
    if not counts["places-of-a-variable"]:
        return []
    return [f"{counts['places-of-a-variable']} place counts not read — a variable's card counts "
            f"every write and its jump names the nearest ones"]
