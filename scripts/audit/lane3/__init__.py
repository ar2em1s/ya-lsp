"""Lane 3: the adjudicated residue, the positions no rule decides.

Lane 1 says *wrong* against the source; lane 2 says *inconsistent* against the server's other
answers. Whatever both are silent about lands here, and only a person can settle it. So the lane's
job is to make that work **cost once**: a verdict already in `audit/ledger.json` is reused, and only
a new position asks anyone anything.

**No cap on how many positions this lane holds.** A limit would be an invented number, and how much
residue is worth adjudicating is a judgement per release. The report shows the count per corpus
instead, so the residue's size is visible before anyone commits to reading it.

**A position whose line hash changed is dropped, not re-scored.** The one rule here that is not
bookkeeping: a corpus edit shifts offsets, and a stale verdict carried forward reads as a regression
that never happened.
"""

from audit import site
from audit.answers import card_of, locations, tier
from audit.lane3 import ledger
from audit.ruby import line_key

FINDINGS = ("stale",)


def counters():
    return {"residue": 0, "adjudicated": 0, "stale": 0, "decided": 0,
            "verdicts": {}, "residue-shapes": {}, "residue-signatures": {}}


# How many places an answer named, as four buckets. Coarse on purpose: a signature asks "has a kind
# of position appeared that nobody has looked at?", and a bucket per integer would report the name
# list growing by one as a new kind.
def places(count):
    return "0" if not count else "1" if count == 1 else "2-5" if count <= 5 else "6+"


def signature(shape, card, found):
    """What a person would see at a position, as one string: shape, tier, how many places.

    **A measurement, never a verdict.** Lane 3 cannot say whether a position is right; the ledger
    and a person do that. It can say which kinds of position the residue holds, and the baseline
    reports a **new** signature the way it reports a new finding. That names the shape of the gap
    and leaves the ruling to a person.

    It carries no corpus text (a shape name, a tier name, a bucket), so it is safe to commit.
    """
    return f"{shape}/{tier(card) if card else 'no-card'}/{places(len(found))}"


def run(corpus, drawn, answers, counts, findings, held):
    """Classify every drawn position. Returns `(counts, findings, residue)`.

    `residue` is `[(index, path, line, offset, shape, hash)]`: what `ledger pending` would ask a
    person about. Returned, not written, because this lane reports a size and the file records human
    work. Dumping the residue into the ledger would commit thousands of unadjudicated rows and call
    them a ledger.

    The draw index leads the tuple because nobody can adjudicate from a path and an offset:
    `adjudicate` needs the replies this run collected, and the index finds them.
    """
    cell = counters()
    # **One namespace, and it must be `audit.site`.** The Rails key draws its own rows, so its
    # findings index a different list than the sample's. As draw indices, every `rails-wrong` would
    # strike off whichever sampled position shared its number. A site cannot collide that way.
    decided = {where for _, where, _ in findings}
    for key in counts.get("keys") or {}:
        decided |= (counts["keys"][key].get("covered") or set())
    residue, source = [], {}

    def text_of(path):
        if path not in source:
            try:
                source[path] = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
            except OSError:
                source[path] = ""
        return source[path]

    for index, (_, shape, path, line, _, offset, _) in enumerate(drawn):
        if site(path, offset) in decided:
            cell["decided"] += 1
            continue
        text = text_of(path)
        if not text:
            continue
        found = line_key(text, offset)
        state, row = ledger.verdict(held, corpus, path, offset, found)
        if state == "live":
            cell["adjudicated"] += 1
            name = row.get("verdict") or "unfilled"
            cell["verdicts"][name] = cell["verdicts"].get(name, 0) + 1
            continue
        if state == "stale":
            cell["stale"] += 1
            findings.append(("stale", site(path, offset),
                             f"{shape} at {path}:{line + 1} -> the line changed under a "
                             f"`{row.get('verdict') or 'unfilled'}` verdict; re-adjudicate"))
            continue
        cell["residue"] += 1
        cell["residue-shapes"][shape] = cell["residue-shapes"].get(shape, 0) + 1
        name = signature(shape, card_of(answers.get((index, "textDocument/hover"))),
                         locations(answers.get((index, "textDocument/definition"))))
        cell["residue-signatures"][name] = cell["residue-signatures"].get(name, 0) + 1
        residue.append((index, path, line, offset, shape, found))
    counts.update(cell)
    return counts, findings, residue


def line(counts):
    filled = "  ".join(f"{name} {count}" for name, count in
                       sorted(counts["verdicts"].items(), key=lambda kv: -kv[1]))
    return (f"{counts['residue']} positions no rule decides, {counts['adjudicated']} adjudicated"
            + (f" ({filled})" if filled else "")
            + f", {counts['stale']} stale, of {counts['positions']}")


def summary(counts):
    return line(counts)


def under(counts):
    shapes = "  ".join(f"{k} {v}" for k, v in
                       sorted(counts["residue-shapes"].items(), key=lambda kv: -kv[1]))
    out = [f"residue   {shapes}"] if shapes else []
    # The six widest signatures. The tail is long and the terminal is not where anyone reads it;
    # `audit report` names the ones that moved, which is the useful half.
    top = sorted(counts["residue-signatures"].items(), key=lambda kv: -kv[1])[:6]
    if top:
        out.append("residue   " + "  ".join(f"{k} {v}" for k, v in top)
                   + f"  (+{len(counts['residue-signatures']) - len(top)} more signatures)")
    return out
