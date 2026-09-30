"""Check 7: the margin types a binding that the card at a call on it does not.

# What it counts

**The asymmetry:** the margin draws a type for a binding, and a card at a call on that same binding
has no type. Two rungs answering one question disagree.

**Not the invariant `hints.md` states.** *A name-matched type is never drawn in the margin* cannot
be seen on the wire:
- `hints.rs` drops guessed labels with one `retain` on `Tier`, so a guessed label never reaches the
  reply;
- a drawn label carries no tier marker, because every surviving hint is derived.
The unit test in `hints.rs` is where that invariant lives.

**Which guesses contradict the label is asked of `typeDefinition`**, since a card says only that it
guessed (decided 2026-09-29), not whether the receiver or the member was the guess. At the
receiver's own occurrence `typeDefinition` answers the class the receiver is, and refuses a guess:
- it answers: the receiver is typed and the member is not on it, a claim about the member;
- it answers nothing: the receiver has no type, or only one read off its name, which the label
  beside it contradicts. The rows this has raised were `cursor::receiver` missing a rung the margin
  has, not guesses in the margin: a block parameter the margin types from the block's signature
  while the card at a call on it guesses from the name.
- A receiver typed as a union answers nothing too (`Typed::one`), so a guessed card at a call on
  one is read as untyped: the one way this can call a consistent pair a finding.

# What is compared

The label sits at a **binding**: the `user` of `user = post.user`, the `post` of
`posts.each do |post|`. **Hover answers nothing at a binding**, at the write or anywhere else: a
local variable has no card, and `cursor::bindings_in` answers only the inlay hint's question.

So the comparison is the one `a_type_matched_on_a_name_alone_is_never_drawn_in_the_margin` makes: a
**call on the labelled binding**. `person.shout` cards as `Person#shout` with *Guessed from name
alone.*, and the margin beside `person` is empty. That card's tier, with `typeDefinition` at the
receiver, says how the receiver was typed, which is the label's question reached through
`cursor::receiver` instead of `bindings_in`.

Finding the calls needs no parser. `documentHighlight` at the binding lights every occurrence of the
variable (the scope walk), and a `.name` right after one of those spans is a call on it.

# Only the binding families

`hints.rs` draws three families: two are bindings (a local, a block parameter), and the third draws
a method's **return** after its `def`'s parameter list.

**The label's spelling is the only discriminator on the wire:** `": Class"` for a binding,
`" -> Class"` for a return. So `TYPED` reads the spelling as an include list, and `margin-returns`
and `margin-foreign` count what it turns away. A check reading a *position* reads an inference;
filter by what the wire says, not by what happens to be true of today's reply.

# What it can be wrong about

- **A variable assigned twice.** Which assignment speaks for a later use is `cursor.rs`' decision,
  so a guessed card at one use of a labelled binding is a contradiction **to be read**, not a defect
  to count blind. `_reassigned` stops at a second write.
- **A call on the line after its receiver** is not compared. That blind spot is kept instead of
  growing a parser.

# Cost

One request per sampled file, one highlight per **binding** label, and one hover and one
`typeDefinition` per call on one.
Small, because a hint is drawn only where the code does not already say the type.
- Filtering to the binding families keeps it small: a return label per `def` would add a highlight
  and a hover per method.
- A binding written twice is dropped before its cards are posted, so `_reassigned` saves requests.
"""

import re

from audit import site
from audit.answers import card_of, locations, spans, tier
from audit.client import open_document, uri
from audit.ruby import line_starts

FINDINGS = ("margin-guessed",)

# **Which hints this check reads, told apart by the only thing on the wire that can.** All three
# `hints.rs` families ship as `InlayHintKind::TYPE` with no `data` naming the family. So the
# **label's spelling** is the whole contract: `hints::label` writes `": Class"` for a binding, and
# the return family writes `" -> Class"`.
#
# A return label read as a binding points one character left of it, at the `)` of a `def`'s
# parameter list. The highlight there lands on a parameter, and every finding raised that way is
# wrong.
#
# **An include list, not an exclude list**, so a fourth family is skipped and counted, not swept in.
# `margin-foreign` makes that visible: a check with an invisible blind spot reads clean by asking
# nothing.
TYPED = ": "
RETURNED = " -> "

# `DocumentHighlightKind.Write`. See `_reassigned`.
WRITE = 3

# The identifier the label is drawn after, read back off the line for the finding's text.
BINDING = re.compile(r"([A-Za-z_][A-Za-z0-9_]*[?!]?)$")

# A call written straight after an occurrence of the binding, on the same line. Not a chain reader:
# `x\n  .foo` is a call on `x` this misses, but a regex crossing the line break would also read a
# comment ending in a full stop as a call. `shapes.find` follows the same rule.
CALLED = re.compile(r"\s*\.\s*([A-Za-z_][A-Za-z0-9_]*[?!]?)")


