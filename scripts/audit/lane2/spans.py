"""Check 3 — `hover` and `definition` agree which span the cursor is on.

Both requests reach the same `locator::locate` at the same offset and both report the span it
found — `hover` as its `range`, `definition` as each link's `originSelectionRange`. They are the
same value computed twice, so a disagreement is not a difference of opinion about the answer: it
is the two requests having decided the cursor is on two different things, and whichever card the
user is reading is then about a construct they are not pointing at.

The plan words this check as *"does `definition` land inside the span `hover` named"*, which
reads as being about the target. It cannot be: a target is in another file more often than not,
and containment across two files is not a question. The span both requests name for the
**cursor** is the one thing they can be held to, so that is what is compared.
"""

from audit.answers import covers, point

FINDINGS = ("span-differs",)


def counters():
    return {"spans": 0, "span-differs": 0}


def check(row, place, counts, findings):
    if not row.named:
        return
    for span in row.origins:
        counts["spans"] += 1
        if not (covers(row.named, span) or covers(span, row.named)):
            counts["span-differs"] += 1
            findings.append(("span-differs", row.site,
                             f"{row.at} -> hover {point(row.named['start'])}-"
                             f"{point(row.named['end'])}, definition "
                             f"{point(span['start'])}-{point(span['end'])}"))
        # One span per position: `definition` reports the same origin on every link it returns,
        # so counting them all would weight a cursor with 200 name-matched candidates 200 times.
        break


def line(counts):
    return (f"{counts['span-differs']} of {counts['spans']} cursors where hover and definition "
            f"name a different span")


def summary(counts):
    return f"{counts['span-differs']} of {counts['spans']} spans disagree"
