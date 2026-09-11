"""One corpus' result, the same numbers over all of them, and this run against the last one.

Nothing here knows how many checks there are or what any of them measures. Both functions walk
`lane1.KEYS` and `lane2.CHECKS` and ask each one for its own line, which is what makes adding a
check a one-file change — the alternative is a format string here that drifts from the counter
it prints.
"""

from audit import baseline, lane1, lane2, lane3, places


def _lines(module, hook, counts):
    got = getattr(module, hook, None)
    return got(counts) if got else []


def report(corpus, counts, findings, elapsed, show):
    """One corpus: the header, every check, every key, the breakdown, then the findings."""
    tiers = counts["tiers"]
    print(f"{corpus.name:10} {counts['positions']:5} positions  {elapsed:5.1f}s   "
          f"hover {counts['hover']}  definition {counts['definition']}  "
          f"highlight {counts['highlight']}")
    if counts["hover"] and not (tiers["derived"] or tiers["guessed"]):
        # The one way this lane can be wrong without failing: `hover.rs` rewords a footnote,
        # nothing matches, every card reads as Resolved, and check 2 reports a flood or a zero
        # that means nothing either way. Refuse to be believed rather than report it.
        print(f"{'':10} BROKEN   every card read as Resolved — the sentences in GUESSED and "
              f"DERIVED no longer match hover.rs")
        return
    print(f"{'':10} tiers    resolved {tiers['resolved']}  derived {tiers['derived']}  "
          f"guessed {tiers['guessed']}")
    print(f"{'':10} places   {_places(counts)}")
    print(f"{'':10} described {places.line(counts)}")
    for number, check in enumerate(lane2.CHECKS, 1):
        print(f"{'':10} {'check ' + str(number):8} {check.line(counts)}")
        for extra in _lines(check, "under", counts):
            print(f"{'':10}   {extra}")
    for key in lane1.KEYS:
        cell = (counts.get("keys") or {}).get(key.NAME)
        # `TOTAL` rather than a fixed name: the neutral key's denominator is `knowable` and the
        # Rails key's is `asked`, and a key with none of its own rows prints nothing at all
        # rather than a row of zeroes nobody can read a rate off.
        if not cell or not cell.get(key.TOTAL):
            continue
        print(f"{'':10} {key.NAME:8} {key.line(cell)}")
        for extra in _lines(key, "under", cell):
            print(f"{'':10}   {extra}")
    if "residue" in counts:
        print(f"{'':10} {'lane 3':8} {lane3.line(counts)}")
        for extra in _lines(lane3, "under", counts):
            print(f"{'':10}   {extra}")
    for check in lane2.CHECKS:
        for extra in _lines(check, "breakdown", counts):
            print(f"{'':10}   {extra}")
    for kind in kinds():
        rows = [detail for found, _, detail in findings if found == kind]
        for detail in rows[:show]:
            print(f"{'':10}   {kind:16} {detail}")
        if len(rows) > show:
            print(f"{'':10}   {kind:16} ... and {len(rows) - show} more")


def _places(counts):
    """The two measurements that are not checks, on one line.

    Printed with no verdict attached, which is the whole reason they are here rather than in a
    check: `minority` is not a defect — a name spread over forty gems has no majority to be in —
    and a card naming an id is not one this line is entitled to call. What they are for is
    **movement**: re-ordering a place list or renaming an anonymous class moves nothing in
    `shape/tier/places`, so without these two a fix to either is invisible to every lane.
    """
    first = counts["first-place"]
    lists = sum(first.values())
    return (f"{lists} lists of 2+ places: {first['one-library']} in one library, "
            f"{first['majority']} opening on the library most of them are in, "
            f"{first['minority']} not; {counts['cards-anonymous']} cards name an internal id")


def kinds():
    """Every finding kind, in the order to print them: the keys first, then check by check."""
    out = []
    for module in tuple(lane1.KEYS) + tuple(lane2.CHECKS) + (lane3,):
        out.extend(module.FINDINGS)
    return out


def totals(totals_, keyed, positions, spent, budget):
    """The all-five line, and one line per check and per key under it."""
    print()
    print(f"{'all':10} {positions:5} positions  {spent:5.1f}s of the {budget}s budget   "
          f"hover {totals_['hover']}   definition {totals_['definition']}   "
          f"highlight {totals_['highlight']}")
    print(f"{'':10} places   {_places(totals_)}")
    print(f"{'':10} described {places.line(totals_)}")
    for number, check in enumerate(lane2.CHECKS, 1):
        print(f"{'':10} {'check ' + str(number):8} {check.summary(totals_)}")
    for key in lane1.KEYS:
        cell = keyed.get(key.NAME)
        if cell and cell.get(key.TOTAL):
            print(f"{'':10} {key.NAME:8} {key.summary(cell)}")
    if "residue" in totals_:
        print(f"{'':10} {'lane 3':8} {lane3.summary(totals_)}")


def _delta(was, now):
    """One counter's movement. An absent side is named as absent and never printed as a zero."""
    if was is None:
        return f"{'':>7} -> {now:>7}   new counter"
    if now is None:
        return f"{was:>7} -> {'':>7}   counter dropped"
    return f"{was:>7} -> {now:>7}   {now - was:+d}"


def moved(name, before, now, show):
    """One corpus, this run against the baseline.

    Returns `(new, gone, counters)`, or **`None` when no comparison happened** — a corpus with no
    baseline row and one whose pin or draw moved are both "not compared", and folding either into
    a row of zeroes would report them in the total as that many corpora that agreed.

    **Findings first, counters second, and that ordering is the one judgement this function
    makes.** A finding is a defect a check has already named, so a new one is a regression and a
    gone one is a fix — no opinion needed here. Whether a counter going up is good or bad depends
    on the counter, the answer lives in the check that owns it, and a table of directions here
    would be that answer copied to a second place. So a counter is printed with its sign and
    nothing else.
    """
    if not before:
        rows = sum(len(v) for v in (now.get("findings") or {}).values())
        print(f"{name:10} NEW        nothing to compare against; recorded "
              f"{len(now.get('counts') or {})} counters and {rows} findings")
        return None
    why = baseline.why_not(before, now)
    if why:
        print(f"{name:10} NOT COMPARED  {why}")
        return None
    new, gone = baseline.found(before, now)
    counters = baseline.moved(before, now)
    if not (new or gone or counters):
        print(f"{name:10} unchanged  {len(now.get('counts') or {})} counters, "
              f"{sum(len(v) for v in (now.get('findings') or {}).values())} findings")
        return 0, 0, 0
    print(f"{name:10} {len(new)} findings new, {len(gone)} gone, "
          f"{len(counters)} counters moved")
    for label, rows in (("new", new), ("gone", gone)):
        for kind, where in rows[:show]:
            print(f"{'':10}   {label:6} {kind:16} {where}")
        if len(rows) > show:
            print(f"{'':10}   {label:6} {'':16} ... and {len(rows) - show} more")
    for counter, was, has in counters[:show]:
        print(f"{'':10}   {'moved':6} {counter:16} {_delta(was, has)}")
    if len(counters) > show:
        print(f"{'':10}   {'moved':6} {'':16} ... and {len(counters) - show} more")
    return len(new), len(gone), len(counters)
