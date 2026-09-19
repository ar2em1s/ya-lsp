"""Sample the corpora, ask ya-lsp, score what can be scored, and diff against last time.

The subcommands:

    sample   the seeded, twice-stratified draw; prints it, writes nothing
    cost     per-position latency, and the sample size the budget buys
    latency  what one request makes a person wait, one in flight; not a throughput
    score    ask, then lane 2 (answers against each other) and lane 1 (against the source)
    report   a recorded run against the committed baseline: what moved, and what is new
    ledger / adjudicate   lane 3's residue, and the verdicts a person has already given

**`make audit` is `score --record`, then `report`.** They are two commands because `report` needs no
server and no corpus: a diff of two recordings can be re-read anywhere, long after the sweep.

**This package never sets up a corpus.**
- `scripts/corpora.toml` is the pin, and `scripts/corpora.py` makes a machine match it.
- This package imports that table, so each commit is written down once.
- It refuses to measure a corpus that has drifted from its pin. `make corpora-status` is the same
  check by hand.

**No corpus is named here.** The pin table's `role` says which corpora may be swept. `static-only`
is for one that may be counted but never asked.

**No corpus source text is ever written to disk** (`corpora.md`). Four of the six corpora are
copyleft, one has a proprietary subtree, and `audit/` is committed into an MIT repository.
- A sampled line travels as `sha256(line)`, never as itself.
- A position travels as `audit.site`: a path and an offset, never the identifier at it.
- Words from a corpus live in memory and reach the terminal, which is not committed.
- `audit/ledger.json` and `audit/baseline.json` hold integers, paths and hashes. Each is written
  from a named field list, so this is structural.

The modules, in pipeline order:

    config     the budget, the pin table, and what a measurable corpus is
    latency    the wait a request is, with the empty count a median has to be read against
    ruby       what part of a file is Ruby: comments, strings, heredocs, line offsets
    shapes     which *kind* of cursor a position is, and every candidate in one file
    places     where an answer sends a reader, and whether the server can describe it
    routes     the one shape whose candidates need a corpus-derived filter
    sample     the twice-stratified draw
    client     one server, one settle, replies matched by id
    answers    how to read a reply: the tier, the locations, the spans, where they landed
    lane2/     the checks that need no key; one module per check
    lane1/     the keys that are machine-decidable; one module per key
    lane3/     the residue a person rules on once, and the ledger those verdicts live in
    baseline   one run written down, and what may be compared with what
    report     one corpus' result, the totals over all of them, and this run against the last
    commands   what each subcommand does
"""


def site(path, offset):
    """How every part of this package names one position: the relative path and an offset.

    **One spelling, because three things match by string equality:** a lane-2 finding, a lane-1
    finding and a ledger row. That lets lane 3 subtract the first two from the draw and look up the
    third. It also lets the committed baseline name a finding without the word under the cursor.

    **The offset counts code points, not bytes** (`client.start` negotiates `utf-32`).

    **An offset, not a line.** Two shapes can be drawn on one line. A `path:line` identity would
    merge them, and lane 3 subtracting one would drop both.
    """
    return f"{path}:{offset}"
