"""Check 2: a **Resolved** card never points outside the code the application loads.

Only the top tier is judged; that is the design. A Derived or Guessed card landing in a spec is the
tier doing its job: it said it followed something, and a reader who checks finds the spec. A
**Resolved** card said the code names this.

`answers.where` holds the vocabulary of places, and why it is a deny-list, not an allow-list.

**The check may call wrong only what the server would refuse**, so the cursor side reads both of
`environment.rs`' lists. A cursor in shared test support is one the server deliberately unfences, so
an answer in a test tree there is specified behaviour, not a finding; a check that reports specified
behaviour is one readers learn to skip.

The consequence: a defect whose only symptom is an answer the fence already allows is **invisible
here**. That is a blind spot of the kind `audit.md` lists, and needs its own instrument, not a wider
rule here.
"""

from pathlib import Path

from audit.answers import in_test_support, is_defect, path_of

FINDINGS = ("resolved-outside",)


def counters():
    return {"resolved-outside": 0, "outside-targets": 0, "landed": {}}


def check(row, place, counts, findings):
    cursor_kind = place.kind(row.uri)
    # **Read once per position, not per target.** It is a fact about the cursor, and a long place
    # list would otherwise ask it once per place for the same answer.
    supported = in_test_support(row.uri)
    outside = []
    for target, _ in row.found:
        kind = place.kind(target)
        landed = counts["landed"].setdefault(row.tier or "no-card", {})
        landed[kind] = landed.get(kind, 0) + 1
        if row.tier == "resolved" and is_defect(kind, cursor_kind, supported):
            outside.append(target)
    if not outside:
        return
    # **Positions, not targets.** One name whose name-matched candidates fan out over hundreds of
    # spec files is one wrong answer a person sees once. Counting targets would measure how many
    # specs a corpus has, not how often ya-lsp is wrong. Both are kept: the targets say how far one
    # wrong answer spreads.
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
    """Where each tier's answers landed, as a vocabulary. Printed after the lanes, not under this
    check: it is context for every number above, not one check's detail. A Guessed column full of
    `gem` is the tier working.
    """
    out = []
    for rung in ("resolved", "derived", "guessed"):
        row = counts["landed"].get(rung)
        if row:
            mix = "  ".join(f"{k} {v}" for k, v in sorted(row.items(), key=lambda kv: -kv[1]))
            out.append(f"{rung:9} {mix}")
    return out