def counters():
    return {"margin-files": 0, "margin-labels": 0, "margin-returns": 0, "margin-foreign": 0,
            "margin-reassigned": 0, "margin-uses": 0, "margin-calls": 0,
            "margin-cards": 0, "margin-unread": 0, "margin-untyped": 0,
            "margin-named": 0, "margin-tiers": {}}


def ask(client, corpus, drawn, answers, opened):
    """The margin of every sampled file, then a card at every call on what it labelled.

    Three hops, and none is one request per drawn position: the unit is a *file*, then a *label*.
    That is why this is an `ask`, not a `POST`.
    """
    files = sorted({row[2] for row in drawn})
    posts = []
    for path in files:
        if path not in opened:
            open_document(client, corpus, path)
            opened.add(path)
        posts.append((("margin", path), "textDocument/inlayHint",
                      {"textDocument": {"uri": uri(corpus.dir / path)},
                       "range": _whole(corpus, path)}))
    hinted = _post(client, posts)

    lit = _post(client, [(("margin-lit", path, line, column), "textDocument/documentHighlight",
                          {"textDocument": {"uri": uri(corpus.dir / path)},
                           "position": {"line": line, "character": column}})
                         for path, line, column in _labels(hinted)])
    cards = []
    for path, line, column in _labels(hinted):
        answered = lit.get(("margin-lit", path, line, column))
        # Asked and read must agree on the population. Otherwise a label skipped here arrives in
        # `fold` with all its calls unanswered and lands in `margin-unread`.
        if _reassigned(answered):
            continue
        for at_line, at_column, (on_line, on_column) in _calls(_read(corpus, path),
                                                               spans(answered)):
            cards.append((("margin-card", path, at_line, at_column), "textDocument/hover",
                          {"textDocument": {"uri": uri(corpus.dir / path)},
                           "position": {"line": at_line, "character": at_column}}))
            cards.append((("margin-type", path, at_line, at_column),
                          "textDocument/typeDefinition",
                          {"textDocument": {"uri": uri(corpus.dir / path)},
                           "position": {"line": on_line, "character": on_column}}))
    return {**hinted, **lit, **_post(client, cards)}


def _post(client, posts, in_flight=32):
    out = {}
    for key, method, params in posts:
        client.post(key, method, params)
        for answered, result in client.drain(down_to=in_flight):
            out[answered] = result
    for answered, result in client.drain():
        out[answered] = result
    return out


def _read(corpus, path):
    try:
        return (corpus.dir / path).read_text(encoding="utf-8", errors="replace").split("\n")
    except OSError:
        return []


def _whole(corpus, path):
    """The whole file as a range. The range bounds the work, not the answer (`hints.md`), so this
    asks what an editor asks over a scroll of the file.
    """
    lines = _read(corpus, path)
    return {"start": {"line": 0, "character": 0},
            "end": {"line": max(0, len(lines) - 1), "character": len(lines[-1]) if lines else 0}}


def _reassigned(lit):
    """Whether the name this label was drawn on is written **more than once** in its scope.

    **Sound, not complete, because complete is not on the wire.** A label is drawn at one write, the
    highlight lights every occurrence of the *name*, and which write a read sees is a control-flow
    question:

        a, value = pair            # write 1
        find(value)                # read: sees write 1
        value = clean(value)       # write 2, in one `case` arm: the one the margin labels
        lookup(value)              # read in another arm: still sees write 1

    Both reads belong to write 1, so a card there saying "no type" contradicts nothing.
    - Textual order does not help: the last read comes *after* write 2 and still sees write 1,
      because a `case` arm is not a line number.
    - The server does not help: `definition` and `hover` answer nothing at a local.

    The one-write case is exact: every read in scope sees that write. So a second write is where the
    check stops asking. That narrows it by exactly the population it could never be right about, and
    the population is counted, for `margin-unread`'s reason.
    """
    rows = lit if isinstance(lit, list) else []
    return sum(1 for row in rows if isinstance(row, dict) and row.get("kind") == WRITE) > 1


def _labels(hinted):
    """Every **binding's** label, as the position of the last character of the name it follows.

    A hint sits one past the end of the binding's name, so the cursor *on* the name is one to the
    left. That holds only for a binding, which is why the family is filtered here: one left of a
    return label is punctuation. See `TYPED`.
    """
    return [at for at, label in _drawn(hinted) if label.startswith(TYPED)]


def _drawn(hinted):
    """Every hint in the replies, as `((path, line, column), label)`, family included."""
    out = []
    for key, reply in sorted(hinted.items()):
        if key[0] != "margin" or not isinstance(reply, list):
            continue
        for hint in reply:
            at = hint.get("position") or {}
            label = hint.get("label")
            if isinstance(label, list):
                # An editor renders the parts in order, so the contract is their concatenation.
                label = "".join(part.get("value", "") for part in label if isinstance(part, dict))
            out.append(((key[1], at.get("line", 0), max(0, at.get("character", 0) - 1)),
                        label if isinstance(label, str) else ""))
    return out


