"""What one request makes a person wait, which is not what a sweep's wall clock divides out.

`audit cost` reports a **throughput**: a batch posted 32 deep, answered in order, divided by a
count. Right for sizing the draw; not what an editor blocks on. Here every request is posted and
answered alone, before anything is edited, and the report is the distribution of that wait.

**The empty count sits beside every median, and here an empty answer is the slow one.** The usual
worry is declines flattering a median. Here it is the reverse: `completion::by_name` ranks
candidates until it passes `MAX_UNTYPED_CANDIDATES` and then drops the whole list, so an empty
member popup has walked the whole name universe to produce nothing, and the p50 over *answered*
requests sits below the p50 over all. So every table prints `n`, the empty count, and the quantiles
twice: over every request, and over answered ones. Neither alone is the latency.

**Member cursors, because that is where the popup is used** and where the `empty` column means
something. `--shape` asks elsewhere; `--shape any` asks at the whole draw.

**The first request after each `didOpen` is timed apart and kept out of the histograms.** An open
can wake the settle, and a request queued behind it measures the settle. It is printed on its own
line because a person feels that wait too (the first question about a file just opened). It uses a
*fourth* method, because a discarded warm-up timing one of the three would bias the histogram it was
left out of.

**Two runs; the second measures the instrument.** `--runs 2` sweeps everything twice on one binary
and prints the spread beside the numbers, so a reader can tell a change from the machine's noise
floor. With two binaries, alternate; with one, the second pass is a repeat.

**Not measured: a keystroke, the wait a person meets most.** A `didChange` followed by a completion
answers over the *last settled graph* through `position::Rebase` (`concurrency.md`), and this pass
asks an unedited buffer. It times per-request work, which is what a request-path change moves, not
typing latency.

Not part of `make audit`: serial is the point, and one request in flight does not fit the budget.
`make audit-latency` is the target, like `audit-prefix` and `audit-rank`. It records nothing, diffs
nothing, and is run by a person who wants a number.
"""

import hashlib
import statistics
import subprocess
import time
from datetime import date

from audit.answers import card_of, locations
from audit.client import open_document, start, uri
from audit.config import ROOT
from audit.lane1.completion import CONTEXT
from audit.sample import positions

# The three requests with a latency worth a percentile.
# - `completion` is the subject: the one on a keystroke loop.
# - `hover` and `definition` are timed in the same pass, so a change to the request path all three
#   share shows up as itself, not as completion's.
METHODS = ("textDocument/completion", "textDocument/hover", "textDocument/definition")
# What the discarded first ask after a `didOpen` uses: deliberately none of the three.
WARM = "textDocument/documentHighlight"
# Nothing here is diffed or recorded, so this prints the statistics that answer the question. `max`
# is there because a p95 cannot say whether the tail is one cursor or a hundred.
QUANTILES = ("mean", "p50", "p95", "max")


def sha_of(path):
    """The binary's own hash, so a table of numbers names the thing that produced them."""
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _git(*args):
    try:
        out = subprocess.run(("git", "-C", str(ROOT)) + args, capture_output=True, text=True,
                             timeout=10)
        return out.stdout.strip() if out.returncode == 0 else "?"
    except (OSError, subprocess.SubprocessError):
        return "?"


def manifest(server, args):
    """The facts a reader needs before a millisecond means anything.

    Printed by the command, to copy into `completion.md` above the table. A latency without the
    binary, the tree and the draw that produced it compares with nothing, and every corpus row below
    carries its pin too.
    """
    tree = _git("rev-parse", "--short", "HEAD")
    if _git("status", "--porcelain"):
        tree += " + uncommitted"
    shape = getattr(args, "shape", "member")
    cap = getattr(args, "n", 0) or 0
    return "\n".join((
        f"{'measured':10} {date.today().isoformat()}",
        f"{'server':10} {server}",
        f"{'':10} sha256 {sha_of(server)[:16]}, tree {tree}",
        f"{'draw':10} seed {args.seed}, per-file {args.per_file}, shape {shape}, "
        f"cap {cap or 'none'}, runs {max(1, getattr(args, 'runs', 1))}",
    ))


