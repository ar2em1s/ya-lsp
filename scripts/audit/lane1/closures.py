"""Lane 1's closures key: **a bare word inside a block written into a class body**.

This key draws its own cursors, like `rails`, for the same reason: the shape is too rare for the
stratified draw to land on. A draw of the sample's size reaches none of its sites.

# The shape, and why it gets its own key

`self` inside `rule(:colon) { … }` or `scope :recent, -> { … }` is the class object until whoever
takes the block re-binds it, and every DSL that takes a block does. So a bare name there may be on
the class object or on an instance. `locator`'s closure rung answers the second, with a card reading
*Found on an instance of `X`*. `completion` at the same cursor must then offer that instance's
members too; otherwise the card names a member the list beside it lacks.

# The footnote is the filter, so the scan may be crude

The scan is a floor, not a census. It reaches a macro call opening a block at two-space indentation,
in a file whose first construct is a `class` or `module`: RuboCop's layout, not Ruby's grammar. That
is deliberate.
- **The server's own sentence decides whether a candidate counts.** A card carrying the closure
  footnote is `locator` saying the rung fired, which means the class object holds no such name.
- A candidate the scan invents costs one pipelined hover and is dropped.
- A site it misses is not counted, and the number is honest about being a lower bound.

No mask is run, for the same reason: a word in a string or a comment is a candidate the footnote
throws away, and `ruby.masked` over every `.rb` file in six corpora is the one step here that would
cost real time.

# The prefix is the word

As `lane1.calls` argues: at an empty prefix, a bare-word list is the 512-row ceiling at every one of
these cursors, so a key posed there scores the cap. The cursor goes at the end of the word.
"""

import re

from audit import site
from audit.answers import card_of
from audit.client import uri
from audit.lane1.completion import CONTEXT, IN_FLIGHT, METHOD, rank_of
from audit.sample import SKIP

NAME = "closures"
FINDINGS = ("closure-absent",)
TOTAL = "scanned"
ASKS = True

# A macro call at two-space indentation that opens a block: `scope :recent, -> { … }`,
# `validate :x, if: -> { … }`, `has_many :xs do … end`, `with_options … do |o| … end`.
OPENS = re.compile(r"^  ([a-z_][A-Za-z0-9_]*[!?]?)[ (].*?"
                   r"(?:\bdo\b(?: *\|[^|]*\|)? *$|-> *\{|\blambda *\{|\{ *(?:\|[^|]*\| *)?)")
# The first bare word inside it, deeper than the opener.
WORD = re.compile(r"^(\s{4,})([a-z_][A-Za-z0-9_]*)")
# Words that open a construct instead of calling one. Over-blocking is safe in a key.
KEYWORDS = {"end", "if", "unless", "while", "until", "case", "when", "else", "elsif", "begin",
            "rescue", "ensure", "return", "yield", "def", "do", "then", "in", "and", "or",
            "not", "next", "break", "redo", "retry", "super", "self", "nil", "true", "false"}
# `hover.rs`' closure footnote, quoted exactly, for `answers.GUESSED`'s reason: a reworded sentence
# would turn this key off without failing it. `under` says so when the scan finds candidates and the
# footnote finds none.
CLOSURE = "Found on an instance of `"
# The code fence a card opens with, as `Owner#member` or `Owner.member`.
NAMES = re.compile(r"^```ruby\n(?:private |protected )?.*?[#.]([A-Za-z0-9_?!]+)")
# How many rows a list may hold before an absence is the ceiling talking: `MAX_COMPLETION_ITEMS`.
CEILING = 512
# How deep below the opener to look for the first statement.
REACH = 12


def counters():
    return {"scanned": 0, "answered": 0, "present": 0, "absent": 0, "capped": 0}


