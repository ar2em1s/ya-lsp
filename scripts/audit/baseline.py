"""`audit/baseline.json`: one run's numbers, so the next run can say what moved.

One sweep says *38 Resolved cards land in a test tree*, and nobody can read that alone. What means
something is the change: was it 38 last time? That needs last time written down. This module is the
written-down form and the comparison; `report.moved` prints it.

**It is committed, so the licence rule governs it exactly as it governs the ledger.** A row holds
integers, a git SHA, and each finding's `audit.site` (a relative path and an offset). Never a word,
a line, or a `detail` string: the detail is for a person at a terminal and carries the identifier
under the cursor. `save` rebuilds every row from named fields, so this is structural, as in
`lane3.ledger`.

**Two runs are comparable or they are not; there is no third answer.** Across a moved pin or a
different seed every offset shifts and the draw is a different set of questions, so every counter
"moves" and none of it means anything. `why_not` checks this per corpus, and a corpus that fails is
reported as not compared.

    {
      "version": 1,
      "lobsters": {
        "sha": "abc123...",
        "draw": {"seed": "0", "per-file": 32, "cap": 0, "eager-only": false},
        "counts": {"positions": 777, "check/missed": 0, "key/exact": 303, "tiers.guessed": 190},
        "findings": {"missed": ["app/models/user.rb:6234"]}
      }
    }
"""

import json

from audit.config import OUT

VERSION = 1
PATH = OUT / "baseline.json"
# Every field a corpus' row may hold; the writer builds rows from this list only.
FIELDS = ("sha", "draw", "counts", "findings")
# What makes two runs ask the same *questions*. Changing any of these means re-baselining, not
# diffing.
DRAW = ("seed", "per-file", "cap", "eager-only")


def drawn_as(args):
    """The four facts that decide whether two runs asked the same thing."""
    return {"seed": str(args.seed), "per-file": int(args.per_file),
            "cap": int(getattr(args, "n", 0) or 0),
            "eager-only": bool(getattr(args, "eager_only", False))}


def numbers(counts, prefix=""):
    """Every countable thing in a `counts` dict, flattened to `name -> int`.

    Two levels, because that is how deep counters go: a check keeps ints, plus a dict of ints where
    a total would hide a shape. Anything else (a set of covered sites, a list of warnings, the
    two-level `landed` and `by-shape` tables) is a working value, not a measurement, and recording
    it would make the baseline diff it.
    """
    out = {}
    for name, value in counts.items():
        if isinstance(value, bool):
            continue
        if isinstance(value, int):
            out[prefix + name] = value
        elif isinstance(value, dict) and value and all(
                isinstance(count, int) and not isinstance(count, bool)
                for count in value.values()):
            for sub, count in value.items():
                out[f"{prefix}{name}.{sub}"] = count
    return out


def of(corpus, args, counts, findings):
    """One corpus' row. Reads the same `counts` and `findings` the terminal report just printed."""
    every = numbers(counts)
    # The keys live one level down, and their counter names collide with lane 2's (both have `wrong`
    # and `silent`). `key/` and `rails/` are the report's own labels for them.
    for name, cell in (counts.get("keys") or {}).items():
        every.update(numbers(cell, prefix=f"{name}/"))
    sites = {}
    for kind, where, _ in findings:
        sites.setdefault(kind, []).append(where)
    return {"sha": corpus.sha, "draw": drawn_as(args), "counts": every,
            "findings": {kind: sorted(rows) for kind, rows in sites.items()}}


def load(path=PATH):
    """The baseline, or an empty one. A missing file is the first run, not an error."""
    if not path.exists():
        return {"version": VERSION}
    got = json.loads(path.read_text())
    if got.get("version") != VERSION:
        raise ValueError(f"{path}: baseline version {got.get('version')}, expected {VERSION}")
    return got


def save(baseline, path=PATH):
    """Write the baseline, keeping only known fields on every row.

    The field filter makes the licence guarantee structural, as in `lane3.ledger.save`: whatever a
    row picked up, only `FIELDS` is written back.
    """
    out = {"version": VERSION}
    for name, row in sorted(baseline.items()):
        if name == "version" or not isinstance(row, dict):
            continue
        out[name] = {field: row[field] for field in FIELDS if field in row}
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    return path


def why_not(before, now):
    """Why these two rows may not be diffed, or `None`.

    - The corpus SHA first: a pin bump moves every offset in every edited file, so every counter
      would move and every finding would read as both new and gone.
    - Then the draw, field by field: "the draw changed" is not actionable, and "seed 0 -> 7" is.
    """
    if before.get("sha") != now.get("sha"):
        return (f"the pin moved: {(before.get('sha') or '?')[:8]} -> "
                f"{(now.get('sha') or '?')[:8]}")
    was, has = before.get("draw") or {}, now.get("draw") or {}
    changed = [f"{field} {was.get(field)!r} -> {has.get(field)!r}"
               for field in DRAW if was.get(field) != has.get(field)]
    return "the draw changed: " + ", ".join(changed) if changed else None


def moved(before, now):
    """`[(name, was, is_now)]` for every counter that changed, with `None` where one did not exist.

    A counter absent from one side is **not** zero there. A check added since the baseline would
    read `0 -> 38`, which claims a regression that never happened. So `None` on either side prints
    as a new or a dropped counter.
    """
    was, has = before.get("counts") or {}, now.get("counts") or {}
    out = []
    for name in sorted(set(was) | set(has)):
        if was.get(name) != has.get(name):
            out.append((name, was.get(name), has.get(name)))
    return out


def found(before, now):
    """`(new, gone)`: the findings this run raised that the baseline did not, and the reverse.

    Findings are the half of a diff that needs no opinion about direction.
    - A counter going up may be good or bad, and this module refuses to guess: encoding that would
      put a table of judgements outside the checks that own them.
    - A *finding* is a defect the harness has already named, so a new one is a regression and a gone
      one is a fix. That is why they lead the report.
    """
    was, has = before.get("findings") or {}, now.get("findings") or {}
    new, gone = [], []
    for kind in sorted(set(was) | set(has)):
        # Multisets, in case one site raises one kind twice: a set difference would report a real
        # second firing as no change.
        left = list(was.get(kind) or [])
        for where in has.get(kind) or []:
            if where in left:
                left.remove(where)
            else:
                new.append((kind, where))
        gone.extend((kind, where) for where in left)
    return new, gone
