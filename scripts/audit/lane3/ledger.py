"""`audit/ledger.json`: the adjudicated residue, and the only file the audit writes.

**It is committed, so the licence rule is load-bearing here.**
- A row carries the sha256 of its line, never the line.
- It never carries the identifier under the cursor either: an identifier is a fragment of the
  source, and `corpora.md`'s rule is blanket ("not a line, not a fragment, not as a ledger key").
- The path and line number are enough to go and look. That is the rule's whole cost.

Rows are written from named fields only; nothing here copies a substring of a corpus file into a
row. That makes the guarantee structural, not a habit.

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

**Matching: the hash guards; it is not the key.** A position is found by `path:offset`, and its
verdict applies only while the recorded hash still matches the line there:

    live    in the ledger, and the hash matches: the verdict is reused
    stale   in the ledger, and the hash does not: the line changed, so the verdict is
            **dropped, not re-scored**; carried forward, it would read as a regression that
            never happened
    new     not in the ledger: residue, and the only kind that costs a person anything

Offsets are stable within a pin, so the corpus SHA is recorded once per corpus, not in every key. A
pin bump moves every offset in an edited file, and the SHA tells a reader the ledger predates it.
"""

import json

from audit import site
from audit.config import OUT

VERSION = 1
PATH = OUT / "ledger.json"
# The three verdicts a position can get. `wrong` and `accepted` both mean "the answer is not right";
# they differ in whether anyone intends to fix it. Without that, every known limitation reads as an
# open defect.
VERDICTS = ("correct", "wrong", "accepted")
# Every field a row may hold. The writer builds rows from this list only, so no corpus text reaches
# the file by accident. `why` is written by a person and is the one place someone could paste a line
# in: a review question, not something code can check.
FIELDS = ("line", "shape", "hash", "verdict", "why")


# A ledger row's key and a finding's site are the same string **by construction**: `audit.site` is
# the one spelling, and lane 3 subtracts findings from the draw by comparing them. Two functions
# that merely agreed could stop agreeing.
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

    `verdict` is left empty on purpose: a row arriving with a verdict nobody chose would be the
    ledger asserting on somebody's behalf.
    """
    out = {}
    for _, path, line, offset, shape, line_hash in positions:
        out[key(path, offset)] = {"line": line + 1, "shape": shape, "hash": line_hash,
                                  "verdict": "", "why": ""}
    return {"sha": corpus.sha, "positions": out}


def save(ledger, path=PATH):
    """Write the ledger, keeping only known fields on every row.

    The field filter makes the licence guarantee structural: whatever a hand-edited row picked up,
    only `FIELDS` is written back, so a stray source line cannot survive a round trip through this
    function.
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