def candidates(corpus):
    """[(path, line, column, word)]: every cursor the scan reaches, before the server sees one."""
    found = []
    for path in sorted(corpus.dir.rglob("*.rb")):
        marked = "/" + str(path.parent.relative_to(corpus.dir)).strip(".") + "/"
        if any(skip in marked for skip in SKIP):
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        lines = text.split("\n")
        # Past the magic comments and requires: the first construct the file opens.
        head = next((row for row in lines if row.strip() and not row.lstrip().startswith("#")), "")
        if not (head.startswith("class ") or head.startswith("module ")):
            continue
        relative = str(path.relative_to(corpus.dir))
        for row, line in enumerate(lines):
            if not OPENS.match(line):
                continue
            for below in range(row + 1, min(row + REACH, len(lines))):
                if lines[below].strip() == "end":
                    break
                seen = WORD.match(lines[below])
                if not seen or seen.group(2) in KEYWORDS:
                    continue
                after = lines[below][seen.end():seen.end() + 2]
                # A member cursor, or an assignment: neither is a bare call.
                if after.startswith(".") or (after.startswith("=") and not after.startswith("==")):
                    continue
                found.append((relative, below, len(seen.group(1)), seen.group(2)))
                break
    return found


def ask(corpus, client, seed, opened=None, drawn=None, answers=None):
    counts, findings = counters(), []
    rows = candidates(corpus)
    counts["scanned"] = len(rows)
    if not rows:
        return counts, findings
    # Two pipelined passes, not one round trip per cursor, as `places` does it. A crude scan only
    # works if a wasted candidate is cheap.
    cards = {}
    for index, (path, line, column, _) in enumerate(rows):
        client.post((index, "closure-hover"), "textDocument/hover", {
            "textDocument": {"uri": uri(corpus.dir / path)},
            "position": {"line": line, "character": column}})
        for key, result in client.drain(down_to=IN_FLIGHT):
            cards[key] = result
    for key, result in client.drain():
        cards[key] = result

    # Only cursors where the server says the rung fired. The card must also *name this word*: a
    # hover that landed on something else is the scan's mistake, not the server's.
    fired = []
    for index, (path, line, column, word) in enumerate(rows):
        card = card_of(cards.get((index, "closure-hover")))
        if not card or CLOSURE not in card:
            continue
        named = NAMES.match(card)
        if not named or named.group(1) != word:
            continue
        fired.append((index, path, line, column, word))

    lists = {}
    for index, path, line, column, word in fired:
        client.post((index, "closure-list"), METHOD, {
            "textDocument": {"uri": uri(corpus.dir / path)},
            "position": {"line": line, "character": column + len(word)},
            "context": CONTEXT})
        for key, result in client.drain(down_to=IN_FLIGHT):
            lists[key] = result
    for key, result in client.drain():
        lists[key] = result

    for index, path, line, column, word in fired:
        counts["answered"] += 1
        answer = lists.get((index, "closure-list"))
        items = answer.get("items") if isinstance(answer, dict) else answer
        items = items or []
        if rank_of(items, word) is not None:
            counts["present"] += 1
            continue
        # **A full list is the ceiling talking, not the server.** `completion.rs` caps at
        # `MAX_COMPLETION_ITEMS` after ranking, so a list at exactly that length with `isIncomplete`
        # has dropped rows and cannot mean *the name is not a candidate*.
        # - Counted: a name a developer cannot reach is still unreachable.
        # - Not reported: its fix is a ranking argument.
        if len(items) >= CEILING and isinstance(answer, dict) and answer.get("isIncomplete"):
            counts["capped"] += 1
            continue
        counts["absent"] += 1
        findings.append(("closure-absent", site_of(corpus, path, line, column),
                         f"`{word}` — the card finds it on an instance and completion at the "
                         f"same byte offers {len(items)} rows without it at {path}:{line + 1}"))
    return counts, findings


def site_of(corpus, path, line, column):
    """`audit.site` for a cursor this key drew itself: an offset the sample never made."""
    text = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
    rows = text.split("\n")
    return site(path, sum(len(row) + 1 for row in rows[:line]) + column)


def line(counts):
    if not counts.get("scanned"):
        return "nothing scanned"
    return (f"{counts['present']} of {counts['answered']} closure cards have the member on the "
            f"list beside them, of {counts['scanned']} candidates scanned")


summary = line


def under(counts):
    out = []
    if counts.get("scanned") and not counts.get("answered"):
        out.append("no candidate carried the closure footnote — if the scan found any, "
                   "`hover.rs`'s sentence has been reworded and this key is reading nothing")
    if counts.get("capped"):
        out.append(f"{counts['capped']} not read — the list came back at the {CEILING}-row "
                   f"ceiling, so an absence there is the cap and not the candidate set")
    return out
