"""`audit/ledger.json`: the adjudicated residue, and the only file the audit writes.

**It is committed, so the licence rule is load-bearing here and nowhere else in this package.**
A row carries the sha256 of the line it sits on and never the line — and it does not carry the
identifier under the cursor either. An identifier is a fragment of the source, `corpora.md`'s rule
is blanket ("not a line, not a fragment, not as a ledger key"), and the path with the line number
is enough to go and look. That is the cost the rule imposes and it is the whole cost.

The row is written from named fields only. Nothing here ever copies a substring of a corpus file
into the row, which is what makes the guarantee structural rather than a habit.

    {
      "version": 1,
      "lobsters": {
        "sha": "abc123...",
        "positions": {
          "app/models/user.rb:6234": {
            "line": 211, "shape": "ivar", "hash": "ab12cd34ef567890",
            "verdict": "accepted", "why": "the scope walk answers, the graph records no ref"
          }
        }
      }
    }

**Matching, and why the hash is a guard rather than the key.** A position is found by
`path:offset` and the verdict applies only if the recorded hash still matches the line there.
Three outcomes and they are three different things:

    live    the offset is in the ledger and the hash matches — the verdict is reused
    stale   the offset is in the ledger and the hash does not — the line changed under it, so
            the verdict is **dropped rather than re-scored**: a stale verdict carried forward
            reads as a regression that never happened
    new     the offset is not in the ledger at all — residue, and the only kind that costs a
            human anything

Offsets are stable within a pin, which is why the corpus SHA is recorded per corpus rather than
folded into every key: a pin bump moves every offset in an edited file, and the SHA on the row's
own corpus is what tells a reader the ledger predates it.
"""

import json

from audit import site
from audit.config import OUT

VERSION = 1
PATH = OUT / "ledger.json"
# The three a position can be adjudicated as. `wrong` and `accepted` are both "the answer is not
# right"; what separates them is whether anyone intends to do something about it, and a ledger
# that cannot say that is a ledger where every known limitation reads as an open defect.
VERDICTS = ("correct", "wrong", "accepted")
# Every field a row may hold. The writer builds rows from this list and nothing else, so no
# corpus text can reach the file by accident. `why` is human-written and is the one place a
# person could paste a line in — that is a review question, not something code can check.
FIELDS = ("line", "shape", "hash", "verdict", "why")


# A ledger row's key and a finding's site are the same string **by construction** rather than by
# coincidence: `audit.site` is the one spelling, and lane 3 subtracts findings from the draw by
# comparing them. Two functions that happened to agree would be two functions that can stop.
key = site


def load(path=PATH):
    """The ledger, or an empty one. A missing file is the first run, not an error."""
    if not path.exists():
        return {"version": VERSION}
    got = json.loads(path.read_text())
    if got.get("version") != VERSION:
        raise ValueError(f"{path}: ledger version {got.get('version')}, expected {VERSION}")
    return got


def rows(ledger, corpus):
    """One corpus' adjudicated positions, as `{key: row}`."""
    return (ledger.get(corpus.name) or {}).get("positions") or {}


def verdict(ledger, corpus, path, offset, line_hash):
    """`("live", row)`, `("stale", row)` or `("new", None)` for one position."""
    row = rows(ledger, corpus).get(key(path, offset))
    if row is None:
        return "new", None
    return ("live" if row.get("hash") == line_hash else "stale"), row


def pending(corpus, positions):
    """Unadjudicated rows for `positions`, in the shape a person fills in and commits.

    `verdict` is left empty on purpose rather than defaulted to anything: a row that arrived with
    a verdict nobody chose is a row the ledger asserts on somebody's behalf.
    """
    out = {}
    for _, path, line, offset, shape, line_hash in positions:
        out[key(path, offset)] = {"line": line + 1, "shape": shape, "hash": line_hash,
                                  "verdict": "", "why": ""}
    return {"sha": corpus.sha, "positions": out}


def save(ledger, path=PATH):
    """Write the ledger, keeping only known fields on every row.

    The field filter is the licence guarantee made structural: whatever else a hand-edited row
    picked up, only `FIELDS` is written back, so a stray copy of a source line cannot survive a
    round trip through this function.
    """
    out = {"version": VERSION}
    for name, held in sorted(ledger.items()):
        if name == "version" or not isinstance(held, dict):
            continue
        kept = {}
        for at, row in sorted((held.get("positions") or {}).items()):
            kept[at] = {field: row[field] for field in FIELDS if field in row}
        out[name] = {"sha": held.get("sha"), "positions": kept}
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    return path
