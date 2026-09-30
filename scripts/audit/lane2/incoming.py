"""Check 8: every call a call hierarchy shows is a reference the references list already had.

`callHierarchy/incomingCalls` and `textDocument/references` answer the same question about the same
name, printed two ways: one groups call sites by the `def` they are in, the other lists them flat.
Both are scoped to the user's own documents, neither is fenced by a test tree, and
`hierarchy::incoming` reads `references::calls_to`. So the *list of call sites* is one thing, and a
span in the tree that is missing from the list is the two disagreeing about the name they were asked
about.

**They start from opposite ends, which keeps this a check.**
- `references` starts at the cursor, locates a target and resolves it.
- `incomingCalls` starts at a `DeclarationId` that `prepareCallHierarchy` chose at the same cursor,
  and never reads the buffer again.
One is a cursor-to-declaration walk, the other a declaration-to-callers index read. This check says
they meet.

**Containment, one way only.** `references` is wider by construction (it includes the declaration
when asked, and lists every reference, not every *call*), so the check is `incoming ⊆ references`,
never the reverse.

**Four absences are legal, and are counted, not raised:**

  `redirect`  the item `prepareCallHierarchy` prepared is spelled differently from the cursor.
              `Foo.new` resolves to `Foo#initialize`, so the tree holds `initialize`'s callers
              and the list holds `new`'s references: different names by design.
              `references::find` leaves the declaration out of exactly this redirect, the same
              absence check 7 counts as `renamed`.
  `capped`    `references` stopped at `MAX_REFERENCES`. The list is then a prefix of the truth,
              ordered by file, so a caller after the cut is missing from the comparison, not
              from the server. The whole cursor is dropped.
  `multi`     `prepareCallHierarchy` answered with several items and only the first was
              followed, as an editor does. A second declaration's callers are not in this
              cursor's `references` answer.
  `silent`    the tree came back empty where `references` found places. Counted, because the
              empty tree is `incoming`'s own answer, not a missing one.

# Why it stays although it reads 0

**This zero is contingent; check 8's is structural.**
- Check 8's invariant is enforced by one `retain` after every family has had its say, so no
  repository can break it and no transcript can see it.
- This zero is a fact about today's server. Moving either route (`locate`, `resolve`,
  `references::find`, `calls_to`, or the reverse index under them) moves it.
- Nothing else compares those two routes: check 7 holds `references` against `highlight` and
  `definition`, and all three start at the cursor.
It has caught a real defect before: an `alias` line counted as an incoming call and left out of the
references list (`search-references.md`).

**Cost.** It rides check 7's subset, the only place it can be asked: it compares against
`references` at the same cursor, so its positions are exactly where check 7 asked
(`references.STRIDE`), narrowed to the two shapes a call can be written at. The subset keeps it
inside the rule that **a check past about a minute takes a fixed subset**.

**Memory was the real cost**, fixed in `_narrowed` by reducing replies as they arrive. A wider
stride would buy seconds this sweep does not need, at the price of the comparison it is kept for.
"""

from pathlib import Path

from audit.answers import path_of, point
from audit.client import uri
from audit.lane2.references import MAX_REFERENCES

FINDINGS = ("call-unreferenced", "callers-silent")

SHAPES = ("member", "call")


def counters():
    return {"calls-asked": 0, "calls-prepared": 0, "calls-multi": 0, "calls-capped": 0,
            "calls-redirected": 0, "calls-rows": 0, "calls-spans": 0, "calls-unreferenced": 0,
            "calls-silent": 0}


def ask(client, corpus, drawn, answers, opened):
    """`prepareCallHierarchy` where check 7 asked, then `incomingCalls` on what came back.

    **Two hops, so not a `POST`.** The second request carries the *item* the first answered with (a
    `DeclarationId` in its `data`, the only way to ask this), so it cannot be posted before the
    first reply is read. The positions come from `answers`: asking the transcript "where did check 7
    ask?" is one fact, and repeating `references.STRIDE`'s arithmetic here would be two.
    """
    posts = []
    for index, (_, shape, path, line, column, _, _) in enumerate(drawn):
        if shape not in SHAPES or (index, "textDocument/references") not in answers:
            continue
        posts.append(((index, "textDocument/prepareCallHierarchy"),
                      {"textDocument": {"uri": uri(corpus.dir / path)},
                       "position": {"line": line, "character": column}}))
    prepared = _post(client, "textDocument/prepareCallHierarchy", posts)
    followed = [((index, "callHierarchy/incomingCalls"), {"item": items[0]})
                for (index, _), items in prepared.items()
                if isinstance(items, list) and items]
    called = _post(client, "callHierarchy/incomingCalls", followed)
    return {**prepared, **{key: _narrowed(reply) for key, reply in called.items()}}


