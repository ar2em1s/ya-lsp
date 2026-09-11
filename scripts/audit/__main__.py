"""`python3 scripts/audit <command>` — the argument parser and nothing else."""

import argparse
import sys
from pathlib import Path

# **Line-buffered, so a sweep can be watched.** Python block-buffers stdout whenever it is not a
# terminal, and `make audit` is the one command here that runs for minutes: piped, redirected or
# backgrounded it printed every per-corpus line at once on exit, which reads exactly like a hang.
# Set on the stream rather than asked of each `print`, and here rather than in the Makefile, so
# that it holds however this was invoked. A terminal is line-buffered already, so this is a no-op
# for the interactive run.
sys.stdout.reconfigure(line_buffering=True)

# `scripts/` on the path, so that `audit.*` and the pin table both import by name however this
# was invoked: `python3 scripts/audit` puts `scripts/audit` itself on the path and neither name
# is reachable from there.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import audit                                                              # noqa: E402
from audit.baseline import PATH as BASELINE                              # noqa: E402
from audit.commands import (cmd_adjudicate, cmd_cost, cmd_ledger,         # noqa: E402
                            cmd_prefix, cmd_rank, cmd_report, cmd_sample,
                            cmd_score)
from audit.config import PER_FILE, PLACES_CAP, ROOT, pins                             # noqa: E402
from audit.lane3.ledger import PATH as LEDGER                             # noqa: E402

COMMANDS = {"sample": cmd_sample, "cost": cmd_cost, "score": cmd_score,
            "report": cmd_report, "ledger": cmd_ledger, "adjudicate": cmd_adjudicate,
            "prefix": cmd_prefix, "rank": cmd_rank}


def main():
    # The shared flags go on a `parents=` parser rather than on the top level, so that
    # `audit sample --only lobsters` works. With them on the top level argparse requires
    # them *before* the subcommand, which is the spelling nobody types.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--server", default=str(ROOT / "target/release/ya-lsp"))
    common.add_argument("--seed", default="0", help="the draw is a function of this and the SHA")
    common.add_argument("--per-file", type=int, default=PER_FILE,
                        help="positions per sampled file; the draw is sub-linear in it")
    common.add_argument("--only", action="append", help="one corpus; repeatable")
    common.add_argument("--places-cap", type=int, default=PLACES_CAP,
                        help="distinct `def` places to ask about per corpus; 0 for all")
    parser = argparse.ArgumentParser(prog="audit", parents=[common],
                                     description=audit.__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    draw = sub.add_parser("sample", parents=[common],
                          help="draw and print the sample; write nothing")
    draw.add_argument("--check", action="store_true",
                      help="also run the mask's worked examples and walk every .rb file for one "
                           "the scan lost its place in")
    cost = sub.add_parser("cost", parents=[common],
                          help="per-position latency, and what the budget buys")
    cost.add_argument("-n", type=int, default=200, help="positions per corpus to time")
    score = sub.add_parser("score", parents=[common],
                           help="both lanes over the whole draw")
    score.add_argument("-n", type=int, default=0, help="cap positions per corpus (0 = the draw)")
    score.add_argument("--show", type=int, default=8, help="findings to print per check")
    score.add_argument("--jobs", type=int, default=3,
                       help="corpora swept in parallel, one queue each (1 = serial)")
    score.add_argument("--eager-only", action="store_true",
                       help="skip check 5's second pass after a didChange")
    score.add_argument("--no-key", action="store_true",
                       help="lane 2 only; skip lane 1's keys (which walk the corpus)")
    score.add_argument("--ledger", default=str(LEDGER),
                       help="the ledger lane 3 reuses verdicts from")
    score.add_argument("--record", metavar="PATH",
                       help="write this run in the shape `audit report` diffs; the only thing "
                            "`score` writes, and it holds no corpus text")
    score.add_argument("--pending", metavar="PATH",
                       help="write lane 3's residue there as unadjudicated rows, for a person "
                            "to fill in; never the ledger itself")
    diff = sub.add_parser("report", parents=[common],
                          help="a recorded run against the committed baseline")
    diff.add_argument("record", help="the run to report on (`score --record` wrote it)")
    diff.add_argument("--baseline", default=str(BASELINE), help="the run to compare against")
    diff.add_argument("--save", action="store_true",
                      help="merge this run into the baseline afterwards")
    diff.add_argument("--show", type=int, default=8, help="rows to print per kind")
    judge = sub.add_parser("adjudicate", parents=[common],
                           help="show a batch of lane 3's residue, with what ya-lsp said there")
    judge.add_argument("-n", "--batch", dest="n_batch", type=int, default=10,
                       help="positions per corpus to present")
    judge.add_argument("--start", type=int, default=0, help="skip this many of the residue")
    judge.add_argument("--shape", help="only this cursor shape (symbol, ivar, route, ...)")
    judge.add_argument("--context", type=int, default=1, help="source lines either side")
    judge.add_argument("--targets", type=int, default=2,
                       help="targets to print the destination source of")
    judge.add_argument("--ledger", default=str(LEDGER), help="verdicts already recorded")
    judge.add_argument("--out", help="write the batch as blank rows there")
    judge.set_defaults(n=0, eager_only=True, no_key=False, show=0)
    step = sub.add_parser("prefix", parents=[common],
                          help="what the untyped completion list costs and buys at each prefix "
                               "length; sets MAX_UNTYPED_COMPLETION_ITEMS")
    step.add_argument("-n", type=int, default=150,
                      help="member cursors per corpus to follow (0 = every one drawn); each "
                           "costs up to seven requests, so the draw is capped by default")
    seat = sub.add_parser("rank", parents=[common],
                          help="where the member sits in a typed completion list; sets "
                               "MAX_COMPLETION_ITEMS")
    seat.add_argument("-n", type=int, default=0,
                      help="member cursors per corpus to ask (0 = every one drawn); one request "
                           "each, so the whole draw is affordable")
    seat.add_argument("--shape", default="member",
                      help="which drawn cursor shape to ask at. `member` is after a dot, where "
                           "the typed ceiling lives; `call` is a bare word, the only shape where "
                           "Ruby's keywords are offered at all")
    seat.add_argument("--steps", type=int, default=3,
                      help="characters of the word to follow the cursor through; 0 asks only at "
                           "the word's start, which is a quarter of the requests")
    seat.add_argument("--trace", type=int, default=0,
                      help="print this many of the worst-ranked positions")
    look = sub.add_parser("ledger", parents=[common],
                          help="what is in the ledger, and whether it still applies")
    look.add_argument("--path", default=str(LEDGER), help="the ledger to read")
    args = parser.parse_args()
    COMMANDS[args.command](args)


if __name__ == "__main__":
    try:
        main()
    except pins.Fail as failure:
        sys.exit(f"audit: {failure}")
    except KeyboardInterrupt:
        sys.exit(130)