def quantiles(ms):
    """`n`, `mean`, `p50`, `p95` and `max` of a list of milliseconds; zeroes for an empty one."""
    if not ms:
        return {"n": 0, "mean": 0.0, "p50": 0.0, "p95": 0.0, "max": 0.0}
    ordered = sorted(ms)
    last = len(ordered) - 1
    return {"n": len(ordered), "mean": statistics.fmean(ordered),
            "p50": ordered[min(last, len(ordered) // 2)],
            "p95": ordered[min(last, int(0.95 * len(ordered)))],
            "max": ordered[last]}


def declined(method, answer):
    """Did the server come back with nothing? One reading per method, because the reply shapes
    differ.

    Not an error and not a defect (the lanes judge answers). It is only what a median must be read
    against.
    """
    if method == "textDocument/completion":
        rows = answer.get("items") if isinstance(answer, dict) else answer
        return not rows
    if method == "textDocument/definition":
        return not locations(answer)
    return not card_of(answer)


def cursors(corpus, args):
    """The drawn positions this pass asks at, in the draw's own order."""
    shape = getattr(args, "shape", "member")
    drawn = positions(corpus, args.seed, args.per_file)
    if shape != "any":
        drawn = [row for row in drawn if row[1] == shape]
    cap = getattr(args, "n", 0) or 0
    return drawn[:cap] if cap else drawn


def sweep(server, corpus, drawn):
    """One settled server, every cursor asked three times, one request in flight throughout.

    The ordering is the measurement: a document is opened, warmed with the fourth method, then
    timed. The three methods are asked back to back at one cursor, not in three passes, so a
    document's first-touch cost lands in the warm-up instead of on whichever method ran first.
    """
    began = time.time()
    client, _, why = start(server, corpus)
    settled = time.time() - began
    timed = {method: {"all": [], "answered": [], "empty": 0} for method in METHODS}
    warm, opened, slowest = [], set(), []
    for _, _, path, line, column, _, _ in drawn:
        params = {"textDocument": {"uri": uri(corpus.dir / path)},
                  "position": {"line": line, "character": column}}
        if path not in opened:
            open_document(client, corpus, path)
            opened.add(path)
            at = time.perf_counter()
            client.ask(WARM, params)
            warm.append(1000 * (time.perf_counter() - at))
        for method in METHODS:
            asked = dict(params)
            if method == "textDocument/completion":
                asked["context"] = CONTEXT
            at = time.perf_counter()
            answer = client.ask(method, asked)
            took = 1000 * (time.perf_counter() - at)
            cell = timed[method]
            cell["all"].append(took)
            if declined(method, answer):
                cell["empty"] += 1
            else:
                cell["answered"].append(took)
            slowest.append((took, method.split("/")[1], f"{path}:{line + 1}"))
    warnings = list(client.warnings)
    client.stop()
    slowest.sort(reverse=True)
    return {"settle": settled, "why": why, "cursors": len(drawn), "files": len(opened),
            "warm": warm, "methods": timed, "warnings": warnings, "slowest": slowest[:16]}


HEAD = (f"  {'method':13} {'n':>5} {'empty':>11}   "
        f"{'mean':>6} {'p50':>6} {'p95':>6} {'max':>7}   {'p50':>6} {'p95':>6} {'max':>7}\n"
        f"  {'':13} {'':>5} {'':>11}   {'— every request —':^28}   {'— answered —':^21}")


def _row(label, cell):
    every, answered = quantiles(cell["all"]), quantiles(cell["answered"])
    empty = cell["empty"]
    share = f"{100.0 * empty / every['n']:3.0f}%" if every["n"] else "   -"
    return (f"  {label:13} {every['n']:5} {empty:6} {share}   "
            f"{every['mean']:6.1f} {every['p50']:6.1f} {every['p95']:6.1f} {every['max']:7.1f}   "
            f"{answered['p50']:6.1f} {answered['p95']:6.1f} {answered['max']:7.1f}")


def report(name, result, trace=0):
    """One corpus' table, as read while the pass runs.

    `trace` prints the slowest cursors underneath as `path:line`: `audit.site`'s spelling minus the
    offset, carrying no word out of the corpus. A `max` nobody can turn into a position is a number
    to worry about, not one to act on.
    """
    lines = [f"{name:10} {result['cursors']:5} cursors over {result['files']:4} files, "
             f"settled in {result['settle']:5.1f}s ({result['why']})", HEAD]
    for method in METHODS:
        lines.append(_row(method.split("/")[1], result["methods"][method]))
    first = quantiles(result["warm"])
    lines.append(f"  {'(first ask after didOpen, excluded above)':50} "
                 f"{first['n']:5}  mean {first['mean']:6.1f}  p50 {first['p50']:6.1f}  "
                 f"p95 {first['p95']:6.1f}  max {first['max']:7.1f}")
    for took, method, where in result["slowest"][:trace]:
        lines.append(f"  {'slowest':13} {took:7.1f}  {method:18} {where}")
    for warning in result["warnings"][:4]:
        lines.append(f"  server said: {warning}")
    return "\n".join(lines)


def _spread(values):
    """How far apart the runs are, as a percentage of the smaller. `-` when one run is zero."""
    low, high = min(values), max(values)
    return f"{100.0 * (high - low) / low:4.0f}%" if low else "   -"


def noise(runs, method):
    """Every run's p50, p95 and empty count for one method, per corpus, with the spread.

    **This says whether a difference is a difference.** A request-path change is worth reporting
    when it moves a median further than this table's worst spread, and not otherwise. Without it, a
    5% improvement and a warm page cache read the same.
    """
    names = [name for name, _ in runs[0]]
    width = max(len(name) for name in names) if names else 6
    count = len(runs)
    lines = [f"{method.split('/')[1]}, {count} runs of one binary",
             f"  {'corpus':{width}}   " + "  ".join(f"{'p50':>6}" for _ in range(count))
             + " spread   " + "  ".join(f"{'p95':>6}" for _ in range(count))
             + " spread   " + "  ".join(f"{'empty':>6}" for _ in range(count))]
    worst = 0.0
    for index, name in enumerate(names):
        cells = [run[index][1]["methods"][method] for run in runs]
        fifty = [quantiles(cell["all"])["p50"] for cell in cells]
        ninety = [quantiles(cell["all"])["p95"] for cell in cells]
        empties = [cell["empty"] for cell in cells]
        for values in (fifty, ninety):
            if min(values):
                worst = max(worst, 100.0 * (max(values) - min(values)) / min(values))
        lines.append(f"  {name:{width}}   "
                     + "  ".join(f"{value:6.1f}" for value in fifty) + f"  {_spread(fifty)}   "
                     + "  ".join(f"{value:6.1f}" for value in ninety) + f"  {_spread(ninety)}   "
                     + "  ".join(f"{value:6}" for value in empties))
    lines.append(f"  worst spread between runs: {worst:.0f}% — a move smaller than this is the "
                 f"machine, not the server")
    return "\n".join(lines)
