"""Check 7: `references` against the two answers it can be held to.

`textDocument/references` is the widest answer the server gives. No key is possible: nothing writes
down every place a name is used in a real Rails app, and a key would be `references::by_name`
rewritten in Python and scored against itself. It *can* be held to the two answers that compute part
of the same thing, and to the cursor it was asked at.

# Why lane 2, and how tight

`highlight.rs` answers a constant or a method by calling `references::find` **scoped to one
document**: same function, same `locate`, same `resolve`, `include_declaration` on. So inside the
cursor's file the two replies are one computation asked twice, with different range conversions on
the way out (`Ranges::at` here, `highlight.rs`'s own there). A disagreement is a self-contradiction,
the lane's standard, and containment holds in **both** directions.

# Three shapes, because of the scope walk

`member`, `call` and `constant` only.
- `highlight.rs` asks [`scopes::occurrences`] before the graph, so at a local, a block parameter or
  an ivar it lights a variable the graph has no target for. `references` has no such path.
- A macro's `:symbol` is the mirror image: `highlight.rs` falls through to `references::to_member`,
  and the `references` handler does not.
A check that forgets this fires on every local and every macro symbol and reports the design as a
defect.

# What a missing place may be

Four legal absences on pairing 2. Each is counted apart, not filtered: a filter nobody can see the
size of quietly eats the check.

  `outside`    the place is not in the user's own code. `references` answers the workspace's
               own code only, and `definition` answers wherever the declaration is: a gem,
               Ruby's own lib, the vendored RBS.
  `generated`  the place came from a generator's source map. `story.title` jumps to
               `db/schema.rb`'s `t.string "title"`, a line no reference list refers to.
  `redirected` the declaration is spelled differently from the cursor. `Foo.new` resolves to
               `Foo#initialize`, and `references::find` leaves the declaration out of a
               redirected resolution on purpose. The harness cannot read
               `resolution.redirected`, so it reads the **text at the place**: a declaration
               whose name is not the word under the cursor is the redirect; a `def title`
               under a `title` cursor is not.
  `guessed`    the card is the bottom tier, so `definition`'s places are the **name-based
               list**, and `references`' declarations are not. `references` stays on
               `locator::resolve` *on purpose*: that path reads no document text, so it
               derives no receiver, and a derived receiver is the one entry in a list of
               places to edit that could be wrong. `find` then adds declaration sites for
               `resolution.declarations` only, which a Guessed `definition` can exceed. This
               absorbs many places on some corpora, which is why it is counted where a
               reader sees it.

               **Its blind spot:** a real defect in `references`' declaration list at a
               Guessed cursor is invisible to pairing 2. Catching it needs a stated rule about
               which rungs the two requests share, and the crate states none.

# Not checked, on purpose

`Reference::write` against `documentHighlight`'s write spans. `references.rs` decides that
distinction once so highlight does not have to, so two readings of one computation cannot disagree,
and a green line would look like coverage without being any. It is also invisible here: a
`references` reply is `Location[]` with no kind.
"""

from pathlib import Path

from audit.answers import covers, path_of, point
from audit.client import Post

# The shapes this check may conclude from, and therefore the only ones the request is posted at.
SHAPES = ("member", "call", "constant")
# **Every third eligible position, because all of them cost more than the budget allows.**
# - The rule: a check costing **more than about a minute asks a fixed subset**; the budget and the
#   draw stay where they are. Asking every eligible position costs over a minute across the six
#   corpora, mostly on discourse.
# - Every *n*th **eligible** position, not every *n*th of the draw, so the subset is the same every
#   run and spread evenly through the three shapes.
# - **Memory is the other reason.** A discourse reference list averages thousands of places, all
#   held as `Location` dicts in one `answers` map until `lane2.run` is done with it. The stride
#   divides that too.
STRIDE = 3
# `MAX_REFERENCES` in `analysis/mod.rs`. A list at exactly the cap was truncated, so every
# comparison below would ask whether a span survived an arbitrary cut, not whether the server found
# it.
MAX_REFERENCES = 10_000

POST = Post("textDocument/references",
            params={"context": {"includeDeclaration": True}},
            shapes=SHAPES, stride=STRIDE)

