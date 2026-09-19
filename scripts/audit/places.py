"""Which places a reader is actually sent to, and whether the server can say what is there.

**A measurement, not a check.** `definition` answers with a list of places, and nothing in either
lane asks whether the server can describe the thing at one. It can fail to: a declaration's
definition list can hold a definition that `locator::locate` does not attribute back to it, and then
the jump lands on a real `def` the server has nothing to say about.

**Only a `def`, and only once per line.** Both restrictions are measured, not chosen.
- A schema column, a route or a macro is a *generated* declaration pointing at its real source, and
  hover says nothing there by design. Those are most of the silent places, and counting them would
  bury the rest.
- One hover per distinct line, not per offering: a name-matched list of forty offers the same forty
  lines at every cursor that asks.

**A counter, not a check**, for `audit.md`'s reason: a silent place is not automatically wrong, and
this file cannot say which ones are. It can say that the number moved.
"""

from audit.answers import locations, path_of


def _line_at(cache, path, number):
    if path not in cache:
        try:
            with open(path, encoding="utf-8", errors="replace") as handle:
                cache[path] = handle.read().split("\n")
        except OSError:
            cache[path] = []
    rows = cache[path]
    return rows[number] if 0 <= number < len(rows) else ""


def offered(answers, drawn, cap=0):
    """Every distinct `def` line the draw's `definition` answers offer, in the order offered.

    Returns `(places, skipped)`: the ones to ask about, and how many the cap left out. The order is
    the draw's, so one cap takes the same places on two runs of one commit, and the counter is
    comparable with itself.
    """
    cache, seen, places, skipped = {}, set(), [], 0
    for index in range(len(drawn)):
        for uri, span in locations(answers.get((index, "textDocument/definition"))):
            start = span["start"]
            key = (uri, start["line"], start["character"])
            if key in seen:
                continue
            seen.add(key)
            path = path_of(uri)
            if path is None or not _line_at(cache, path, start["line"]).strip().startswith("def "):
                continue
            if cap and len(places) >= cap:
                skipped += 1
                continue
            places.append(key)
    return places, skipped


def ask(client, places, in_flight=32):
    """Hover once at each place. Returns `{place: whether the server said anything}`.

    No `didOpen` first: a place is somewhere the reader has not been yet, and hover reads the file
    from disk when no buffer is held. Opening them would put documents the sample never drew into
    the server's map and change what later requests see.
    """
    described = {}
    for index, (uri, line, character) in enumerate(places):
        client.post((index, "place"), "textDocument/hover", {
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character},
        })
        for key, result in client.drain(down_to=in_flight):
            described[places[key[0]]] = _says_something(result)
    for key, result in client.drain():
        described[places[key[0]]] = _says_something(result)
    return described


def _says_something(reply):
    contents = (reply or {}).get("contents") or {}
    return bool(contents.get("value"))


def counters(described, skipped):
    return {"described": sum(1 for lit in described.values() if lit),
            "undescribed": sum(1 for lit in described.values() if not lit),
            "not-asked": skipped}


def line(counts):
    cell = counts["def-places"]
    asked = cell["described"] + cell["undescribed"]
    over = f", {cell['not-asked']} not asked" if cell["not-asked"] else ""
    return (f"{asked} distinct `def` places asked what is there; "
            f"{cell['undescribed']} answered nothing{over}")
