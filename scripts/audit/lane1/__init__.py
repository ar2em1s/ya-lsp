"""Lane 1 — the keys that are **machine-decidable**: a right answer the corpus itself writes down.

Lane 2 compares the server against itself; lane 1 compares it against the source, and the
difference is that lane 1 can say *wrong* rather than only *inconsistent*.

Every filter in a key throws positions away rather than adjudicating them, and each one is there
because a run got it wrong — so the reasons travel with the code. **Over-blocking is safe here
and only here**: in a key it removes a question from the denominator, where in a filter on the
draw it removes the question from the sample. `routes.non_helpers` is the same idea on the other
side of that line and is deliberately weaker for it.

**One module per key.** A key is one of two kinds and says which with `ASKS`, because the two
run at different moments and the reason is not a preference:

    ASKS = False  `grade(corpus, drawn, answers)` — it reads replies lane 2 already collected,
                  so it needs no server and may run against a recorded transcript. `neutral`.
    ASKS = True   `ask(corpus, client, seed, opened, drawn, answers)` — it poses its own
                  questions, needs a live server **and must run before `ask_rebased`**, which
                  inserts a line into every sampled document and would move every one of its
                  cursors. `rails` and `completion`.

An asking key is handed the run's `drawn` and `answers` as well as the client, and the two keys
use that differently on purpose. `rails` ignores both and draws its own cursors, because the
macro positions it scores are a shape the sample does not target. `completion` takes both, because
it asks a *second question at the sample's own cursors* and the hover reply already collected for
each one is what lets it say which tier the server had claimed before it lost the member.

Both kinds also carry:

    NAME          the label the report prints the line under
    TOTAL         the counter that is this key's denominator; zero means print nothing
    line(counts) / summary(counts) / under(counts)
    FINDINGS      the finding kinds it raises

A finding is `(kind, site, detail)` in both lanes, and the `site` is `audit.site` rather than an
index into anything: a key that draws its own rows — `rails` does — numbers a list nobody else
holds, and lane 3 unions the two lanes' findings.

The two lanes divide by what a violation *means*, not by what it costs. A key that needed no
server would be nicer to have; `rails` cannot be one, because the positions it scores are a shape
the draw does not target and no amount of reading lane 2's replies will produce them.
"""

from audit.lane1 import calls, closures, completion, neutral, rails

KEYS = (neutral, rails, completion, calls, closures)


def asked(corpus, client, seed, opened=None, drawn=None, answers=None):
    """Every key that poses its own questions. Call before `ask_rebased` edits the buffers.

    `opened` is the caller's set of already-`didOpen`ed paths, shared so that a document the
    sample and a key both reach is opened once — see `client.ask_all`. `drawn` and `answers` are
    the run's own draw and the replies to it, for a key that asks a second question at those same
    cursors rather than at cursors of its own.
    """
    return _run([key for key in KEYS if getattr(key, "ASKS", False)],
                lambda key: key.ask(corpus, client, seed, opened, drawn, answers))


def graded(corpus, drawn, answers):
    """Every key that reads the draw's replies. Safe after the server has been stopped."""
    return _run([key for key in KEYS if not getattr(key, "ASKS", False)],
                lambda key: key.grade(corpus, drawn, answers))


def _run(keys, once):
    by_key, findings = {}, []
    for key in keys:
        counts, found = once(key)
        by_key[key.NAME] = counts
        findings += found
    return by_key, findings