FINDINGS = ("no-references", "in-file-unlit", "lit-unreferenced", "place-unreferenced",
            "cursor-unreferenced")


def counters():
    return {
        # Asked, and what came back. `eligible` is every cursor at one of `SHAPES`; `asked` is what
        # the stride kept, and it is the only honest denominator for the rest.
        "ref-eligible": 0, "ref-asked": 0, "ref-answered": 0, "ref-places": 0, "ref-capped": 0,
        # Asked and told nothing, by the kind of place the **cursor** is in: the one diagnostic that
        # says whether silence is the scope argument doing its job.
        "ref-silent": 0, "ref-silent-where": {},
        # Pairing 1, both directions.
        "ref-in-file": 0, "ref-in-file-unlit": 0, "ref-lit": 0, "ref-lit-unreferenced": 0,
        # Pairing 2, with its legal absences counted, not filtered.
        "ref-def-places": 0, "ref-def-outside": 0, "ref-def-generated": 0, "ref-def-renamed": 0,
        "ref-def-guessed": 0, "ref-def-unreferenced": 0,
        # Pairing 3.
        "ref-cursor-in": 0, "ref-cursor-out": 0,
    }


def check(row, place, counts, findings):
    if row.shape not in SHAPES:
        return
    counts["ref-eligible"] += 1
    if not row.referenced:
        return
    counts["ref-asked"] += 1
    found = row.references
    if not found:
        counts["ref-silent"] += 1
        where = counts["ref-silent-where"]
        seat = place.kind(row.uri)
        where[seat] = where.get(seat, 0) + 1
        # **Only against a highlight**, because that is the contradiction. A cursor neither request
        # speaks for is on nothing, which is check 1's business.
        if row.lit:
            findings.append(("no-references", row.site,
                             f"{row.at} -> {len(row.lit)} highlighted, no references"))
        return
    counts["ref-answered"] += 1
    counts["ref-places"] += len(found)
    if len(found) >= MAX_REFERENCES:
        counts["ref-capped"] += 1
        return
    _in_file(row, counts, findings)
    _places(row, place, counts, findings)
    _cursor(row, counts, findings)


def _in_file(row, counts, findings):
    """Pairing 1: inside the cursor's own file, the two replies are one computation."""
    here = [span for target, span in row.references if target == row.uri]
    counts["ref-in-file"] += len(here)
    counts["ref-lit"] += len(row.lit)
    unlit = [s for s in here if not any(covers(h, s) or covers(s, h) for h in row.lit)]
    if unlit:
        counts["ref-in-file-unlit"] += len(unlit)
        at = ", ".join(str(s["start"]["line"] + 1) for s in unlit[:8])
        findings.append(("in-file-unlit", row.site,
                         f"{row.at} -> {len(unlit)} of {len(here)} references unlit, "
                         f"line {at}"))
    stray = [h for h in row.lit if not any(covers(h, s) or covers(s, h) for s in here)]
    if stray:
        counts["ref-lit-unreferenced"] += len(stray)
        at = ", ".join(str(s["start"]["line"] + 1) for s in stray[:8])
        findings.append(("lit-unreferenced", row.site,
                         f"{row.at} -> {len(stray)} of {len(row.lit)} highlights with no "
                         f"reference, line {at}"))


def _places(row, place, counts, findings):
    """Pairing 2: every place `definition` named, against the list `references` returned."""
    missing = []
    for target, span in row.found:
        counts["ref-def-places"] += 1
        if any(t == target and (covers(r, span) or covers(span, r)) for t, r in row.references):
            continue
        legal = _absent_legally(row, place, target, span)
        if legal:
            counts[f"ref-def-{legal}"] += 1
            continue
        counts["ref-def-unreferenced"] += 1
        missing.append((target, span))
    if missing:
        target, span = missing[0]
        where = Path(path_of(target) or target).name
        findings.append(("place-unreferenced", row.site,
                         f"{row.at} -> {len(missing)} of {len(row.found)} definition places "
                         f"unreferenced, first {where}:{span['start']['line'] + 1}"))


