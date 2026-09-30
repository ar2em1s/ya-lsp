"""The neutral key: exactly one `def` in the tree.

At `recv.some_long_name`, if exactly one `def some_long_name` exists anywhere in the repository, a
`definition` landing anywhere else is wrong and one landing nowhere is a miss, whatever `recv` is.

Three filters keep that honest, and each drops positions instead of guessing:
- a second `def` anywhere;
- anything a macro or a column installs;
- any name plausibly Ruby's or a gem's, not this corpus'.

**It costs no extra requests.** The key is by *name*, so it grades the `definition` answers lane 2
already collected, wherever the draw lands on a name the corpus answers unambiguously. The draw is
biased *away* from the key on purpose (the residue is where the unknown is), so the small share this
key covers is expected, not a shortfall.
"""

import os
from pathlib import Path

from audit import site
from audit.answers import locations, path_of
from audit.lane1 import patterns
from audit.lane1.foreign import foreign_names

NAME = "key"
FINDINGS = ("key-wrong",)
TOTAL = "knowable"
# It reads replies lane 2 already collected, so it needs no server and could be re-run against a
# recorded transcript. That is this key's property, not the lane's: `rails` asks its own questions.
# See `lane1`.
ASKS = False

# The shapes this key may grade.
# - `member` and `call` are call sites, and so is `symbol`: `before_action :require_login` names a
#   method the corpus defines once, and `definition` there is the same neutral question.
# - `ivar` is not: `@foo` is storage, not a call.
# - `constant` and `route` have keys of their own, or none.
SHAPES = ("member", "call", "symbol")


def knowable(member):
    """A name unlikely to be Ruby's or Rails' own: six characters, and an `_` or a `?`/`!`.

    `each`, `count` and `first` are defined by everybody; `deliver_later` is not. A heuristic, not a
    proof, which is why this lane is reported as *precision where an answer is knowable* and never
    as a correctness rate.
    """
    body = member.rstrip("?!")
    # A route helper has no single right answer either: Rails generates `root_url` from
    # `config/routes.rb`, and an application may also write a `def root_url` beside it (lobsters
    # does).
    if body.endswith(("_url", "_path")):
        return False
    return len(member) >= 6 and ("_" in body or member[-1] in "?!")


def build(corpus):
    """`{member: path relative to the corpus}` for every member the corpus answers unambiguously.

    Walks **everything**, specs and vendored copies included, on purpose: a second definition
    anywhere means the position has no single right answer, so this walk must be wider than the
    sample's.
    """
    defined, blocked, counts = {}, set(), {}
    for here, dirs, names in os.walk(corpus.dir):
        dirs[:] = [d for d in dirs if d not in (".git", "node_modules")]
        for name in names:
            if not name.endswith((".rb", ".rake", ".sql")):
                continue
            path = Path(here, name)
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            relative = str(path.relative_to(corpus.dir))
            for found in patterns.DEF.finditer(text):
                member = found.group(1)
                # A second definition anywhere: remembered as blocked, never as an answer. The count
                # is kept as well as the file, because a name defined twice in *one* file is
                # ambiguous too, and comparing files alone cannot see that.
                if member in defined and defined[member] != relative:
                    blocked.add(member)
                defined.setdefault(member, relative)
                counts[member] = counts.get(member, 0) + 1
            for found in patterns.MACRO.finditer(text):
                body = patterns.macro_body(text, found)
                for symbol in patterns.SYMBOL.finditer(body):
                    blocked.add(symbol.group(1).rstrip("="))
                # The hash-key spelling, and the four names `enum` makes from each of its values.
                # Blocking `class_name` off a `belongs_to` is the price, and this key is built to
                # pay it: over-blocking removes a question, and the alternative removes a right
                # answer.
                for hashed in patterns.HASH_KEY.finditer(body):
                    key = hashed.group(1)
                    blocked.add(key)
                    if found.group(1) == "enum":
                        blocked.update(key + suffix for suffix in patterns.ENUM_SUFFIXES)
                        blocked.add("not_" + key)
            blocked |= patterns.prefixed(text)
            if name.endswith("schema.rb"):
                for found in patterns.COLUMN.finditer(text):
                    blocked.update(found.group(1) + suffix for suffix in patterns.COLUMN_SUFFIXES)
            elif name.endswith(".sql"):
                for found in patterns.COLUMN_SQL.finditer(text):
                    blocked.update(found.group(1) + suffix for suffix in patterns.COLUMN_SUFFIXES)
    blocked |= foreign_names()
    # **A definition only in `vendor/` blocks instead of answering**: it is a copy the walk sees of
    # a file whose real copy the walk cannot see.
    # - A gem checked into `vendor/cache/` is *also* installed under the bundle's gem home, outside
    #   the corpus.
    # - The installed copy is the one a language server indexes and resolves to.
    # - So expecting the vendored path states a unique definition that does not exist, and no
    #   correct server can satisfy it.
    #
    # **Blocked, not path-matched**, on purpose: accepting any path with the library's own tail
    # would let a *wrong* copy of a truly duplicated name pass, which is the failure this key exists
    # to catch. Losing the question is the conservative direction, the one `patterns.HASH_KEY` takes
    # too. The rule is symmetric: it removes the verdict for every server.
    vendored = {member for member, path in defined.items()
                if path.startswith("vendor/") or "/vendor/" in path}
    blocked |= vendored
    return {member: path for member, path in defined.items()
            if member not in blocked and counts.get(member) == 1 and knowable(member)}


