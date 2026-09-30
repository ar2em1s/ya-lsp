"""Lane 2: the checks that need **no key at all**.

They compare ya-lsp's answers with each other, so a violation is a self-contradiction, not a
disagreement with somebody's opinion. That is why the lane exists: nothing in it is a judgement
anyone makes twice, so it could one day run unattended.

**One module per check; the registry below numbers them.** A check's own file holds everything it
owns: its counters, the test, its report line and its finding kinds, plus `POST`, `ask` or `fold`
where it needs them. Adding a check is one file and one name in `CHECKS`; nothing in `report` or
`commands` lists checks by hand.

    counters()                 -> a fresh dict of the counter keys this check owns
    check(row, place, counts, findings)
                               -> run it over one position; append `(kind, site, detail)`
    line(counts)               -> the `check N` line in one corpus' report
    summary(counts)            -> the same number over every corpus
    under(counts)   optional   -> extra lines indented directly under this check's line
    breakdown(counts) optional -> extra lines in the trailing block, after both lanes
    fold(place, drawn, answers, counts, findings)
                    optional   -> counted once per corpus rather than once per position, for a
                                  check whose unit is a file or a label rather than a cursor
    FINDINGS                   -> the finding kinds it raises, in the order to print them
    POST            optional   -> a `client.Post` named in `METHODS`: the request this check
                                  sends, the params it needs, the shapes it may be concluded
                                  from and how much of the draw it takes
    ask(client, corpus, drawn, answers, opened)
                    optional   -> the requests a `Post` cannot express, returned as
                                  `{key: reply}` and merged into `answers`

**`POST` is one request per eligible position; `ask` is everything else.**
- A `Post` is declarative (a method, its params, its legal shapes, its stride), and `ask_all` sends
  it beside the others in one pass.
- `ask` covers conversations: a request whose unit is not a drawn position (check 7 asks per file),
  or whose params are a previous reply (check 8's `incomingCalls` carries the item
  `prepareCallHierarchy` returned).
A check that needs neither has neither.
"""

from audit.answers import first_place, names_an_id
from audit.lane2 import (footnotes, highlight, incoming, margins, rebase, references, renaming,
                         resolved, spans)
from audit.lane2.context import Corpus, Row

# The order is the numbering: `check 1` is the first entry. Findings print in this order too, so a
# reader meets `check 2`'s findings right after its line.
#
# Append; never insert. Readers remember last week's numbering, and an insertion renumbers every
# check after it. (Check 6, `receiver`, was removed on 2026-09-29 with the card sentences it read:
# the checks after it moved up one.)
CHECKS = (highlight, resolved, spans, footnotes, rebase, references, margins, incoming, renaming)

# What lane 2 needs asked. `hover` and `definition` are the pair every lane reads;
# `documentHighlight` is check 1's alone; check 5 asks the first two a second time.
#
# **Three are asked at every position and the fourth is not**, so this is strings plus one
# [`client.Post`]. `references` is legal at only three of the six shapes (`highlight.rs` answers a
# local from a scope walk `references` lacks), needs a `context` the others do not, and is the
# widest answer the server gives, so its share of the draw is a measurement. All of that lives in
# `lane2/references.py`, beside the check that reads the reply: a request posted here with its
# meaning written there would drift.

METHODS = ("textDocument/hover", "textDocument/definition", "textDocument/documentHighlight",
           references.POST, renaming.POST)


def asked(client, corpus, drawn, answers, opened):
    """Every check's `ask` hook, merged into `answers`. Returns what was added.

    Runs after `ask_all` (a hook that reads the transcript needs one) and **before** `ask_rebased`,
    which inserts a line at the top of every sampled document and would move every cursor these
    hooks pose. Lane 1's asking keys are under the same constraint. That is why `measure()` calls
    this, not `run`: by the time the checks run, the server is stopped.
    """
    added = {}
    for check in CHECKS:
        hook = getattr(check, "ask", None)
        if hook:
            added.update(hook(client, corpus, drawn, answers, opened) or {})
    answers.update(added)
    return added


def run(corpus, drawn, answers, rebased=None, shifted=None, places=None):
    """Every check over one corpus' answers. Returns `(counts, findings)`.

    `findings` is `[(kind, site, detail)]`; the caller decides how many to print.
    - `site` is `audit.site` (`path:offset`, no word), a finding's identity: lane 3 subtracts it
      from the draw, and the committed baseline records it so a later run can say which findings are
      new.
    - `detail` is for a person and does carry the word.

    `score` writes nothing to disk. The identifier reaches the terminal, because a finding nobody
    can go and look at is not a finding. The licence rule governs what is **committed**: the ledger
    and the baseline, which hold no word.
    """
    # **Two measurements beside `tiers`; neither is a check.** Counted per position like the tier,
    # they raise nothing and judge nothing (`lane3.signature`'s standard). Each catches a change
    # otherwise invisible:
    # - `shape/tier/places` counts places but never which came first, so a re-ordered list moves no
    #   counter;
    # - a card printing an internal id instead of a name leaves the tier and the count unchanged.
    counts = {"positions": len(drawn), "hover": 0, "definition": 0, "highlight": 0,
              "tiers": {"resolved": 0, "guessed": 0},
              "first-place": {"one-library": 0, "majority": 0, "minority": 0},
              "cards-anonymous": 0,
              # Counted per distinct place, not per position, so it arrives already summed instead
              # of accumulated in the loop below.
              "def-places": places or {"described": 0, "undescribed": 0, "not-asked": 0}}
    for check in CHECKS:
        counts.update(check.counters())
    place = Corpus(corpus, shifted)
    findings = []
    for index, drawn_row in enumerate(drawn):
        row = Row(index, drawn_row, answers, rebased, place)
        if row.card:
            counts["hover"] += 1
            counts["tiers"][row.tier] += 1
            if names_an_id(row.card):
                counts["cards-anonymous"] += 1
        if row.found:
            counts["definition"] += 1
        where_first = first_place(place.corpus, row.found, place.library)
        if where_first:
            counts["first-place"][where_first] += 1
        if row.lit:
            counts["highlight"] += 1
        for check in CHECKS:
            check.check(row, place, counts, findings)
    for check in CHECKS:
        # After every row, because a check that counts files or labels still reports under its own
        # number in the same line. Only the unit differs, not where it prints.
        fold = getattr(check, "fold", None)
        if fold:
            fold(place, drawn, answers, counts, findings)
    return counts, findings
