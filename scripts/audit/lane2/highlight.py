"""Check 1: `documentHighlight` and `definition` agree about this cursor.

Both start from the same offset in the same buffer, and `highlight.rs` has already decided which
half of the crate speaks for it. They can contradict each other two ways, which are different bugs,
counted apart:

  `missed`    `definition` landed in **this** file on a span `documentHighlight` did not light.
              The literal check: the highlight set contains the same-file definition set.
  `disagree`  `documentHighlight` answered and **neither `definition` nor `hover`** did, at the
              same cursor. The empty set is contained in anything, so containment can never
              catch this, and this is the shape the ivar gap takes (`highlight.rs`' module
              doc): the scope walk answers at `@foo`, the graph records no reference to it, so
              `definition` has nothing to look up. One buffer, opposite answers.

**`hover` is in that condition to keep a design decision out of the defect count.** Without it, the
check fires wherever `definition` declines because a generated declaration has no source line
(`synthesized.rs`: *no mapping means no place, never a guess*), while `highlight` lights the name
anyway, as it may, since it matches methods by name. `where`, `includes` and `find_by!` fire that
way, and none is a defect. A cursor where `hover` is silent too makes a different claim: not "we
know what this is and cannot say where", but "this cursor is on nothing". That is the seam.

**A few `disagree` rows are correct refusals of a private method.** Where the name is private
everywhere it is declared and the written receiver is not `self`, Ruby itself would raise, and both
refusals are right. They are not subtracted: a text scan for privacy could never catch a regression
(a server that started *answering* there would just leave the bucket) and could hide one, so it is
not worth its cost.
"""

import re

from audit.answers import covers

FINDINGS = ("missed", "disagree")

# The receiver written right before the cursor, if any. A `member` is `recv.name` and this reads
# `recv`; a bare call has none, and `self.name` is the one receiver allowed to reach a private
# method.
RECEIVER = re.compile(r"([A-Za-z_@][A-Za-z0-9_]*[?!]?)\s*\.\s*$")


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
    """Which shapes the `disagree` half fell in: for `ivar`, this line is the whole finding."""
    said = []
    shapes = "  ".join(f"{k} {v}" for k, v in
                       sorted(counts["disagree-shapes"].items(), key=lambda kv: -kv[1]))
    if shapes:
        said.append(f"by shape  {shapes}")
    return said
