"""The wall-clock budget, the pin table, and what makes a corpus measurable at all."""

import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import corpora as pins                                            # noqa: E402  the pin table

ROOT = Path(__file__).resolve().parent.parent.parent
OUT = ROOT / "audit"
# What every server the audit starts is configured with, sent as `initializationOptions`.
SERVER_TOML = Path(__file__).resolve().parent / "server.toml"

# The wall-clock ceiling `make audit` is held to. A gate that takes half an hour runs at release
# time; one that runs in five minutes runs today. It is a starting point and is meant to grow
# once the harness has earned it — raise it here, re-run `cost`, and record both.
#
# **Raised 300 -> 360 on 2026-09-12, when discourse became the sixth corpus.** `places` was
# added the same day and cost 5.1 s of it, so the headroom below is still 34 s. Re-measured that
# day, one release binary, six corpora: 28.4 s of startup and 16.6 ms per position at two
# requests, against 22.4 s and 10.4 ms for five. The six-corpus run reads 320.1 s, which is 75 s
# more than the five-corpus 245.3 s and is what the new headroom is for — the draw was **not**
# resized, so every counter the other five report is unchanged and their baseline blocks stand.
#
# **Raised 360 -> 420 on 2026-09-14, and `PER_FILE` deliberately did not move with it.** Two keys
# were added that day — `lane1.calls` poses `completion` at the draw's 848 receiverless calls and
# `lane1.closures` scans for a shape the draw does not target — and they cost **16.8 s**, measured
# as one release binary over the six corpora before and after: 381.4 s against 398.2 s. The run
# was already over the 360 it was sized against, so the raise makes the constant honest about what
# a sweep costs rather than buying a larger draw. **Re-running `cost` will now say the budget buys
# more positions; it does not follow that it should.** The draw is the thing every committed
# counter is a function of, and resizing it moves all of them at once — which is a decision to
# take on its own, for its own reason, and not one to inherit from an arithmetic identity.
BUDGET_SECONDS = 420
# Positions per sampled file, and therefore the size of the draw — **derived from the budget by
# `audit cost`, not chosen.** Measured 2026-09-10 over the five corpora at their pins, one
# release binary, `hover` + `definition`: 22.4 s of startup for all five (settle included) and
# 10.4 ms per position, so the budget's remaining 278 s buys ~26,600 positions at two requests
# each. Lane 2 asks more than two — `documentHighlight`, and every position a second time after
# a `didChange` that does not touch it — so size against ~25 ms and ~11,000 rather than 26,600.
# 32 draws 4,650 positions and lane 2's three requests answer them in 115 s of the 300 s, so the
# budget is not the binding constraint at this size. The draw is **sub-linear** in this number
# (12 -> 2,855, 24 -> 4,277, 32 -> 4,929): the corpora run out of candidates in the thin shapes
# before the abundant ones do, which is `OVERDRAW` holding the mix rather than a bug. It read
# 5,023 before `SKIP` stopped admitting `spec/` and `test/` to the strata, and 4,929 before the
# `route` shape was filtered down to helpers the corpus does not write down itself.
PER_FILE = 32
# What one server costs before it answers anything: the cold index plus `settle`'s quiet period,
# times five corpora. Measured by `cost`, subtracted from the budget before the draw is sized.
QUIET = 3.0
CEILING = 240.0
# How many distinct `def` places `places.ask` hovers per corpus, 0 for all of them. Derived the
# way `PER_FILE` was — from what it costs, measured, and recorded here beside the run that
# produced the number.
#
# **All of them, because the cap turned out to buy nothing.** Measured 2026-09-12 over six
# corpora on one release binary: **31,924 distinct places asked and 5.1 s added** to a 320.5 s
# run — lobsters 22.9 -> 22.9, discourse 76.4 -> 77.6. A hover on a file nobody has opened reads
# from disk and answers from the settled graph, and the pass pipelines like every other, so the
# cost is dominated by the requests the sample was always going to make. The flag stays for a
# corpus that one day changes that.
PLACES_CAP = 0
# What one corpus costs, for splitting the six across the queues `score --jobs` runs. **Seconds,
# measured serially** — one release binary, 2026-09-14, the whole draw and every lane: the run
# that produced them read 384.1 s for all six. Uncontended on purpose: these are weights, and a
# weight taken under contention would carry the schedule it was measured under into the schedule
# it is used to pick.
#
# **The absolutes predate `ask_rebased`'s protocol change of 2026-09-15 and were not re-measured;
# the ordering was.** That pass now issues an edit per position rather than one per document, so
# every figure here is low by roughly a factor of two — but a weight is only ever read against the
# other five, and the first run on the new protocol ranked them discourse > chatwoot > mastodon >
# forem > solidus > lobsters, which is this table's order exactly. The queues it picked came out
# 261.5 / 255.1 / 263.2 s, inside 3%. Re-measure serially before quoting any number below as a
# cost; the split does not need it.
#
# **Three queues is the default, and it is as much as parallelism gives here.** Same draw, same
# binary, same day: serial 384.1 s; two queues 237.0 s wall for 473.0 s summed; three queues
# 198.2 s wall for 574.1 s summed. The second queue buys 38% of the wall for 23% more CPU and the
# third buys 16% more for another 26%, which is the trade taken deliberately — wall clock is what
# a person waits on and this machine has the cores. **A fourth queue is not worth measuring
# until discourse is split**: it is alone on the critical path, and its own pass already grows
# from 98.3 s serial to 118.0 s at two queues and 157.6 s at three. **All three runs recorded
# byte-identical counters and findings**, which is the property that makes any of this safe: a
# queue changes when a corpus is asked and nothing about what it answers.
QUEUE_WEIGHT = {"discourse": 98.3, "chatwoot": 83.9, "mastodon": 68.8, "forem": 66.4,
                "solidus": 41.0, "lobsters": 25.0}


def server_options():
    """`server.toml` as the `initializationOptions` the audit sends every server.

    One rule applied on the way past: a **relative** `log.file_path` is resolved against the
    ya-lsp checkout rather than against the corpus. A corpus is a clone pinned to a commit and
    `check_clean` refuses to measure a dirty one, so nothing the audit does may put a file inside
    one — and the server resolves a relative path against the *workspace* root, which is the
    corpus. Absolute here means the server takes it as written.
    """
    with SERVER_TOML.open("rb") as handle:
        options = tomllib.load(handle)
    log = options.get("log")
    if log and not Path(log.get("file_path", "")).is_absolute():
        log["file_path"] = str(ROOT / log.get("file_path", "tmp/ya-lsp.log"))
    return options


def fresh_log():
    """Start a sweep with an empty log, and say where it is.

    **The server never truncates** — a second window on the same project is a second process
    writing to the same file, and throwing away the session somebody is trying to report is the
    one thing a log file must not do. A sweep is the opposite case: it is one run of one
    instrument, six servers it started itself, and nothing else writing there. Without this the
    file grows by ~25 MB every `make audit` and a reader has no way to tell this run's lines from
    last week's.

    Silent when `[log] file` is off, and silent when the file is not there yet.
    """
    log = (server_options().get("log") or {})
    if not log.get("file"):
        return None
    path = Path(log.get("file_path", ""))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("", encoding="utf-8")
    return path


def sweepable(only=None):
    """The corpora the audit may ask questions of: everything but `static-only`.

    The role is read from the pin table rather than named here, so the decision has one home.
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
