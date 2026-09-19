"""The wall-clock budget, the pin table, and what makes a corpus measurable."""

import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import corpora as pins                                            # noqa: E402  the pin table

ROOT = Path(__file__).resolve().parent.parent.parent
OUT = ROOT / "audit"
# The settings every audited server gets, sent as `initializationOptions`.
SERVER_TOML = Path(__file__).resolve().parent / "server.toml"

# The wall-clock ceiling `make audit` is held to.
#
# - **Why a ceiling:** a gate that takes half an hour runs at release time; one that takes five
#   minutes runs today.
# - **To raise it:** change it here, re-run `cost`, and record both.
# - **Raising it never resizes the draw.** Every committed counter depends on the draw, so resizing
#   it is a decision of its own, with its own reason.
# - **A check that costs more than about a minute takes a fixed subset**, not a bigger budget.
#   `lane2.references.STRIDE` is that subset, for checks 7 and 9.
# - **Memory is the cost this cannot see.** Check 7's places sit in one `answers` map for a whole
#   corpus.
#
# The sum of `QUEUE_WEIGHT` is the last serial sweep's cost. Compare it with this.
BUDGET_SECONDS = 420
# Positions per sampled file, which sets the size of the draw.
#
# - **Derived from the budget by `audit cost`, not chosen.**
# - **`cost` times two requests per position; a sweep asks more.** Lane 2 adds `documentHighlight`
#   and re-asks after an untouching `didChange`, so size against the smaller number `cost` prints.
# - **The draw is sub-linear in this number.** The corpora run out of the thin shapes before the
#   abundant ones. That is `OVERDRAW` holding the mix, not a bug.
PER_FILE = 32
# `client.settle`'s quiet period, and the longest it waits for a cold server.
QUIET = 3.0
CEILING = 240.0
# How many distinct `def` places `places.ask` hovers per corpus; 0 means all of them.
#
# All of them, because a cap buys nothing. A hover on an unopened file answers from the settled
# graph, and the pass pipelines like every other, so the whole pass costs a few seconds. The flag
# stays for a corpus that changes that.
PLACES_CAP = 0
# What each corpus costs in seconds, for splitting the six across `score --jobs` queues.
#
# - **Measured serially:** one release binary, the whole draw, every lane. A weight taken under
#   contention carries that run's schedule into the one it picks.
# - **Re-measure after any change that adds requests:** one serial sweep,
#   `make audit ARGS="--jobs 1"`. A stale table cannot say it is stale.
# - **The default is three queues.** A fourth saves little: discourse alone is the critical path, so
#   no split beats discourse's own time until discourse itself is split.
# - **A queue changes when a corpus is asked, never what it answers.** Runs at one to four queues
#   recorded identical counters and findings.
# - **Summed seconds grow with contention.** Only a serial run over every corpus is compared with
#   `BUDGET_SECONDS` (`cmd_score`).
QUEUE_WEIGHT = {"discourse": 99.9, "chatwoot": 54.7, "forem": 46.0, "mastodon": 43.6,
                "solidus": 40.3, "lobsters": 19.6}


def server_options():
    """`server.toml` as the `initializationOptions` every audited server gets.

    A **relative** `log.file_path` is resolved against the ya-lsp checkout, not the corpus:
    - the server resolves a relative path against the workspace root, which is the corpus;
    - a file inside a corpus makes it dirty, and `check_clean` refuses to measure a dirty one.

    An absolute path is sent as written.
    """
    with SERVER_TOML.open("rb") as handle:
        options = tomllib.load(handle)
    log = options.get("log")
    if log and not Path(log.get("file_path", "")).is_absolute():
        log["file_path"] = str(ROOT / log.get("file_path", "tmp/ya-lsp.log"))
    return options


def fresh_log():
    """Empty the log at the start of a sweep, and return where it is.

    **The server never truncates its log.** A second window on the same project writes to the same
    file, and throwing away the session somebody is about to report is what a log must never do.

    A sweep is the opposite case: six servers it started itself, and nothing else writing. Without
    this, the file grows every `make audit` and this run's lines mix with last week's.

    Returns None, silently, when `[log] file` is off.
    """
    log = (server_options().get("log") or {})
    if not log.get("file"):
        return None
    path = Path(log.get("file_path", ""))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("", encoding="utf-8")
    return path


def sweepable(only=None):
    """The corpora the audit may ask questions of: every one whose role is not `static-only`.

    The role comes from the pin table, so the decision has one home.
    """
    return [c for c in pins.load_table(only=only) if c.role != "static-only"]


def check_clean(corpus):
    """Refuse to measure a corpus that has drifted from its pin. Returns a reason or None."""
    if not corpus.is_clone:
        return "not a clone"
    head = corpus.head()
    if head != corpus.sha:
        return f"at {(head or '?')[:12]}, pinned {corpus.sha[:12]}"
    if corpus.dirty():
        return "working tree dirty"
    return None
