"""Check 3: `hover` and `definition` agree which span the cursor is on.

Both requests reach the same `locator::locate` at the same offset and report the span it found:
`hover` as its `range`, `definition` as each link's `originSelectionRange`. One value computed
twice, so a disagreement is not about the answer. The two requests decided the cursor is on two
different things, and whichever card the user reads is about a construct they are not pointing at.

**Why the cursor's span, not the target.** A target is usually in another file, and containment
across two files is not a question. The span both requests name for the **cursor** is what they can
be held to.
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
        # One span per position: `definition` reports the same origin on every link, so counting
        # each would weight a cursor with hundreds of name-matched candidates hundreds of times.
        break


def line(counts):
    return (f"{counts['span-differs']} of {counts['spans']} cursors where hover and definition "
            f"name a different span")


def summary(counts):
    return f"{counts['span-differs']} of {counts['spans']} spans disagree"
