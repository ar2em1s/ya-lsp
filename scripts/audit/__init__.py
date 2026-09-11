"""Sample the corpora, ask ya-lsp, score what can be scored, and diff against last time.

Four stages, and they are built in the order they *depend* on each other rather than the order
they run in. Stage 1 draws a sample sized to a wall-clock budget, and the size that budget buys
is not knowable until stage 2 has been measured — so the ask loop came first and `audit cost`
is what sized the draw. The number it produced is recorded in `.claude/rules/audit.md` beside the
run that produced it, because a budget derived once and then forgotten is a budget nobody can
check.

    sample   the seeded, twice-stratified draw; prints it, writes nothing
    cost     per-position latency, and the sample size the budget buys
    score    ask, then lane 2 (the answers against each other) and lane 1 (against the source)
    report   a recorded run against the committed baseline: what moved, and what is new
    ledger / adjudicate   lane 3's residue, and the verdicts a person has already given

`make audit` is `score --record` and then `report`, and they are **two commands rather than one**
because the second needs no server and no corpus. A diff of two recordings can be re-read, re-cut
and re-run in CI from an artifact long after the machine that swept is gone; a report that had to
re-sweep to say what moved could only ever run where the corpora are.

**The corpora are pinned elsewhere and this package never sets one up.** `scripts/corpora.toml`
is the pin and `scripts/corpora.py` is what makes a machine match it; this package imports the
table so that one copy of a commit exists rather than two, and refuses to measure a corpus whose
working tree has drifted from it. `make corpora-status` is the same check by hand.

**Six corpora, and the count is not written down here.** The pin table's `role` says which may
be swept; `static-only` is the role for one that may be counted and never asked, and nothing
currently holds it. Nothing here reads a corpus name to decide that — a list of names here would
be a second copy of a decision that already has a home. discourse was that exception until
2026-09-12: it was excluded on the cost of an *exhaustive* sweep, which is not what this package
does, and at a stratified 896 positions it costs 77s and is the only corpus check 5 has ever
fired on.

**No corpus source text is ever written to disk by this package.** Four of the six are copyleft
and one has a proprietary subtree; `audit/` is committed into an MIT repository, so a sampled line
travels as `sha256(line)` and never as itself, and a position travels as `audit.site` — a path and
a byte offset, never the identifier standing at it. The rule is `corpora.md`'s and it is blanket.
Words sampled from a corpus live in memory for the length of a run and reach the terminal, which is
not committed; the two files that are — `audit/ledger.json` and `audit/baseline.json` — hold
integers, paths and hashes, and each is written from a named field list so that is structural.

The modules, in the order the pipeline uses them:

    config     the budget, the pin table, and what a measurable corpus is
    ruby       what part of a file is Ruby: comments, strings, heredocs, line offsets
    shapes     which *kind* of cursor a position is, and every candidate in one file
    places     where an answer sends a reader, and whether the server can describe it
    routes     the one shape whose candidates need a corpus-derived filter
    sample     the twice-stratified draw
    client     one server, one settle, replies matched by id
    answers    how to read a reply: the tier, the locations, the spans, where they landed
    lane2/     the checks that need no key — one module per check
    lane1/     the keys that are machine-decidable — one module per key
    lane3/     the residue a person rules on once, and the ledger those verdicts live in
    baseline   one run written down, and what may be compared with what
    report     one corpus' result, the totals over all of them, and this run against the last
    commands   what each subcommand does
"""


def site(path, offset):
    """How every part of this package names one position: the relative path, and a byte offset.

    One spelling, because three things agree by *being* the same string. A lane-2 finding, a
    lane-1 finding and a ledger row all name a position this way, which is what lets lane 3
    subtract the first two from the draw and look the third up — and what lets a committed
    baseline carry a finding's identity without carrying the word under the cursor.

    **A byte offset and not a line.** Two of the six shapes can be drawn twice on one line, so a
    `path:line` identity would silently merge them, and lane 3 subtracting one would drop both.
    """
    return f"{path}:{offset}"
