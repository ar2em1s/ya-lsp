"""Check 1 — `documentHighlight` and `definition` agree about this cursor.

Both requests start from the same offset in the same buffer and `highlight.rs` has already
decided which of the two halves of the crate speaks for it. So there are two ways they can
contradict each other, they are different bugs, and they are counted apart:

  `missed`   — `definition` landed in **this** file on a span `documentHighlight` did not light.
               This is the plan's phrasing of the check, literally: the highlight set is a
               superset of the same-file definition set.
  `disagree` — `documentHighlight` answered and **neither `definition` nor `hover`** did, at the
               same cursor. The empty set is a superset of nothing, so the literal containment
               above can never catch this — and this, not containment, is the shape the ivar gap
               actually takes. `highlight.rs`'s own module doc is the statement of it: the scope
               walk answers at `@foo` and the graph records no reference to one, so `definition`
               has nothing to look up. One buffer, one already-run walk, opposite answers.

**`hover` is in that condition to keep a design decision out of the defect count.** Without it
the check also fires wherever `definition` declines because a generated declaration has no source
line to point at — `synthesized.rs`'s stated rule, *no mapping means no place, never a guess* —
while `highlight` lights the name anyway, which it is entitled to do because it matches methods
by name. Measured on lobsters, that is `where`, `includes`, `find_by!`: 23 firings in 200
positions, none of them a defect. A cursor where `hover` also says nothing is a different claim
entirely — not "we know what this is and cannot say where", but "this cursor is on nothing at
all" — and that is the seam. With the condition, 40 ivar positions on lobsters split 21 silent to
19 answered, against the 319-of-657 the report measured independently.
"""

from audit.answers import covers

FINDINGS = ("missed", "disagree")


def counters():
    return {"same-file": 0, "missed": 0, "disagree": 0, "disagree-shapes": {}}


def check(row, place, counts, findings):
    same_file = [span for target, span in row.found if target == row.uri]
    if same_file:
        counts["same-file"] += len(same_file)
        missed = [s for s in same_file if not any(covers(h, s) for h in row.lit)]
        if missed:
            counts["missed"] += len(missed)
            at = ", ".join(str(s["start"]["line"] + 1) for s in missed)
            findings.append(("missed", row.site,
                             f"{row.at} -> line {at}, {len(row.lit)} highlighted"))
    elif row.lit and not row.found and not row.card:
        counts["disagree"] += 1
        by_shape = counts["disagree-shapes"]
        by_shape[row.shape] = by_shape.get(row.shape, 0) + 1
        findings.append(("disagree", row.site,
                         f"{row.at} -> {len(row.lit)} highlighted, no definition"))


def line(counts):
    return (f"{counts['missed']} of {counts['same-file']} same-file definitions unlit; "
            f"{counts['disagree']} cursors highlighted with no definition")


summary = line


def under(counts):
    """Which shapes the `disagree` half fell in — the whole finding on `ivar` is this line."""
    shapes = "  ".join(f"{k} {v}" for k, v in
                       sorted(counts["disagree-shapes"].items(), key=lambda kv: -kv[1]))
    return [f"by shape  {shapes}"] if shapes else []
