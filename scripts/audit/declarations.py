"""The five requests that only happen at a declaration, and what they answer there.

**A measurement, not a check**, for the reason `places` is not one: this file may not call what
these requests answer at a `def` right or wrong. It may say how often each answers at all, and that
the number moved.

`sample.positions` rightly excludes a cursor on a declaration's own name: there `definition` asks
where its own answer is, and the draw measures navigation. But five requests live at exactly those
cursors:

    prepareRename           a `class` name is renameable; a method's name is refused out loud
    prepareTypeHierarchy    a type's ancestors, which only a type has
    prepareCallHierarchy    the `def` a tree is rooted at
    implementation          every override below the receiver's own class
    declaration             the `.rbs` over the `def`, which is the one thing at this cursor
                            a reader standing on the definition cannot already see

`declaration` also answers at ordinary navigation cursors, but a `def`'s own name is where the
question is sharpest.

**Every counter here is prefixed `decl-`, and nothing else in the sweep writes one.** That is the
point of drawing it apart: every committed counter is a function of the draw, so adding questions to
the draw would move all of them. A separate stratum leaves the six shapes' counters unchanged.

**One invariant is stated here, not only counted** (`hierarchy.md`'s): `implementation` answers with
the definition itself, then the overrides. At a cursor on that very definition, the list must
**contain the cursor's own line**. That is the one finding this file raises.

**Containment, not first place.** A namespace like `module Admin` is written in dozens of files, all
definitions of the same name, and no ranking can put each of them first. A method has one
declaration, so "first" is still counted for the `method` half, and raises nothing.
"""

from audit import site
from audit.answers import locations, path_of

FINDINGS = ("declaration-absent",)

METHODS = ("textDocument/prepareRename", "textDocument/prepareTypeHierarchy",
           "textDocument/prepareCallHierarchy", "textDocument/implementation",
           "textDocument/declaration")

# What each reply is counted under: the method's last segment, minus the `prepare` that only says a
# hierarchy takes two requests to walk.
COUNTER = {"textDocument/prepareRename": "decl-rename",
           "textDocument/prepareTypeHierarchy": "decl-types",
           "textDocument/prepareCallHierarchy": "decl-calls",
           "textDocument/implementation": "decl-places",
           "textDocument/declaration": "decl-signature"}


def counters():
    return {"decl-cursors": 0, "decl-method": 0, "decl-type": 0, "decl-rename": 0,
            "decl-types": 0, "decl-calls": 0, "decl-places": 0, "decl-signature": 0,
            "decl-listed": 0, "decl-absent": 0, "decl-first": 0, "decl-not-first": 0}


def ask(client, corpus, rows, opened=None, in_flight=32):
    """The five methods at every declaration cursor. Returns `{(index, method): reply}`.

    `ask_all` could post these (they are five bare positions), but it strides and shapes against
    `lane2.METHODS`, whose shapes are the draw's six, not this stratum's two. Posting them here
    keeps the stratum's requests in its own file, the rule a check's `POST` follows.
    """
    from audit.client import open_document, uri

    answers = {}
    opened = set() if opened is None else opened
    for index, (_, _, path, line, column, _, _) in enumerate(rows):
        if path not in opened:
            open_document(client, corpus, path)
            opened.add(path)
        for method in METHODS:
            client.post((index, method), method,
                        {"textDocument": {"uri": uri(corpus.dir / path)},
                         "position": {"line": line, "character": column}})
        for key, result in client.drain(down_to=in_flight):
            answers[key] = result
    for key, result in client.drain():
        answers[key] = result
    return answers


def count(corpus, rows, answers, findings):
    """The stratum's counters, and the one finding it raises."""
    counts = counters()
    for index, (_, kind, path, line, _, offset, word) in enumerate(rows):
        counts["decl-cursors"] += 1
        counts[f"decl-{kind}"] += 1
        for method in METHODS:
            reply = answers.get((index, method))
            if reply:
                counts[COUNTER[method]] += 1
        places = locations(answers.get((index, "textDocument/implementation")))
        if not places:
            continue
        mine = [at for at, (target, span) in enumerate(places)
                if (path_of(target) or "").endswith(path) and span["start"]["line"] == line]
        if not mine:
            counts["decl-absent"] += 1
            findings.append(("declaration-absent", site(path, offset),
                             f"{kind} `{word}` at {path}:{line + 1} -> implementation names "
                             f"{len(places)} places and none of them is this one"))
            continue
        counts["decl-listed"] += 1
        if kind != "method":
            continue
        # Only a method: a namespace written in dozens of files has a definition in each, so no
        # ranking puts every one first. A `def` has one declaration, and there the order is a claim.
        if mine[0] == 0:
            counts["decl-first"] += 1
        else:
            counts["decl-not-first"] += 1
    return counts


def line(counts):
    return (f"{counts['decl-cursors']} declaration cursors ({counts['decl-method']} def, "
            f"{counts['decl-type']} class/module): rename {counts['decl-rename']}, "
            f"type hierarchy {counts['decl-types']}, call hierarchy {counts['decl-calls']}, "
            f"implementation {counts['decl-places']}, "
            f"declaration {counts['decl-signature']}; "
            f"{counts['decl-absent']} of {counts['decl-listed'] + counts['decl-absent']} "
            f"implementation lists leave the cursor's own line out, "
            f"{counts['decl-not-first']} of "
            f"{counts['decl-first'] + counts['decl-not-first']} `def` lists do not open on it")


summary = line
