"""Lane 3 — the adjudicated residue: a position no rule decides.

Lane 1 says *wrong* against the source; lane 2 says *inconsistent* against the server's other
answers. Everything they both stay silent about is this lane, and the only thing that can settle
it is a person. So the lane's whole job is to make that work **cost once**: a verdict already in
`audit/ledger.json` is reused, and only a new position asks anybody anything.

**There is no cap on how many positions this lane may hold.** A limit would be a number invented
here rather than measured, and how much residue is worth adjudicating in a given release is a
judgement about that release. What the report owes instead is the count, per corpus, so the size
of the residue is visible before anyone commits to reading it.

**A position whose line hash changed is dropped rather than re-scored.** That is the one rule in
the lane that is not bookkeeping: a corpus edit shifts offsets, and a stale verdict carried
forward reads as a regression that never happened.
"""

from audit import site
from audit.answers import card_of, locations, tier
from audit.lane3 import ledger
from audit.ruby import line_key

FINDINGS = ("stale",)


def counters():
    return {"residue": 0, "adjudicated": 0, "stale": 0, "decided": 0,
            "verdicts": {}, "residue-shapes": {}, "residue-signatures": {}}


# How many places an answer named, as four buckets. Coarse on purpose: the question a signature
# has to answer is "has a kind of position appeared that nobody has looked at", and a bucket per
# integer would report the name list growing by one as a new kind.
def places(count):
    return "0" if not count else "1" if count == 1 else "2-5" if count <= 5 else "6+"


def signature(shape, card, found):
    """What a person would see at a position, as one string: shape, tier, how many places.

    **A signature is a measurement and never a verdict.** Lane 3 cannot say whether a position is
    right — that is what the ledger and a person are for. What it can say is which kinds of
    position the residue holds, and the baseline then reports a **new** signature the same way it
    reports a new finding. That is the honest form of "has anything appeared nobody has ruled on":
    it names the shape of the gap and leaves the ruling where the ruling belongs.

    It carries no corpus text — a shape name, a tier name and a bucket — so it is safe to commit.
    """
    return f"{shape}/{tier(card) if card else 'no-card'}/{places(len(found))}"


def run(corpus, drawn, answers, counts, findings, held):
    """Classify every drawn position. Returns `(counts, findings, residue)`.

    `residue` is `[(index, path, line, offset, shape, hash)]` — what `ledger pending` would have
    to ask a person about — and it is returned rather than written, because this lane reports a
    size and the file records human work. A run that dumped its residue into the ledger would
    commit thousands of rows nobody adjudicated and call them a ledger.

    The draw index leads the tuple because a person cannot adjudicate a position from a path and
    an offset: `adjudicate` needs the replies this run already collected, and the index is how it
    finds them.
    """
    cell = counters()
    # **One namespace, and it has to be `audit.site`.** Both halves of this union used to be draw
    # indices — but the Rails key draws its own rows, so its findings numbered a different list,
    # and every `rails-wrong` struck the sampled position that happened to share its number off
    # the residue. Eleven of them on the last five-corpus sweep. A site cannot collide that way.
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
    # The six widest signatures, because the tail is long and the terminal is not where a reader
    # goes through it — `audit report` names the ones that moved, which is the useful half.
    top = sorted(counts["residue-signatures"].items(), key=lambda kv: -kv[1])[:6]
    if top:
        out.append("residue   " + "  ".join(f"{k} {v}" for k, v in top)
                   + f"  (+{len(counts['residue-signatures']) - len(top)} more signatures)")
    return out
