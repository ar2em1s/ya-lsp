"""Check 4: a footnote's claim about a place is true.

Two footnotes make a claim concrete enough to check without knowing anything the server knows:

  `Defined in N places.`   `hover.rs` counts the definitions it could show, and `definition`
                           answers from the same resolution. Two functions compute the same
                           number, so they must agree **wherever both answer about the same
                           thing**, which at a variable they do not (`VARIABLE_SHAPES`).
  `Type taken from the assignment on line N`
                           the card names a line in **this** file and invites the reader to
                           look. If line N does not assign the variable under the cursor, the
                           footnote is a wrong address.

Neither needs a key, and neither judges whether the answer is any good.
"""

import re

# The footnote naming a count, and the one naming a line. Both are claims about places, checkable
# against facts: the first against `definition` at the same cursor, the second against the buffer.
PLACES = re.compile(r"Defined in (\d+) places\.")
ASSIGNED = re.compile(r"Type taken from the assignment on line (\d+)")
# **The assignment footnote is about the receiver, not the cursor.** `@foo.bar` carries it on a
# `member` cursor: `@foo`'s type came from an assignment, and `bar` is the member looked up on it.
# Gating on the `ivar` shape would make a check that cannot fire, reported as a check that passed.
RECEIVER = re.compile(r"@([A-Za-z_][A-Za-z0-9_]*)\s*&?\.\s*$")
IVAR_ASSIGN = re.compile(r"@[A-Za-z_][A-Za-z0-9_]*\s*(?:\|\||&&|\*\*|[-+*/%|&^]|<<|>>)?=(?!=)")

# **A variable's card is about its type; its jump is about its writes.** So the two counts are not
# one number computed twice, and subtracting them measures nothing.
# - At an `ivar` cursor, `definition` answers every assignment sharing its `self`
#   (`locator::variable_at`).
# - `hover` answers the class the variable was *typed* as, so a class reopened many times carries
#   `Defined in 54 places.` beside a jump naming one line.
# These claims are counted, not dropped, so what this check stopped reading is a number on the
# report, not a silence.
VARIABLE_SHAPES = ("ivar",)

FINDINGS = ("places-differs", "assignment-wrong")


def counters():
    return {"places": 0, "places-differs": 0, "places-of-a-variable": 0,
            "assignment": 0, "assignment-wrong": 0}


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
    claim = ASSIGNED.search(row.card)
    if not claim:
        return
    counts["assignment"] += 1
    lines = place.lines(row.path)
    at = int(claim.group(1)) - 1
    named_line = lines[at] if 0 <= at < len(lines) else ""
    here_line = lines[row.line] if 0 <= row.line < len(lines) else ""
    receiver = row.word if row.shape == "ivar" else None
    if receiver is None:
        seen = RECEIVER.search(here_line[:row.column])
        receiver = seen.group(1) if seen else None
    # A named receiver: the line must mention that variable. With no receiver readable from the
    # buffer (a chained or multi-line expression), fall back to the weaker claim the footnote still
    # makes: line N assigns *an* instance variable. Weaker, and still enough to catch a wrong
    # address.
    sound = f"@{receiver}" in named_line if receiver else bool(IVAR_ASSIGN.search(named_line))
    if not sound:
        counts["assignment-wrong"] += 1
        findings.append(("assignment-wrong", row.site,
                         f"{row.at} -> card names line {claim.group(1)} as the assignment"
                         + (f" to @{receiver}, which is not on it" if receiver
                            else ", which assigns no ivar")))


def line(counts):
    return (f"{counts['places-differs']} of {counts['places']} 'defined in N places' wrong; "
            f"{counts['assignment-wrong']} of {counts['assignment']} assignment lines wrong")


def summary(counts):
    return (f"{counts['places-differs']} of {counts['places']} place counts wrong; "
            f"{counts['assignment-wrong']} of {counts['assignment']} assignment lines")


def under(counts):
    """The claims this check read past: the only honest way to narrow it."""
    if not counts["places-of-a-variable"]:
        return []
    return [f"{counts['places-of-a-variable']} place counts not read — a variable's card names "
            f"its type and its jump names its writes"]
