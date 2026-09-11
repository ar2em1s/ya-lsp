"""Lane 1's fifth key — **a bare word inside a block written into a class body**.

`rails` draws its own cursors because the macro positions it scores are a shape the sample does
not target. This is the second such key and the reason is the same one measured: the stratified
draw holds 848 receiverless calls out of the hundreds of thousands the six corpora write, and the
shape below has **90** sites in them. A draw that size lands on none of them, which is why defect
26 was closed on a probe written for the occasion and the sweep for it moved 0 counters.

# The shape, and why it is worth a key of its own

`self` inside `rule(:colon) { … }` or `scope :recent, -> { … }` is the class object until whoever
takes the block re-binds it, and re-binding it is what every DSL that takes a block does. So a
bare name there may be on the class object and may equally be on an instance, and `locator`'s
closure rung answers the second — a card reading *Found on an instance of `X`*. Until 2026-09-14
`completion` at that same byte never left the class object, so the card named a member the list
beside it did not hold.

# The footnote is the filter, so the scan may be crude

The scan below is a floor and not a census: it reaches a macro call opening a block at two-space
indentation in a file whose first construct is a `class` or `module`, which is Rubocop's layout
and not Ruby's grammar. That is deliberate. **What decides whether a candidate counts is the
server's own sentence** — a card carrying the closure footnote is `locator` stating that the rung
fired, which by construction means the class object holds no such name. A candidate the scan
invents costs one pipelined hover and is dropped; a site it misses is not counted and the number
is honest about being a lower bound.

No mask is run over the file, for the same reason: a word inside a string or a comment is a
candidate the footnote throws away, and `ruby.masked` is a character walk over every byte of every
`.rb` file in six corpora, which is the one part of this that would cost real time.

# The prefix is the word

`lane1.calls` holds the argument: at an empty prefix a bare-word list is the 512-row ceiling on
every one of these, so a key posed there scores the cap. The cursor goes at the end of the word.
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
# Words that open a construct rather than call one. Over-blocking is safe in a key.
KEYWORDS = {"end", "if", "unless", "while", "until", "case", "when", "else", "elsif", "begin",
            "rescue", "ensure", "return", "yield", "def", "do", "then", "in", "and", "or",
            "not", "next", "break", "redo", "retry", "super", "self", "nil", "true", "false"}
# `hover.rs`' closure footnote, quoted rather than matched loosely for `answers.GUESSED`'s
# reason: a reworded sentence would turn this key off without ever failing it, and `under`
# below says so out loud when the scan finds candidates and the footnote finds none.
CLOSURE = "Found on an instance of `"
# The code fence a card opens with, as `Owner#member` or `Owner.member`.
NAMES = re.compile(r"^```ruby\n(?:private |protected )?.*?[#.]([A-Za-z0-9_?!]+)")
# How many rows a list may hold before an absence is the ceiling talking. `MAX_COMPLETION_ITEMS`.
CEILING = 512
# How deep below the opener to look for the first statement.
REACH = 12


def counters():
    return {"scanned": 0, "answered": 0, "present": 0, "absent": 0, "capped": 0}


def candidates(corpus):
    """[(path, line, column, word)] — every cursor the scan reaches, before the server sees one."""
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
        # Past the magic comments and the requires: the first construct the file opens.
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
    # Two pipelined passes and not one round trip each: `places` reads 31,924 hovers in 5.1 s
    # this way, and the whole point of a crude scan is that a wasted candidate is cheap.
    cards = {}
    for index, (path, line, column, _) in enumerate(rows):
        client.post((index, "closure-hover"), "textDocument/hover", {
            "textDocument": {"uri": uri(corpus.dir / path)},
            "position": {"line": line, "character": column}})
        for key, result in client.drain(down_to=IN_FLIGHT):
            cards[key] = result
    for key, result in client.drain():
        cards[key] = result

    # Only the cursors the server itself says the rung fired at. The card also has to *name this
    # word* — a hover that landed on something else is a candidate the scan got wrong, not a
    # server answering the wrong thing.
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
        # **A full list is the ceiling talking and not the server.** `completion.rs` caps at
        # `MAX_COMPLETION_ITEMS` after ranking, so a list at exactly that length with
        # `isIncomplete` has dropped rows and cannot be read as *the name is not a candidate*.
        # It is counted, because a name a developer cannot reach is still a name they cannot
        # reach, and it is not reported, because the fix for it is a ranking argument.
        if len(items) >= CEILING and isinstance(answer, dict) and answer.get("isIncomplete"):
            counts["capped"] += 1
            continue
        counts["absent"] += 1
        findings.append(("closure-absent", site_of(corpus, path, line, column),
                         f"`{word}` — the card finds it on an instance and completion at the "
                         f"same byte offers {len(items)} rows without it at {path}:{line + 1}"))
    return counts, findings


def site_of(corpus, path, line, column):
    """`audit.site` for a cursor this key drew itself, which is an offset the sample never made."""
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
