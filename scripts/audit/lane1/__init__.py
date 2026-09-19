"""Lane 1: the keys that are **machine-decidable**, where the corpus itself writes the right answer
down.

Lane 2 compares the server with itself; lane 1 compares it with the source, so lane 1 can say
*wrong*, not only *inconsistent*.

Every filter in a key throws positions away instead of adjudicating them, and each exists because a
run got it wrong, so the reasons sit with the code. **Over-blocking is safe here and only here:** in
a key it removes a question from the denominator; in a filter on the draw it removes the question
from the sample. `routes.non_helpers` is the same idea on the other side of that line, and
deliberately weaker for it.

**One module per key.** `ASKS` says which of two kinds a key is, because the two run at different
moments:

    ASKS = False  `grade(corpus, drawn, answers)`: reads replies lane 2 already collected, so
                  it needs no server and may run against a recorded transcript. `neutral`.
    ASKS = True   `ask(corpus, client, seed, opened, drawn, answers)`: poses its own questions,
                  so it needs a live server **and must run before `ask_rebased`**, which
                  inserts a line into every sampled document and would move its cursors.
                  Every other key.

An asking key gets the run's `drawn` and `answers` as well as the client, and keys use them
differently on purpose:
- `rails` ignores both and draws its own cursors: the macro positions it scores are a shape the
  sample does not target.
- `completion` takes both: it asks a *second question at the sample's own cursors*, and the hover
  reply already collected there says which tier the server claimed before it lost the member.

Both kinds also carry:

    NAME          the label the report prints the line under
    TOTAL         the counter that is this key's denominator; zero means print nothing
    line(counts) / summary(counts) / under(counts)
    FINDINGS      the finding kinds it raises

A finding is `(kind, site, detail)` in both lanes, and `site` is `audit.site`, not an index: a key
that draws its own rows numbers a list nobody else holds, and lane 3 unions both lanes' findings.

The lanes divide by what a violation *means*, not by cost. `rails` cannot be a server-free key: the
positions it scores are a shape the draw does not target, and no reading of lane 2's replies
produces them.
"""

from audit.lane1 import calls, closures, completion, neutral, outline, rails

KEYS = (neutral, rails, completion, calls, closures, outline)


def asked(corpus, client, seed, opened=None, drawn=None, answers=None):
    """Every key that poses its own questions. Call before `ask_rebased` edits the buffers.

    - `opened` is the caller's set of already-`didOpen`ed paths, shared so a document the sample and
      a key both reach is opened once (see `client.ask_all`).
    - `drawn` and `answers` are the run's draw and its replies, for a key that asks a second
      question at those same cursors.
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
