"""One corpus' result, the same numbers over all corpora, and this run against the last.

Nothing here knows how many checks there are or what they measure. Both functions walk `lane1.KEYS`
and `lane2.CHECKS` and ask each for its own line, so adding a check is a one-file change. A format
string here would drift from the counters it prints.
"""

from audit import baseline, declarations, lane1, lane2, lane3, places


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
        # The one way this lane can be wrong without failing: `hover.rs` rewords a footnote, nothing
        # matches, every card reads as Resolved, and check 2 reports a flood or a zero that means
        # nothing. Refuse to be believed instead.
        print(f"{'':10} BROKEN   every card read as Resolved — the sentences in GUESSED and "
              f"DERIVED no longer match hover.rs")
        return
    print(f"{'':10} tiers    resolved {tiers['resolved']}  derived {tiers['derived']}  "
          f"guessed {tiers['guessed']}")
    print(f"{'':10} places   {_places(counts)}")
    print(f"{'':10} described {places.line(counts)}")
    if counts.get("declarations"):
        print(f"{'':10} declared  {declarations.line(counts['declarations'])}")
    for number, check in enumerate(lane2.CHECKS, 1):
        print(f"{'':10} {'check ' + str(number):8} {check.line(counts)}")
        for extra in _lines(check, "under", counts):
            print(f"{'':10}   {extra}")
    for key in lane1.KEYS:
        cell = (counts.get("keys") or {}).get(key.NAME)
        # `TOTAL`, not a fixed name: the neutral key's denominator is `knowable`, the Rails key's is
        # `asked`. A key with no rows of its own prints nothing, not a row of zeroes nobody can read
        # a rate off.
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

    Printed with no verdict, which is why they are here and not in a check: `minority` is not a
    defect (a name spread over forty gems has no majority), and this line may not call a card naming
    an id a defect either. They exist to show **movement**: re-ordering a place list or naming an
    anonymous class moves nothing in `shape/tier/places`, so without these a fix to either is
    invisible to every lane.
    """
    first = counts["first-place"]
    lists = sum(first.values())
    return (f"{lists} lists of 2+ places: {first['one-library']} in one library, "
            f"{first['majority']} opening on the library most of them are in, "
            f"{first['minority']} not; {counts['cards-anonymous']} cards name an internal id")


def kinds():
    """Every finding kind, in print order: the keys first, then check by check.

    **A kind missing from here is silently dropped from the report**: collected, counted, never
    printed. `declarations` is listed although it is a measurement, not a check, because it raises
    one finding: `implementation` answering at a declaration without the declaration's own line
    contradicts `hierarchy.md` outright.
    """
    out = []
    for module in tuple(lane1.KEYS) + tuple(lane2.CHECKS) + (lane3, declarations):
        out.extend(module.FINDINGS)
    return out


def totals(totals_, keyed, positions, spent, budget):
    """The all-corpora line, and one line per check and per key under it.

    **`budget` is `None` when this run's seconds cannot be compared with it**, and the line then
    says `wall` and names no budget. `BUDGET_SECONDS` is what the whole draw costs one server
    answering alone, so only a serial sweep of every corpus is the same kind of number: a partial
    sweep is a fraction of the draw, and a parallel one is a wall clock several servers shared.
    Printing a `--jobs 3` wall against the budget would suggest room the serial draw does not have.
    """
    against = f"of the {budget}s budget" if budget else "wall"
    print()
    print(f"{'all':10} {positions:5} positions  {spent:5.1f}s {against}   "
          f"hover {totals_['hover']}   definition {totals_['definition']}   "
          f"highlight {totals_['highlight']}")
    print(f"{'':10} places   {_places(totals_)}")
    print(f"{'':10} described {places.line(totals_)}")
    if totals_.get("declarations"):
        print(f"{'':10} declared  {declarations.summary(totals_['declarations'])}")
    for number, check in enumerate(lane2.CHECKS, 1):
        print(f"{'':10} {'check ' + str(number):8} {check.summary(totals_)}")
    for key in lane1.KEYS:
        cell = keyed.get(key.NAME)
        if cell and cell.get(key.TOTAL):
            print(f"{'':10} {key.NAME:8} {key.summary(cell)}")
    if "residue" in totals_:
        print(f"{'':10} {'lane 3':8} {lane3.summary(totals_)}")


def _delta(was, now):
    """One counter's movement. An absent side is named as absent, never printed as zero."""
    if was is None:
        return f"{'':>7} -> {now:>7}   new counter"
    if now is None:
        return f"{was:>7} -> {'':>7}   counter dropped"
    return f"{was:>7} -> {now:>7}   {now - was:+d}"


def moved(name, before, now, show):
    """One corpus, this run against the baseline.

    Returns `(new, gone, counters)`, or **`None` when no comparison happened**: a corpus with no
    baseline row, or whose pin or draw moved, is "not compared". Folding either into zeroes would
    count it in the total as a corpus that agreed.

    **Findings first, counters second: the one judgement this function makes.** A finding is a
    defect a check already named, so new is a regression and gone is a fix, with no opinion needed.
    Whether a counter going up is good depends on the counter, and the check that owns it holds that
    answer. A table of directions here would copy it to a second place, so a counter prints with its
    sign and nothing else.
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