def _absent_legally(row, place, target, span):
    """Which legal absence this place is, or `None`, checked in this order.

    Kind before text: a schema line is `generated` whatever word is on it, and a place in a gem is
    `outside` whether or not it is spelled like the cursor.
    """
    seat = place.kind(target)
    if seat in ("gem", "generated-uri"):
        return "outside"
    if seat == "generated-source":
        return "generated"
    wrote = _text_at(place, target, span)
    if wrote is not None and _bare(wrote) != _bare(row.word):
        return "renamed"
    # Last, so a redirect at a Guessed cursor is still counted as the redirect it is.
    if row.tier == "guessed":
        return "guessed"
    return None


def _cursor(row, counts, findings):
    """Pairing 3: the cursor is in its own answer.

    Nobody stated this as an invariant, so it is a counter first. A cursor past the cap, or in code
    `references` does not scope to, may legally be absent, and both cases have already returned
    before this runs.
    """
    at = (row.line, row.column)
    inside = any(target == row.uri and point(span["start"]) <= at <= point(span["end"])
                 for target, span in row.references)
    if inside:
        counts["ref-cursor-in"] += 1
        return
    counts["ref-cursor-out"] += 1
    findings.append(("cursor-unreferenced", row.site,
                     f"{row.at} -> {len(row.references)} references, none on the cursor"))


def _text_at(place, target, span):
    """The text a place's own range covers, or `None` where it is not one word.

    A multi-line span is not a name and is not read. Columns are utf-32 code points, which is what
    slicing a Python `str` counts, and why this harness negotiates utf-32 at `initialize`.
    """
    path = path_of(target)
    if path is None or span["start"]["line"] != span["end"]["line"]:
        return None
    try:
        relative = str(Path(path).resolve().relative_to(Path(place.dir).resolve()))
    except ValueError:
        return None
    lines = place.lines(relative)
    at = span["start"]["line"]
    if at >= len(lines):
        return None
    return lines[at][span["start"]["character"]:span["end"]["character"]]


def _bare(word):
    """One name, without the punctuation two spellings of it differ by.

    `attr_reader :title` files a declaration whose name span is the symbol, and `def title=` is how
    the same attribute's writer is spelled; neither is a redirect. `Foo.new` -> `initialize`
    survives all of this, and it is the case the comparison is for.
    """
    return (word or "").strip().strip(":\"'").rstrip("?!=")


def _stride():
    return "" if STRIDE == 1 else f", 1 in {STRIDE}"


def line(counts):
    if counts["ref-asked"] and not counts["ref-answered"]:
        # The params trap, caught from inside. `ReferenceParams.context` is required: a post without
        # it fails `parse_params` and comes back `null`, exactly like a cursor with no references. A
        # clean population of zeroes is the one thing this check cannot tell from success, so it
        # refuses to be believed instead.
        return (f"BROKEN   references answered at none of {counts['ref-asked']} cursors — the "
                f"post is malformed, see `Post.params` in client.py")
    return (f"{counts['ref-in-file-unlit']} of {counts['ref-in-file']} in-file references "
            f"unlit, {counts['ref-lit-unreferenced']} of {counts['ref-lit']} highlights "
            f"unreferenced; {counts['ref-def-unreferenced']} of {counts['ref-def-places']} "
            f"definition places absent ({counts['ref-def-renamed']} redirected, "
            f"{counts['ref-def-outside']} outside, {counts['ref-def-generated']} generated, "
            f"{counts['ref-def-guessed']} guessed)")


summary = line


def under(counts):
    if not counts["ref-eligible"]:
        return []
    rows = [f"asked     {counts['ref-asked']} of {counts['ref-eligible']} "
            f"{'/'.join(SHAPES)} cursors{_stride()}; {counts['ref-answered']} answered, "
            f"{counts['ref-places']} places, {counts['ref-capped']} at the {MAX_REFERENCES} cap"]
    if counts["ref-silent"]:
        seats = "  ".join(f"{k} {v}" for k, v in
                          sorted(counts["ref-silent-where"].items(), key=lambda kv: -kv[1]))
        rows.append(f"silent    {counts['ref-silent']} asked and told nothing, by where the "
                    f"cursor sits: {seats}")
    asked = counts["ref-cursor-in"] + counts["ref-cursor-out"]
    if asked:
        rows.append(f"cursor    {counts['ref-cursor-out']} of {asked} cursors not inside their "
                    f"own answer")
    return rows