def grade(corpus, drawn, answers):
    """Grade the drawn positions this key covers. No requests of its own.

    Four verdicts, summing to `knowable`, the only denominator any rate here may be quoted over:
    - `exact`: one location, and it is the key's;
    - `contains`: several, one of which is;
    - `wrong`: locations, none of them the key's;
    - `silent`: no location.
    `wrong` names a defect, not a gap, so it is reported at the position.
    """
    key = build(corpus)
    counts = {"knowable": 0, "exact": 0, "contains": 0, "wrong": 0, "silent": 0}
    by_shape, findings = {}, []
    # Which drawn positions this key had an opinion about. Lane 3 subtracts them: a graded position
    # is not residue, whatever the verdict. A set, not a count, and the totals loop skips it as
    # neither an int nor a rate.
    #
    # **Sites, not draw indices.** Lane 3 unions this set with one built from findings, which the
    # Rails key also writes from a draw of its own. As indices, the two spaces would overlap, and
    # lane 3 would strike off whichever sampled position shared a number. `audit.site` has no such
    # space.
    covered = set()
    for index, (_, shape, path, line, column, offset, word) in enumerate(drawn):
        if shape not in SHAPES or path.endswith(".erb"):
            continue
        expected = key.get(word)
        if not expected:
            continue
        counts["knowable"] += 1
        covered.add(site(path, offset))
        cell = by_shape.setdefault(shape, {"knowable": 0, "exact": 0, "contains": 0,
                                           "wrong": 0, "silent": 0})
        cell["knowable"] += 1
        found = locations(answers.get((index, "textDocument/definition")))
        places = [target for target, _ in found]
        want = "/" + expected.replace(os.sep, "/")
        hit = [target for target in places if (path_of(target) or target).endswith(want)]
        if not places:
            verdict = "silent"
        elif hit and len(places) == 1:
            verdict = "exact"
        elif hit:
            verdict = "contains"
        else:
            verdict = "wrong"
        counts[verdict] += 1
        cell[verdict] += 1
        if verdict == "wrong":
            got = [Path(path_of(target) or target).name for target in places[:3]]
            findings.append(("key-wrong", site(path, offset),
                             f"{shape} `{word}` at {path}:{line + 1} -> wanted {expected}, got "
                             f"{', '.join(got)}" + (f" (+{len(places) - 3} more)"
                                                    if len(places) > 3 else "")))
    counts["by-shape"] = by_shape
    counts["covered"] = covered
    return counts, findings


def line(counts):
    # Every rate on this line is over `knowable`, and the four verdicts sum to it. A precision
    # quoted without `silent` has a denominator holding rows the server declined, so they print
    # together.
    return (f"{counts['exact']} exact, {counts['contains']} contained, {counts['wrong']} wrong, "
            f"{counts['silent']} silent, of {counts['knowable']} knowable")


summary = line


def under(counts):
    """Per shape, with the two numbers that matter: the `symbol` shape is silent everywhere, and the
    totals hide that behind the other two shapes.
    """
    return [f"{shape:9} {cell['exact']} exact, {cell['contains']} contained, {cell['wrong']} "
            f"wrong, {cell['silent']} silent, of {cell['knowable']}"
            for shape, cell in sorted(counts["by-shape"].items(),
                                      key=lambda kv: -kv[1]["knowable"])]
