"""The neutral key: exactly one `def` in the tree.

At `recv.some_long_name`, if exactly one `def some_long_name` exists anywhere in the repository,
a `definition` that lands anywhere else is wrong and one that lands nowhere is a miss — whatever
`recv` turns out to be. Three filters keep that honest and all three drop positions rather than
guessing: a second `def` anywhere, anything a macro or a column installs, and any name plausibly
Ruby's or a gem's rather than this corpus'.

**It costs no extra requests.** The key is by *name*, so it grades the `definition` answers lane
2 has already collected, wherever the draw happens to land on a name the corpus answers for
unambiguously. The draw is deliberately biased *away* from the key — the residue is where the
unknown is — so the ~9% of it this key covers is what that bias leaves behind rather than a
shortfall.
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
# It reads replies lane 2 already collected, so it needs no server and could be
# re-run against a recorded transcript. That is a property of *this* key and not
# of the lane — `rails` asks its own questions. See `lane1`.
ASKS = False

# The shapes this key is entitled to grade. `member` and `call` are call sites and `symbol` is
# one too — `before_action :require_logged_in_user` names a method the corpus defines once, and
# asking `definition` there is the same neutral question. `ivar` is not: `@foo` is storage rather
# than a call, and `constant` and `route` have keys of their own or none.
SHAPES = ("member", "call", "symbol")


def knowable(member):
    """A name unlikely to be Ruby's or Rails' own: six characters, and an `_` or a `?`/`!`.

    `each`, `count` and `first` are defined by everybody; `deliver_later` is not. A heuristic
    rather than a proof, and it is why this lane is reported as *precision where an answer is
    knowable* and never as a correctness rate.
    """
    body = member.rstrip("?!")
    # A route helper has no single right answer either: Rails generates `root_url` from
    # `config/routes.rb` and an application is free to write a `def root_url` beside it, which
    # lobsters does.
    if body.endswith(("_url", "_path")):
        return False
    return len(member) >= 6 and ("_" in body or member[-1] in "?!")


def build(corpus):
    """`{member: path relative to the corpus}` for every member the corpus answers unambiguously.

    Walks **everything**, specs and vendored copies included, and that is the point rather than
    an oversight: a second definition anywhere means the position has no single right answer, so
    the walk that finds it has to be wider than the one the sample is drawn from.
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
                # A second definition anywhere: remembered as blocked, never as an answer. The
                # count is kept as well as the file, because a name defined twice in *one* file
                # is ambiguous too and the file comparison alone cannot see that.
                if member in defined and defined[member] != relative:
                    blocked.add(member)
                defined.setdefault(member, relative)
                counts[member] = counts.get(member, 0) + 1
            for found in patterns.MACRO.finditer(text):
                body = patterns.macro_body(text, found)
                for symbol in patterns.SYMBOL.finditer(body):
                    blocked.add(symbol.group(1).rstrip("="))
                # The hash-key spelling, and the four names `enum` makes out of each of its
                # values. Blocking `class_name` off a `belongs_to` is the cost, and it is the
                # cost this key is built to pay: over-blocking removes a question, and the
                # alternative removes a right answer.
                for hashed in patterns.HASH_KEY.finditer(body):
                    key = hashed.group(1)
                    blocked.add(key)
                    if found.group(1) == "enum":
                        blocked.update(key + suffix for suffix in patterns.ENUM_SUFFIXES)
                        blocked.add("not_" + key)
            blocked |= patterns.prefixed(text)
            if name.endswith("schema.rb"):
                for found in patterns.COLUMN.finditer(text):
                    blocked.update((found.group(1), found.group(1) + "="))
            elif name.endswith(".sql"):
                for found in patterns.COLUMN_SQL.finditer(text):
                    blocked.update((found.group(1), found.group(1) + "="))
    blocked |= foreign_names()
    # **A definition that is only in `vendor/` is a second copy the walk cannot see, so it blocks
    # rather than answers.** This is the docstring's own rule applied to the one case where the
    # walk is narrower than reality: a gem checked into `vendor/cache/` is *also* installed under
    # the bundle's gem home, outside the corpus, and that installed copy is the one a language
    # server indexes and resolves to. Expecting the vendored path therefore states a unique
    # definition that does not exist, and no correct server can satisfy it. Measured on forem: two
    # positions, `following?` and `following_by_type`, both scored `wrong` against
    # `vendor/cache/acts_as_follower-06393d3693a1/lib/acts_as_follower/follower.rb` while the
    # answer given was the same file under
    # `~/.asdf/installs/ruby/3.3.0/.../bundler/gems/acts_as_follower-06393d3693a1/`.
    #
    # **Blocked and not path-matched**, deliberately: accepting any path ending in the library's
    # own tail would let a *wrong* copy of a genuinely duplicated name pass, which is the failure
    # this key exists to catch. Losing the question is the conservative direction — the same one
    # `patterns.HASH_KEY` already takes — and it costs `knowable` two positions on one corpus.
    # The rule is symmetric: it removes the verdict for every server, not the loser of one.
    vendored = {member for member, path in defined.items()
                if path.startswith("vendor/") or "/vendor/" in path}
    blocked |= vendored
    return {member: path for member, path in defined.items()
            if member not in blocked and counts.get(member) == 1 and knowable(member)}


def grade(corpus, drawn, answers):
    """Grade the drawn positions this key covers. No requests of its own.

    Four verdicts and they sum to `knowable`, which is the only denominator any rate here may be
    quoted over: `exact` (one location and it is the key's), `contains` (several, one of which
    is), `wrong` (locations, none of them the key's) and `silent` (no location at all). `wrong`
    is the one that names a defect rather than a gap, so it is reported at the position.
    """
    key = build(corpus)
    counts = {"knowable": 0, "exact": 0, "contains": 0, "wrong": 0, "silent": 0}
    by_shape, findings = {}, []
    # Which drawn positions this key had an opinion about at all. Lane 3 subtracts them: a
    # position a key graded is not residue, whatever verdict it got. A set rather than a count,
    # and the totals loop skips it for being neither an int nor a rate.
    #
    # **Sites, not draw indices.** A draw index only means anything against the list it indexes,
    # and lane 3 unions this set with one built from findings — which the Rails key also writes,
    # out of a draw of its own. As indices those two spaces silently overlapped and lane 3 struck
    # off whichever sampled position happened to share a number. `audit.site` has no such space.
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
    # Every rate on this line is over `knowable` and the four verdicts sum to it, so a precision
    # quoted without `silent` is a precision whose denominator holds rows the server declined.
    # They are printed together for that reason.
    return (f"{counts['exact']} exact, {counts['contains']} contained, {counts['wrong']} wrong, "
            f"{counts['silent']} silent, of {counts['knowable']} knowable")


summary = line


def under(counts):
    """Per shape, and the two numbers that matter rather than the total: the `symbol` shape is
    silent everywhere and the totals hide that behind the two shapes that are not."""
    return [f"{shape:9} {cell['exact']} exact, {cell['contains']} contained, {cell['wrong']} "
            f"wrong, {cell['silent']} silent, of {cell['knowable']}"
            for shape, cell in sorted(counts["by-shape"].items(),
                                      key=lambda kv: -kv[1]["knowable"])]
