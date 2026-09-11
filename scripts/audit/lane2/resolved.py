"""Check 2 — a **Resolved** card never points outside the code the application loads.

Only the top tier is judged, and that is the whole design of the check. A Derived or Guessed card
landing in a spec is the tier doing its job: it said out loud that it followed something, and a
reader who goes and checks finds the spec. A **Resolved** card said the code names this.

The vocabulary of places, and why it is a deny-list rather than an allow-list, is in
`answers.where`.
"""

from pathlib import Path

from audit.answers import is_defect, path_of

FINDINGS = ("resolved-outside",)


def counters():
    return {"resolved-outside": 0, "outside-targets": 0, "landed": {}}


def check(row, place, counts, findings):
    cursor_kind = place.kind(row.uri)
    outside = []
    for target, _ in row.found:
        kind = place.kind(target)
        landed = counts["landed"].setdefault(row.tier or "no-card", {})
        landed[kind] = landed.get(kind, 0) + 1
        if row.tier == "resolved" and is_defect(kind, cursor_kind):
            outside.append(target)
    if not outside:
        return
    # **Positions, not targets.** One `Spree::Base` whose name-matched candidates fan out over
    # 200 spec files is one wrong answer a person sees once, and counting the targets reported
    # it as 3,802 — a number that says more about how many specs solidus has than about how
    # often ya-lsp is wrong. Both are kept: the targets say how far one wrong answer spreads.
    counts["resolved-outside"] += 1
    counts["outside-targets"] += len(outside)
    short = path_of(outside[0]) or outside[0]
    try:
        short = str(Path(short).resolve().relative_to(Path(place.dir).resolve()))
    except ValueError:
        pass
    more = f" (+{len(outside) - 1} more)" if len(outside) > 1 else ""
    findings.append(("resolved-outside", row.site, f"{row.at} -> {short}{more}"))


def line(counts):
    return (f"{counts['resolved-outside']} Resolved cards land in a test tree "
            f"({counts['outside-targets']} targets)")


summary = line


def breakdown(counts):
    """Where each tier's answers landed, as a vocabulary. Printed after the lanes, not under
    this check: it is the context every number above is read against rather than one check's
    detail, and a Guessed column full of `gem` is the tier working."""
    out = []
    for rung in ("resolved", "derived", "guessed"):
        row = counts["landed"].get(rung)
        if row:
            mix = "  ".join(f"{k} {v}" for k, v in sorted(row.items(), key=lambda kv: -kv[1]))
            out.append(f"{rung:9} {mix}")
    return out