def _calls(lines, occurrences):
    """Every `.name` written straight after one of the binding's occurrences: the name's line and
    column, and where the occurrence (the call's receiver) starts.
    """
    out = []
    for span in occurrences:
        at = span.get("end") or {}
        line, column = at.get("line", 0), at.get("character", 0)
        if line >= len(lines):
            continue
        after = CALLED.match(lines[line][column:])
        if after:
            start = span.get("start") or {}
            out.append((line, column + after.start(1),
                        (start.get("line", line), start.get("character", column))))
    return out


def check(row, place, counts, findings):
    """Nothing per position: the unit here is a label, and `fold` is where they are counted."""


def fold(place, drawn, answers, counts, findings):
    hinted = {key: reply for key, reply in answers.items()
              if isinstance(key, tuple) and key and key[0] == "margin"}
    counts["margin-files"] += len({key[1] for key in hinted})
    # The families this check is not about, counted, not dropped:
    # - a return label is a claim about a method, not about the binding one character to its left;
    # - a spelling neither family writes is a fourth family nobody has taught this check to read.
    for _, label in _drawn(hinted):
        if label.startswith(TYPED):
            continue
        counts["margin-returns" if label.startswith(RETURNED) else "margin-foreign"] += 1
    for path, line, column in _labels(hinted):
        counts["margin-labels"] += 1
        lines = place.lines(path)
        answered = answers.get(("margin-lit", path, line, column))
        # A name written twice: the label is one of the writes, the reads are shared between them,
        # and no reply on the wire says which is which. See `_reassigned`.
        if _reassigned(answered):
            counts["margin-reassigned"] += 1
            continue
        occurrences = spans(answered)
        counts["margin-uses"] += len(occurrences)
        calls = _calls(lines, occurrences)
        counts["margin-calls"] += len(calls)
        read = False
        for at_line, at_column, _ in calls:
            card = card_of(answers.get(("margin-card", path, at_line, at_column)))
            if not card:
                continue
            read = True
            counts["margin-cards"] += 1
            said = tier(card)
            counts["margin-tiers"][said] = counts["margin-tiers"].get(said, 0) + 1
            if said != "guessed":
                continue
            if locations(answers.get(("margin-type", path, at_line, at_column))):
                # The receiver is typed and the member is not on it: a claim about the member, not
                # the margin. Counted so the denominator above stays readable beside the one below.
                counts["margin-named"] += 1
                continue
            counts["margin-untyped"] += 1
            at = site(path, _offset(place, path, at_line, at_column))
            findings.append(("margin-guessed", at,
                             f"label at {path}:{line + 1} over `{_named(lines, line, column)}`, "
                             f"and the card at the call on line {at_line + 1} guesses with no "
                             f"type for it"))
        if not read:
            # No call on the binding, or none that answered: the label stands and nothing in the
            # transcript speaks to it. Counted, because a check with an invisible blind spot reads
            # clean by asking nothing.
            counts["margin-unread"] += 1


def _offset(place, path, line, column):
    """`audit.site`'s own coordinate, for a position this check reached by line and column.

    Findings are keyed on `path:offset` everywhere in this package, because two cursors can share a
    line and lane 3 subtracts findings from the draw by that string.
    """
    starts = line_starts("\n".join(place.lines(path)))
    return (starts[line] if line < len(starts) else 0) + column


def _named(lines, line, column):
    """The name the label was drawn after, for a finding a person can go and look at."""
    if line >= len(lines):
        return "?"
    found = BINDING.search(lines[line][:column + 1])
    return found.group(1) if found else "?"


def line(counts):
    if counts["margin-files"] and not counts["margin-labels"]:
        return (f"BROKEN   {counts['margin-files']} files asked and not one label came back; "
                f"inlayHint is answering nothing")
    return (f"{counts['margin-untyped']} of {counts['margin-cards']} cards at a call on a "
            f"labelled binding have no type for it, of {counts['margin-labels']} labels in "
            f"{counts['margin-files']} files")


summary = line


def under(counts):
    said = []
    if counts["margin-unread"]:
        said.append(f"unread    {counts['margin-unread']} labels with no call on them to card")
    if counts["margin-named"]:
        said.append(f"named     {counts['margin-named']} cards typed the receiver and matched "
                    f"the member by name — counted, not reported")
    tiers = "  ".join(f"{name} {count}" for name, count in
                      sorted(counts["margin-tiers"].items(), key=lambda kv: -kv[1]))
    if tiers:
        said.append(f"tiers     {tiers} at {counts['margin-calls']} calls")
    return said