def _narrowed(reply):
    """One `incomingCalls` reply, reduced to the two things `check` reads.

    **Reduced where it arrives, because memory is the cost here, not seconds.** Every answer in this
    lane sits in one `answers` map until the corpus is done, and a large corpus' call trees are
    hundreds of MB of `dict`: each span three dicts, each caller an item whose name, kind, detail
    and extra ranges this check never reads. The same information as
    `(uri, (line, character, line, character))` tuples, without the item, is several times smaller.
    Nothing is lost: the list's *length* is the caller count, and the spans are what the comparison
    compares.

    A reply that is not a list (an error, or `null`) is passed on untouched, because `Row.callers`
    decides that it was not an answer.
    """
    if not isinstance(reply, list):
        return reply
    return [(((caller.get("from") or {}).get("uri"),
              tuple((span["start"]["line"], span["start"]["character"],
                     span["end"]["line"], span["end"]["character"])
                    for span in caller.get("fromRanges") or [])))
            for caller in reply]


def _post(client, method, posts, in_flight=32):
    out = {}
    for key, params in posts:
        client.post(key, method, params)
        for answered, result in client.drain(down_to=in_flight):
            out[answered] = result
    for answered, result in client.drain():
        out[answered] = result
    return out


def check(row, place, counts, findings):
    if not row.prepared:
        return
    counts["calls-asked"] += 1
    if not row.items:
        return
    counts["calls-prepared"] += 1
    if len(row.items) > 1:
        counts["calls-multi"] += 1
    if _bare(row.items[0].get("name")) != _bare(row.word):
        # The declaration is spelled differently from the cursor (`Foo.new` prepares
        # `Foo#initialize`), so the tree's callers and the list's references are for two names.
        counts["calls-redirected"] += 1
        return
    if len(row.references) >= MAX_REFERENCES:
        # A truncated `references` answer is a prefix of the truth ordered by file, so a caller in a
        # file after the cut would read as unreferenced. The whole cursor is dropped.
        counts["calls-capped"] += 1
        return
    if not row.callers:
        if row.references:
            counts["calls-silent"] += 1
            findings.append(("callers-silent", row.site,
                             f"{row.at} -> no callers, {len(row.references)} references"))
        return
    counts["calls-rows"] += len(row.callers)
    listed = {(target, *point(span["start"]), *point(span["end"]))
              for target, span in row.references}
    here, missing = 0, []
    for target, spans in row.callers:
        for span in spans:
            here += 1
            if (target, *span) not in listed:
                missing.append((target, span))
    counts["calls-spans"] += here
    if missing:
        counts["calls-unreferenced"] += len(missing)
        target, span = missing[0]
        where = Path(path_of(target) or target or "?").name
        findings.append(("call-unreferenced", row.site,
                         f"{row.at} -> {len(missing)} of {here} call sites not in the "
                         f"references list, first {where}:{span[0] + 1}"))


def _bare(name):
    """One name, without the punctuation and namespace two spellings of it differ by.

    A call hierarchy item is named `Rails::<Rails>#application`; a cursor is on `application`.
    """
    last = (name or "").rsplit("#", 1)[-1].rsplit("::", 1)[-1]
    return last.strip().strip(":\"'").rstrip("?!=")


def line(counts):
    if counts["calls-asked"] and not counts["calls-prepared"]:
        # The same guard as check 7, for the same reason: a request refused before it is answered (a
        # params shape the server cannot parse, an item it will not take) reads exactly like a
        # corpus where nothing has a caller.
        return (f"BROKEN   {counts['calls-asked']} cursors asked and no call hierarchy came "
                f"back; prepareCallHierarchy is answering nothing")
    return (f"{counts['calls-unreferenced']} of {counts['calls-spans']} incoming call sites "
            f"are not in the references list, from {counts['calls-rows']} callers at "
            f"{counts['calls-prepared']} of {counts['calls-asked']} cursors")


summary = line


def under(counts):
    said = []
    if counts["calls-silent"]:
        said.append(f"silent    {counts['calls-silent']} cursors with references and no caller")
    skipped = counts["calls-capped"] + counts["calls-multi"] + counts["calls-redirected"]
    if skipped:
        said.append(f"legal     {counts['calls-capped']} cursors dropped at the references cap, "
                    f"{counts['calls-redirected']} whose declaration is spelled differently, "
                    f"{counts['calls-multi']} that prepared more than one item and followed the "
                    f"first")
    return said
