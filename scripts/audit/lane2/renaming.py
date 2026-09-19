"""Check 10: the box a rename opens sits exactly where the cursor's own word is lit.

`prepareRename` answers with **one** range: the part of the rename the cursor stands in. An editor
puts its rename box over that range and pre-fills it with the text inside, so the range *is* the
answer a user sees before typing. `documentHighlight` lights the same word from the same buffer at
the same offset, so where both answer, they are two readings of one span.

**Not one computation, which makes this a check, not a tautology.** `highlight.rs` lights what the
scope walk or the graph says is here. `rename::plan` re-reads the file and **narrows**: rubydex
records the name span of `Error = Class.new(StandardError)` as the whole assignment, so `narrow`
finds the one whole-word occurrence inside it, and refuses when there are two. The narrowing is
where the two can differ, and a rename box over the wrong text has a user typing a new name into an
expression.

**A qualified name is where they differ legally.** At the `Source` of `class Foo < Reports::Source`,
`prepareRename` answers the leaf, while `documentHighlight`, `hover` and `definition` all answer the
whole path. Three surfaces answer *what is the reference here*; the fourth answers *what text will
be replaced*, which is the leaf only, since replacing the path would eat the namespace (what
`narrow` prevents). Two right answers to two questions, so the row is counted apart, not raised.

**One shape, picked by the server's own rule.** `rename.rs` renames locals through `scopes` and
constants through the graph, and refuses a method out loud (its uses are found by name). The draw
has no `local` shape, so `constant` is the only shape a rename can answer at, and measurement
agrees: across a whole draw, every range that came back was at a constant. Asking only there cuts
the requests by most of the draw, and stops the sweep provoking a spoken refusal at every method.

**The denominator is small, and that is the server's doing.** A constant is refused unless *every*
place it is written is somewhere ya-lsp would edit, and in a Rails app a model's declaration is
partly generated. A run where the check reads *none* is what the `BROKEN` line says out loud.

The rest of this request lives in the declaration-site stratum: a `class` or `module` name is
renameable, and the draw excludes all of them by design (`sample.declarations`).
"""

from audit.answers import covers, point
from audit.client import Post

FINDINGS = ("rename-span", "rename-unlit")

# Only the shape is set: a position is all it needs. It is cheap, because the server refuses most
# requests in its first two lines. A `Post`, not a bare string, because a check that sends its own
# request names it in its own file; `lane2.METHODS` holds only the name.
POST = Post("textDocument/prepareRename", shapes=("constant",))


def counters():
    return {"rename-asked": 0, "rename-ranges": 0, "rename-unlit": 0, "rename-qualified": 0,
            "rename-compared": 0, "rename-span": 0, "rename-shapes": {}}


def _at_cursor(row):
    """The highlighted span the cursor stands in, or None."""
    here = {"start": {"line": row.line, "character": row.column},
            "end": {"line": row.line, "character": row.column}}
    return next((span for span in row.lit if covers(span, here)), None)


def check(row, place, counts, findings):
    if not row.asked_rename:
        return
    counts["rename-asked"] += 1
    if not row.renaming:
        return
    counts["rename-ranges"] += 1
    by_shape = counts["rename-shapes"]
    by_shape[row.shape] = by_shape.get(row.shape, 0) + 1
    lit = _at_cursor(row)
    if lit is None:
        # Two answers about one offset disagree about whether a name is here at all: the rename box
        # would open over a word the highlight walk did not light.
        counts["rename-unlit"] += 1
        findings.append(("rename-unlit", row.site,
                         f"{row.at} -> rename offers a range, {len(row.lit)} highlighted"))
        return
    if _qualified(row, place, lit):
        # The lit span starts before the cursor's own word: a qualified name, where the two answer
        # different questions and both are right. Counted, never raised.
        counts["rename-qualified"] += 1
        return
    counts["rename-compared"] += 1
    if point(lit["start"]) != point(row.renaming["start"]) \
            or point(lit["end"]) != point(row.renaming["end"]):
        counts["rename-span"] += 1
        findings.append(("rename-span", row.site,
                         f"{row.at} -> rename {_span(row.renaming)}, highlight {_span(lit)}"))


def _qualified(row, place, lit):
    """Is the lit span a path whose leaf the cursor is on, like the `Source` of `Foo::Source`?

    Two conditions, read off the source, not the shapes: the span opens **before** the cursor on the
    cursor's line, and the two characters right before the cursor are `::`. That is the one
    construct where a reference is several names and a rename replaces only the last.
    """
    start = point(lit["start"])
    if start[0] != row.line or start[1] >= row.column or row.column < 2:
        return False
    lines = place.lines(row.path)
    if row.line >= len(lines):
        return False
    return lines[row.line][row.column - 2:row.column] == "::"


def _span(span):
    start, end = point(span["start"]), point(span["end"])
    return f"{start[0] + 1}:{start[1]}-{end[0] + 1}:{end[1]}"


def line(counts):
    if counts["rename-asked"] and not counts["rename-ranges"]:
        # The failure this check cannot see from its own counters: a request refused for a reason
        # that is not the server's (a params shape it cannot parse, a capability it was never told
        # about) reads exactly like a corpus where nothing is renameable.
        return (f"BROKEN   {counts['rename-asked']} cursors asked and not one range came back; "
                f"prepareRename is answering nothing")
    return (f"{counts['rename-span']} of {counts['rename-compared']} rename ranges are not the "
            f"lit span; {counts['rename-ranges']} of {counts['rename-asked']} cursors renameable")


summary = line


def under(counts):
    """What was renameable, and the cursors where only one of the two answered."""
    said = []
    shapes = "  ".join(f"{name} {count}" for name, count in
                       sorted(counts["rename-shapes"].items(), key=lambda kv: -kv[1]))
    if shapes:
        said.append(f"by shape  {shapes}")
    if counts["rename-qualified"]:
        said.append(f"qualified {counts['rename-qualified']} cursors on the leaf of a path, where "
                    f"the box is the leaf and the light is the whole name — counted, not "
                    f"reported")
    if counts["rename-unlit"]:
        said.append(f"unlit     {counts['rename-unlit']} renameable cursors nothing highlighted")
    return said
