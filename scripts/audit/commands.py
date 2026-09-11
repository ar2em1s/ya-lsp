"""What each subcommand does. One function per verb, and none of them knows what a check is."""

import os
import sys
import threading
import time
from pathlib import Path

from audit import baseline, lane1, lane2, lane3, places, report
from audit.answers import card_of, locations, path_of, tier
from audit.client import ask_all, ask_rebased, start, uri
from audit.config import (BUDGET_SECONDS, PLACES_CAP, QUEUE_WEIGHT, check_clean,
                          fresh_log, sweepable)
from audit.lane1.completion import CONTEXT, IN_FLIGHT, rank_of, setter
from audit.lane3 import ledger
from audit.ruby import EXAMPLES, check, keeps_place, line_key
from audit.sample import positions
from audit.shapes import SHARES


def cmd_sample(args):
    """Draw the sample and print it. Writes nothing, starts no server, asks nothing.

    The mix is printed beside `SHARES`' stated targets rather than on its own, because the only
    thing worth checking here is whether the draw came out as the design says — a `member` column
    at 57% against a stated 45% is the sample quietly re-confirming the positions an existing key
    already covers, and it is invisible in a list of counts.
    """
    total, walked, lost = {}, 0, []
    for corpus in sweepable(args.only):
        drift = check_clean(corpus)
        if drift:
            print(f"{corpus.name:10} SKIPPED  {drift}")
            continue
        if args.check:
            for path in corpus.dir.rglob("*.rb"):
                walked += 1
                if not keeps_place(path.read_text(encoding="utf-8", errors="replace")):
                    lost.append(str(path.relative_to(corpus.dir)))
        drawn = positions(corpus, args.seed, args.per_file)
        shapes, strata, files = {}, {}, set()
        for stratum, shape, path, _, _, _, _ in drawn:
            shapes[shape] = shapes.get(shape, 0) + 1
            strata[stratum] = strata.get(stratum, 0) + 1
            files.add(path)
            total[shape] = total.get(shape, 0) + 1
        print(f"{corpus.name:10} {len(drawn):5} positions over {len(files):4} files")
        print(f"{'':10}   shapes   " + "  ".join(
            f"{name} {shapes.get(name, 0)}" for name, _ in SHARES))
        print(f"{'':10}   strata   " + "  ".join(
            f"{name} {strata.get(name, 0)}" for name, _ in
            sorted(strata.items(), key=lambda kv: -kv[1])))
    drew = sum(total.values())
    if not drew:
        return
    print()
    print(f"{'all':10} {drew:5} positions")
    for name, share in SHARES:
        got = total.get(name, 0)
        print(f"{'':10}   {name:9} {got:5}  {100.0 * got / drew:5.1f}%  "
              f"against {100.0 * share:5.1f}% stated")
    if not args.check:
        return
    # **The two invariants on the mask, which is the part of the draw that has been wrong most.**
    # `audit.md` records the six bugs; these are what each one would have caught. A file whose
    # last top-level `end` is masked is a file the scan lost its place in, which is the only way
    # this scanner fails catastrophically — and it fails silently, by drawing nothing.
    print()
    wrong = check()
    print(f"{'mask':10} {len(EXAMPLES) - len(wrong)} of {len(EXAMPLES)} worked examples right; "
          f"{walked - len(lost)} of {walked} files keep their place")
    for line, want, got in wrong:
        print(f"{'':10}   line {line}\n{'':10}   want {want}\n{'':10}   got  {got}")
    for path in lost[:8]:
        print(f"{'':10}   lost its place  {path}")


def cmd_cost(args):
    """What one position costs, and therefore what `BUDGET_SECONDS` buys.

    Two numbers, and they are separated because they scale differently: **startup** is paid once
    per corpus however large the draw is, and **per position** is what the draw is sized against.
    Quoting a single average over a short run hides the first inside the second and sizes the
    draw too small.
    """
    server = args.server
    if not os.path.exists(server):
        sys.exit(f"no server at {server}; `make release` first")
    startup, asked, asking = 0.0, 0, 0.0
    for corpus in sweepable(args.only):
        drift = check_clean(corpus)
        if drift:
            print(f"{corpus.name:10} SKIPPED  {drift}")
            continue
        drawn = positions(corpus, args.seed, args.per_file)[:args.n]
        if not drawn:
            print(f"{corpus.name:10} SKIPPED  no positions drawn")
            continue
        began = time.time()
        client, _, why = start(server, corpus)
        cold = time.time() - began
        began = time.time()
        ask_all(client, corpus, drawn)
        spent = time.time() - began
        client.stop()
        startup += cold
        asked += len(drawn)
        asking += spent
        print(f"{corpus.name:10} {cold:5.1f}s to settle ({why})   {len(drawn):4} positions in "
              f"{spent:5.1f}s   {1000 * spent / len(drawn):5.1f} ms each")
    if not asked:
        return
    each = asking / asked
    # Lane 2 asks three requests per position and then a second pass over two of them, so the
    # per-position cost the draw must be sized against is not the one measured here with two.
    # The factor is stated rather than measured because it is a property of `lane2.METHODS` and
    # `ask_rebased`, not of the machine.
    left = BUDGET_SECONDS - startup
    print()
    print(f"{'all':10} {startup:5.1f}s of startup, {1000 * each:5.1f} ms per position at "
          f"two requests")
    print(f"{'':10} the {BUDGET_SECONDS}s budget leaves {left:5.1f}s, which buys "
          f"{int(left / each):6} positions at two requests")
    print(f"{'':10} lane 2 asks five, so size against {int(left / (each * 2.5)):6} — and the "
          f"draw is sub-linear in --per-file, so raise it and re-run `sample`")


METHOD_COMPLETION = "textDocument/completion"
MAX_PREFIX = 6
# The rows a person would actually read, which is the question a *display* ceiling answers and a
# candidate ceiling does not. Fixed here rather than read from the server, because the point of
# the band table is to compare one run against another run of a differently built binary, and a
# reporting constant that moved with the thing under test would compare nothing.
DISPLAY = 128


def _middle(values):
    """Median and p90 of a list of ints, or `(0, 0)` for an empty one.

    A median here where the `completion` key keeps everything an int on purpose: that key's
    counters are summed across corpora and diffed against a committed baseline, and neither
    operation means anything on a median. Nothing here is recorded or diffed — this is a probe a
    person runs to set a constant — so the statistic that answers the question is the one to
    print.
    """
    if not values:
        return 0, 0
    ordered = sorted(values)
    return ordered[len(ordered) // 2], ordered[min(len(ordered) - 1, int(0.9 * len(ordered)))]


def cmd_prefix(args):
    """What the **untyped** completion list costs and buys at each prefix length.

    `MAX_UNTYPED_COMPLETION_ITEMS` is the line above which a receiver with no type is answered
    with no rows at all, and **no standing measurement can see whether it is in the right place**.
    Every cursor anything here asks at is at a word's *start*, where the candidate set is the
    project's entire name universe and any bound in the plausible range declines identically.
    The value only bites mid-word, and nothing asks mid-word. This does.

    **It follows one cursor across seven prefixes, and classifies it at the first.** At `k = 0` a
    receiver the graph can type answers with its own members and one it cannot is declined, so the
    empty answer at `k = 0` *is* the classification — no card has to be read and no tier guessed.
    Only those cursors are followed outwards, because the bound touches no others.

    **What it can and cannot see, said plainly.** A list that comes back is under the bound, so
    its size is exact and the histogram below the line is real. A declined one is censored at the
    bound: it says *more than* the ceiling and never how much more. That is enough to answer
    *should this be smaller* — if answered lists cluster far below the line, a smaller bound is
    free — and not enough to answer *should it be larger*, which needs a second binary built with
    the constant raised, the way the 512 cap was measured.
    """
    server = args.server
    if not os.path.exists(server):
        sys.exit(f"no server at {server}; `make release` first")
    totals = {k: {"asked": 0, "answered": 0, "declined": 0, "present": 0,
                  "sizes": [], "ranks": [], "pairs": []} for k in range(MAX_PREFIX + 1)}
    for corpus in sweepable(args.only):
        drift = check_clean(corpus)
        if drift:
            print(f"{corpus.name:10} SKIPPED  {drift}")
            continue
        drawn = [row for row in positions(corpus, args.seed, args.per_file) if row[1] == "member"]
        if not drawn:
            print(f"{corpus.name:10} SKIPPED  no member positions drawn")
            continue
        client, _, why = start(server, corpus)
        texts, posed, replies = {}, [], {}
        for index, (_, _, path, line, column, _, word) in enumerate(drawn):
            if path not in texts:
                texts[path] = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
            if setter(texts[path], line, column, word):
                continue
            posed.append((index, path, line, column, word))
            for step in range(min(MAX_PREFIX, len(word)) + 1):
                client.post((index, step), METHOD_COMPLETION, {
                    "textDocument": {"uri": uri(corpus.dir / path)},
                    "position": {"line": line, "character": column + step},
                    "context": CONTEXT})
                for key, result in client.drain(down_to=IN_FLIGHT):
                    replies[key] = result
            if args.n and len(posed) >= args.n:
                break
        for key, result in client.drain():
            replies[key] = result
        client.stop()

        counts = {k: {"asked": 0, "answered": 0, "declined": 0, "present": 0,
                      "sizes": [], "ranks": [], "pairs": []} for k in range(MAX_PREFIX + 1)}
        untyped = 0
        for index, _, _, _, word in posed:
            first = replies.get((index, 0))
            items = first.get("items") if isinstance(first, dict) else first
            # The classification, and the whole reason it is taken here: at an empty prefix a
            # typed receiver answers with its members and an untyped one is over the bound by
            # every corpus' universe, so an empty answer names the path this probe is about.
            if items:
                continue
            untyped += 1
            for step in range(min(MAX_PREFIX, len(word)) + 1):
                answer = replies.get((index, step))
                rows = answer.get("items") if isinstance(answer, dict) else answer
                row = counts[step]
                row["asked"] += 1
                if not rows:
                    row["declined"] += 1
                    continue
                row["answered"] += 1
                row["sizes"].append(len(rows))
                rank = rank_of(rows, word)
                if rank is not None:
                    row["present"] += 1
                    row["ranks"].append(rank)
                    row["pairs"].append((len(rows), rank))
        print(f"{corpus.name:10} {untyped:4} untyped of {len(posed)} member cursors ({why})")
        _print_prefix(counts)
        for step, row in counts.items():
            for field in ("asked", "answered", "declined", "present"):
                totals[step][field] += row[field]
            totals[step]["sizes"].extend(row["sizes"])
            totals[step]["ranks"].extend(row["ranks"])
            totals[step]["pairs"].extend(row["pairs"])
    if not totals[0]["asked"]:
        return
    print()
    print(f"{'all':10} {totals[0]['asked']:4} untyped cursors followed outwards")
    _print_prefix(totals)
    print()
    print(f"{'':10} a list that came back is exact; a declined one says only "
          f"'over the bound'. Raising it needs a second binary.")


def _print_prefix(counts):
    print(f"{'':10}   {'chars':>5} {'asked':>6} {'offered':>8} {'declined':>9} "
          f"{'median':>7} {'p90':>6} {'present':>8} {'rank':>6} {'r/p90':>6}")
    for step in sorted(counts):
        row = counts[step]
        if not row["asked"]:
            continue
        median, p90 = _middle(row["sizes"])
        rank, rank90 = _middle(row["ranks"])
        offered = f"{row['answered']:6} {100.0 * row['answered'] / row['asked']:3.0f}%"
        print(f"{'':10}   {step:5} {row['asked']:6} {offered:>8} {row['declined']:9} "
              f"{median:7} {p90:6} {row['present']:8} {rank:6} {rank90:6}")
    # **Fixed powers of two, not the ceiling in force.** Run against a binary whose bound is
    # raised, this is the shape the censoring hides: how far over the shipped line a declined
    # list actually sits, and therefore what a larger ceiling would convert into a real list.
    # Reporting buckets have to be the same in both runs or the two cannot be laid side by side.
    pooled = [size for row in counts.values() for size in row["sizes"]]
    if pooled:
        edges, seen = (128, 256, 512, 1024), []
        for edge in edges:
            seen.append(f"<={edge} {sum(1 for size in pooled if size <= edge)}")
        seen.append(f">1024 {sum(1 for size in pooled if size > 1024)}")
        print(f"{'':10}   sizes  " + "   ".join(seen))
    # **What an admission ceiling above a display ceiling would actually cost.** A list in the
    # band `(low, high]` is one a bound of `high` admits and a bound of `low` declines; the word
    # is still reachable in it only if the ranking put it in the first `low` rows. Printed per
    # band because the answer is not the same at every size, and it is the whole question when a
    # *display* cap sits under an *admission* cap.
    pairs = [pair for row in counts.values() for pair in row["pairs"]]
    if pairs:
        for low, high in ((0, 128), (128, 256), (256, 512), (512, 1024), (1024, 1 << 30)):
            held = [rank for size, rank in pairs if low < size <= high]
            if not held:
                continue
            end = "up" if high > 1 << 20 else str(high)
            print(f"{'':10}   band   {low + 1}-{end}: {len(held):4} lists, "
                  f"{sum(1 for rank in held if rank <= DISPLAY):4} hold the word in the first "
                  f"{DISPLAY}")


def cmd_rank(args):
    """Where the member sits in a **typed** list, and therefore where `MAX_COMPLETION_ITEMS` goes.

    `cmd_prefix` is the same question asked of the other path. There the receiver has no type, the
    list is a guess, and the bound decides whether to answer at all; here the receiver *is* typed,
    the list is one this server believes in, and the bound decides only how much of it to send.

    **The classification is the same one and is taken the same way.** A list that comes back at
    `k = 0` is a typed receiver's own members; an empty one is the untyped decline. No card is
    read and no tier is guessed. Only typed cursors are followed outwards.

    **It follows the cursor through the word for the reason the other probe does**, and here that
    is the whole question rather than a refinement of it: at `k = 0` nothing has been typed, so
    `tier` is 1 and `length` is 0 for every row and the only live terms are where a name lives.
    A band of one owner's members is then in alphabetical order, and a *display* ceiling under it
    is only honest if the word comes back as soon as the word is being typed — which is what the
    table below measures and what `isIncomplete` is for.

    **What it can see.** Every rank it reports is exact, because the whole list came back — up to
    the ceiling in force. A word ranked *past* that ceiling arrives here as `absent`,
    indistinguishable from one the list never held, so this answers *should the ceiling be lower*
    on its own and *is it low enough to be losing answers* only against a second binary built with
    it raised. That is the asymmetry `cmd_prefix` states for the other bound, for the same reason.

    **`same-owner` is the diagnosis beside the count.** For each present word it counts how many
    of the rows above it are owned by the same class — read off the `detail` line, which is the
    owner rubydex attributed the member to. A rank that is almost entirely same-owner rows is the
    alphabet inside one band, which no ordering of the bands can improve; a rank made of other
    owners' rows is a band that sorted above the one holding the answer, which is a ranking
    question.
    """
    server = args.server
    if not os.path.exists(server):
        sys.exit(f"no server at {server}; `make release` first")
    # The table below is in fixed columns, so the deepest prefix it can hold is a constant and a
    # larger `--steps` is clamped to it rather than raising on the first reply it files.
    steps = max(0, min(args.steps, RANK_PREFIX))
    totals = _rank_counts()
    for corpus in sweepable(args.only):
        drift = check_clean(corpus)
        if drift:
            print(f"{corpus.name:10} SKIPPED  {drift}")
            continue
        shape = getattr(args, "shape", "member")
        drawn = [row for row in positions(corpus, args.seed, args.per_file)
                 if row[1] == shape]
        if not drawn:
            print(f"{corpus.name:10} SKIPPED  no {shape} positions drawn")
            continue
        client, _, why = start(server, corpus)
        texts, posed, replies = {}, [], {}
        for index, (_, _, path, line, column, _, word) in enumerate(drawn):
            if path not in texts:
                texts[path] = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
            if setter(texts[path], line, column, word):
                continue
            posed.append((index, path, line, column, word))
            for step in range(min(steps, len(word)) + 1):
                client.post((index, step), METHOD_COMPLETION, {
                    "textDocument": {"uri": uri(corpus.dir / path)},
                    "position": {"line": line, "character": column + step},
                    "context": CONTEXT})
                for key, result in client.drain(down_to=IN_FLIGHT):
                    replies[key] = result
            if args.n and len(posed) >= args.n:
                break
        for key, result in client.drain():
            replies[key] = result
        client.stop()

        counts = _rank_counts()
        for index, path, line, _, word in posed:
            first = replies.get((index, 0))
            rows = first.get("items") if isinstance(first, dict) else first
            # An empty list at an empty prefix is the untyped decline, which is `cmd_prefix`'s
            # subject entirely. Everything else is a receiver this server typed.
            if not rows:
                continue
            counts["typed"] += 1
            for step in range(min(steps, len(word)) + 1):
                answer = replies.get((index, step))
                at = answer.get("items") if isinstance(answer, dict) else answer
                row = counts["steps"][step]
                row["asked"] += 1
                if not at:
                    continue
                row["sizes"].append(len(at))
                rank = rank_of(at, word)
                if rank is None:
                    row["absent"] += 1
                    continue
                row["present"] += 1
                row["ranks"].append(rank)
            counts["sizes"].append(len(rows))
            rank = rank_of(rows, word)
            if rank is None:
                counts["absent"] += 1
                continue
            counts["present"] += 1
            counts["ranks"].append(rank)
            owner = _owner(rows[rank - 1])
            counts["above"] += rank - 1
            counts["same"] += sum(1 for row in rows[:rank - 1] if _owner(row) == owner)
            counts["worst"].append((rank, f"{corpus.name} {path}:{line + 1} `{word}`"))
        print(f"{corpus.name:10} {counts['typed']:4} answered of {len(posed)} {shape} "
              f"cursors ({why})")
        _print_rank(counts)
        for field in ("typed", "present", "absent", "above", "same"):
            totals[field] += counts[field]
        for field in ("sizes", "ranks", "worst"):
            totals[field].extend(counts[field])
        for step, row in counts["steps"].items():
            for field in ("asked", "present", "absent"):
                totals["steps"][step][field] += row[field]
            for field in ("sizes", "ranks"):
                totals["steps"][step][field].extend(row[field])
    if not totals["typed"]:
        return
    print()
    print(f"{'all':10} {totals['typed']:4} answered cursors")
    _print_rank(totals)
    if args.trace:
        print()
        for rank, where in sorted(totals["worst"], reverse=True)[:args.trace]:
            print(f"{'':10}   rank {rank:5}  {where}")


# How far into the word to follow a typed cursor. Three characters is where `cmd_prefix` found the
# other bound's population starts existing, so the two tables cover the same keystrokes.
RANK_PREFIX = 3
# The rows a reader of a typed list would plausibly reach, widening. The last is the ceiling in
# force when this was written, and the table is in fixed edges for `cmd_prefix`'s reason: a
# reporting constant that moved with the thing under test would compare nothing.
RANK_EDGES = (1, 10, 50, 128, 256, 512)


def _owner(item):
    """The class the `detail` line attributes this member to — `User#shout` -> `User`."""
    detail = item.get("detail")
    if not isinstance(detail, str):
        return ""
    for cut in ("#", "."):
        if cut in detail:
            return detail.split(cut, 1)[0]
    return detail


def _rank_counts():
    return {"typed": 0, "present": 0, "absent": 0, "above": 0, "same": 0,
            "sizes": [], "ranks": [], "worst": [],
            "steps": {step: {"asked": 0, "present": 0, "absent": 0, "sizes": [], "ranks": []}
                      for step in range(RANK_PREFIX + 1)}}


def _print_rank(counts):
    ranks = counts["ranks"]
    median, p90 = _middle(ranks)
    bands, low = [], 0
    for edge in RANK_EDGES:
        name = str(edge) if edge == low + 1 else f"{low + 1}-{edge}"
        bands.append(f"{name} {sum(1 for rank in ranks if low < rank <= edge)}")
        low = edge
    bands.append(f"{low + 1}+ {sum(1 for rank in ranks if rank > low)}")
    print(f"{'':10}   rank   " + "   ".join(bands))
    print(f"{'':10}   words  {counts['present']} present, {counts['absent']} absent "
          f"(or past the ceiling); median rank {median}, p90 {p90}")
    sizes = counts["sizes"]
    if sizes:
        size_median, size_p90 = _middle(sizes)
        seen = [f"<={edge} {sum(1 for size in sizes if size <= edge)}"
                for edge in (128, 256, 512, 1024, 2048)]
        seen.append(f">2048 {sum(1 for size in sizes if size > 2048)}")
        ordered = sorted(sizes)
        p99 = ordered[min(len(ordered) - 1, int(0.99 * len(ordered)))]
        print(f"{'':10}   sizes  " + "   ".join(seen) +
              f"   median {size_median}, p90 {size_p90}, p99 {p99}, max {ordered[-1]}")
    if counts["above"]:
        share = 100.0 * counts["same"] / counts["above"]
        print(f"{'':10}   owner  {counts['same']} of {counts['above']} rows above an answer are "
              f"its own owner's ({share:.0f}%)")
    # **What a lower display ceiling costs at each keystroke.** A word past the ceiling is not
    # sent, and `isIncomplete` is what brings it back: the row that matters is not `chars 0` but
    # the first one where something has been typed, because that is the request the client makes
    # the moment the ceiling bites.
    # `rank1` and `top10` are repeated per step rather than only for the whole run, because a
    # ranking key is not one ordering but one per prefix length: `tier` is constant while nothing
    # has been typed and does all the work the moment something has. A key judged on the `chars 0`
    # bands alone is judged on the request the user spends the least time looking at.
    print(f"{'':10}   {'chars':>5} {'asked':>6} {'present':>8} {'rank1':>6} {'top10':>6} "
          f"{'median':>7} {'p90':>6} {'>128':>6} {'>256':>6}")
    for step, row in sorted(counts["steps"].items()):
        if not row["asked"]:
            continue
        step_median, step_p90 = _middle(row["ranks"])
        first = sum(1 for rank in row["ranks"] if rank == 1)
        top10 = sum(1 for rank in row["ranks"] if rank <= 10)
        over = sum(1 for rank in row["ranks"] if rank > 128)
        over256 = sum(1 for rank in row["ranks"] if rank > 256)
        print(f"{'':10}   {step:5} {row['asked']:6} {row['present']:8} {first:6} {top10:6} "
              f"{step_median:7} {step_p90:6} {over:6} {over256:6}")


def measure(corpus, args, held):
    """One corpus, end to end. Returns `(drawn, answers, counts, findings, residue)`.

    The one pipeline both `score` and `adjudicate` run, because two copies of an ordering this
    particular — lane 1's asking keys before `ask_rebased`, lane 3 after everything — is two
    copies that drift. The server is stopped before this returns.
    """
    drawn = positions(corpus, args.seed, args.per_file)
    if getattr(args, "n", 0):
        drawn = drawn[:args.n]
    if not drawn:
        return [], {}, {}, [], []
    client, _, why = start(args.server, corpus)
    if why != "quiet":
        print(f"{corpus.name:10} WARNING  did not settle: {why}")
    # One set of open documents for the whole server, because a second `didOpen` for a document
    # the client already holds is not a legal message and lane 1's Rails key reaches the same
    # model files the sample does.
    opened = set()
    answers = ask_all(client, corpus, drawn, methods=lane2.METHODS, opened=opened)
    # **Before the rebase pass, and that is an ordering constraint rather than a preference.**
    # Lane 1's asking keys draw their own cursors out of the corpus text; `ask_rebased` then
    # inserts a line at the top of every sampled document, and a model file the sample also
    # reached would answer one line off for all of them.
    keys, graded = ({}, [])
    if not args.no_key:
        keys, graded = lane1.asked(corpus, client, args.seed, opened, drawn, answers)
    # **Before the rebase pass and after lane 1, for the rebase pass's own reason**: it asks
    # about places in whatever documents the answers named, and `ask_rebased` then inserts a line
    # at the top of every sampled document — a place in one of those would be asked one line off.
    described, skipped = places.offered(answers, drawn, args.places_cap)
    place_counts = places.counters(places.ask(client, described), skipped)
    rebased, shifted = (None, None)
    if not args.eager_only:
        rebased, shifted = ask_rebased(client, corpus, drawn, answers)
    client.stop()
    counts, findings = lane2.run(corpus, drawn, answers, rebased, shifted, place_counts)
    findings += graded
    # The rest of lane 1 reads the replies lane 2 already collected, so it runs after the server
    # is stopped — which is the property worth keeping rather than an accident of ordering: a key
    # that reads a transcript can be re-run against one.
    if not args.no_key:
        read, graded = lane1.graded(corpus, drawn, answers)
        counts["keys"] = dict(keys, **read)
        findings += graded
    # Lane 3 last, because "a position no rule decides" is defined by what the other two lanes
    # just did: it subtracts every position a key graded and every position a check raised a
    # finding at, and what is left is the residue it reports the size of.
    counts, findings, residue = lane3.run(corpus, drawn, answers, counts, findings, held)
    counts["warnings"] = client.warnings
    return drawn, answers, counts, findings, residue


def queues(corpora, jobs):
    """`corpora` split into `jobs` queues, each of which one thread sweeps serially.

    Longest-processing-time first: the heaviest corpus starts first and every next one joins the
    queue with the least work in it. That is the standard schedule for this shape and it needs no
    tuning — what it needs is a `QUEUE_WEIGHT` that is a duration rather than a size proxy, which
    is why `cmd_score` prints every queue's real seconds.

    Parallel by **corpus** and never inside one: a corpus is one server answering one pipeline in
    order, and two threads asking one server would interleave two draws through one graph.
    """
    lanes = [[] for _ in range(max(1, min(jobs, len(corpora))))]
    load = [0] * len(lanes)
    for corpus in sorted(corpora, key=lambda c: -QUEUE_WEIGHT.get(c.name, 0)):
        at = load.index(min(load))
        lanes[at].append(corpus)
        load[at] += QUEUE_WEIGHT.get(corpus.name, 0)
    return [lane for lane in lanes if lane]


def sweep(corpus, args, held):
    """One corpus, from the clean check to its counters. Returns what `cmd_score` prints.

    Every server the audit starts is its own process with its own graph, so two of these at once
    share nothing but the log — `O_APPEND` with the pid on every line, the same two properties two
    editor windows on one project rely on — and `held`, which lane 3 only reads.
    """
    drift = check_clean(corpus)
    if drift:
        return {"skipped": drift}
    began = time.time()
    drawn, answers, counts, findings, residue = measure(corpus, args, held)
    elapsed = time.time() - began
    if not drawn:
        return {"skipped": "no positions drawn", "elapsed": elapsed}
    return {"counts": counts, "findings": findings, "residue": residue, "elapsed": elapsed}


def _sweep_queue(lane, args, held, done, seconds, name):
    """One queue's corpora, in order. All a worker prints is that one of them landed."""
    began = time.time()
    for corpus in lane:
        done[corpus.name] = sweep(corpus, args, held)
        spent = done[corpus.name].get("elapsed")
        print(f"{corpus.name:10} {name} done" + (f" in {spent:.1f}s" if spent else " (skipped)"))
    seconds[name] = time.time() - began



def cmd_score(args):
    """Ask, then run both lanes over the answers. Prints and writes nothing to disk.

    The identifier under the cursor and the path it landed in do reach the terminal, because a
    finding nobody can go and look at is not a finding — what the licence rule governs is what
    gets **committed**, and that is the ledger, which carries the sha256 of a line and never the
    line.

    `--record` is the exception to "writes nothing": it saves this run in the shape `audit report`
    diffs, and that file holds no word either.
    """
    server = args.server
    if not os.path.exists(server):
        sys.exit(f"no server at {server}; `make release` first")
    summed, keyed, pending = {}, {}, {}
    record = {"version": baseline.VERSION}
    held = ledger.load(Path(args.ledger))
    # One run, one log. The server never truncates, for a reason that is about editors rather
    # than about sweeps; here there is nothing to keep and ~25 MB a run to lose.
    log = fresh_log()
    if log:
        print(f"logging every request to {log}")
    clock = time.time()
    corpora = sweepable(args.only)
    jobs = max(1, getattr(args, "jobs", 1))
    done, seconds = {}, {}
    if jobs == 1:
        for corpus in corpora:
            # Said before the work and not after it: `sweep` walks the corpus, starts a server,
            # waits out its cold index and then asks every position, which on the largest of the
            # six is a little over a minute of one process printing nothing. The line is replaced
            # by nothing — the result block follows it — because a sweep is read as it runs and a
            # log is read afterwards, and a name in flight is what the first reader needs.
            print(f"{corpus.name:10} sweeping...")
            done[corpus.name] = sweep(corpus, args, held)
    else:
        # **Parallel by corpus, and the reports are still printed in the table's order.** A worker
        # prints one line when a corpus lands, because interleaved result blocks from six servers
        # are unreadable; everything else waits for the join, so `--jobs` changes the order the
        # work happens in and not one character of what is reported.
        lanes = queues(corpora, jobs)
        for at, lane in enumerate(lanes, 1):
            print(f"queue {at:<5} {', '.join(c.name for c in lane)}")
        workers = [threading.Thread(target=_sweep_queue,
                                    args=(lane, args, held, done, seconds, f"q{at}"))
                   for at, lane in enumerate(lanes, 1)]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join()
    for corpus in corpora:
        result = done.get(corpus.name) or {}
        if result.get("skipped"):
            print(f"{corpus.name:10} SKIPPED  {result['skipped']}")
            continue
        counts, findings = result["counts"], result["findings"]
        pending[corpus.name] = ledger.pending(corpus, result["residue"])
        record[corpus.name] = baseline.of(corpus, args, counts, findings)
        report.report(corpus, counts, findings, result["elapsed"], args.show)
        for warning in counts.get("warnings") or ():
            print(f"{'':10}   warning: {warning}")
        for name, value in counts.items():
            if isinstance(value, int):
                summed[name] = summed.get(name, 0) + value
        # A counter kept as a dict of counts rather than as one number is skipped by the loop
        # above, so anything the totals line prints has to be merged by name here. Lane 3's two
        # breakdowns, and the first-place measurement beside `tiers`.
        for name in ("verdicts", "residue-shapes", "first-place", "def-places"):
            running = summed.setdefault(name, {})
            for what, value in counts.get(name, {}).items():
                running[what] = running.get(what, 0) + value
        for name, cell in (counts.get("keys") or {}).items():
            running = keyed.setdefault(name, {})
            for verdict, value in cell.items():
                if isinstance(value, int):
                    running[verdict] = running.get(verdict, 0) + value
    if not summed:
        return
    report.totals(summed, keyed, summed["positions"], time.time() - clock, BUDGET_SECONDS)
    # **Two numbers once the queues run at once, because the budget is a serial one.**
    # `BUDGET_SECONDS` was derived from what one position costs one server, so the wall clock in
    # the line above stops being comparable with it the moment two servers answer together. The
    # summed figure is the one `audit cost` sized the draw against; the per-queue seconds are what
    # to rebalance `QUEUE_WEIGHT` from.
    if seconds:
        summed_seconds = sum(r.get("elapsed", 0.0) for r in done.values())
        print(f"{'':10} {len(seconds)} queues   "
              + "   ".join(f"{name} {spent:.1f}s" for name, spent in sorted(seconds.items()))
              + f"   summed {summed_seconds:.1f}s")
    if getattr(args, "record", None):
        # The one thing `score` may write, and only when asked. It holds integers, a git SHA and
        # each finding's `audit.site` — no word and no `detail` — because `audit report` diffs it
        # against a file that is committed. See `baseline`.
        where = baseline.save(record, Path(args.record))
        print(f"{'':10} recorded {len(record) - 1} corpora to {where}")
    if args.pending:
        # Written only when asked. The residue is thousands of rows nobody has adjudicated, and
        # a run that merged them into the ledger unasked would commit them and call them one.
        where = ledger.save(dict(ledger.load(), **pending), Path(args.pending))
        rows = sum(len(c["positions"]) for c in pending.values())
        print(f"{'':10} wrote {rows} unadjudicated rows to {where}")


def cmd_ledger(args):
    """What is in `audit/ledger.json`, and whether it still applies.

    Reads the corpora but starts no server and asks nothing: every question here is about the
    ledger against the source, which is the same pair of facts lane 3 uses at scoring time.
    """
    held = ledger.load(Path(args.path))
    total = 0
    for corpus in sweepable(args.only):
        rows = ledger.rows(held, corpus)
        if not rows:
            print(f"{corpus.name:10} no rows")
            continue
        drift = check_clean(corpus)
        if drift:
            print(f"{corpus.name:10} {len(rows):5} rows   NOT CHECKED  {drift}")
            continue
        at = (held.get(corpus.name) or {}).get("sha")
        live, stale, unfilled, gone = 0, 0, 0, 0
        source = {}
        for where, row in rows.items():
            path, offset = where.rsplit(":", 1)
            if path not in source:
                try:
                    source[path] = (corpus.dir / path).read_text(encoding="utf-8",
                                                                 errors="replace")
                except OSError:
                    source[path] = ""
            text = source[path]
            if not text or int(offset) >= len(text):
                gone += 1
            elif line_key(text, int(offset)) != row.get("hash"):
                stale += 1
            else:
                live += 1
                if row.get("verdict") not in ledger.VERDICTS:
                    unfilled += 1
        total += len(rows)
        pinned = "" if at == corpus.sha else (f"  taken at {(at or '?')[:12]}, "
                                              f"now {corpus.sha[:12]}")
        print(f"{corpus.name:10} {len(rows):5} rows   {live} live, {stale} stale, "
              f"{gone} off the end of the file{pinned}")
        if unfilled:
            print(f"{'':10}   {unfilled} live rows carry no verdict yet")
    if total:
        print()
        print(f"{'all':10} {total:5} rows in {args.path}")


# ------------------------------------------------------------------------------- adjudicate
#
# Lane 3's residue is 3,826 positions and a person cannot judge one of them from a path and a
# byte offset. What a verdict actually needs is the three things a reviewer would go and look
# up: the line the cursor is on, what the cursor is on in it, and what ya-lsp said there. This
# prints exactly those, in batches small enough to finish, and writes the batch back as rows
# with the verdict left empty.
#
# **The identifier and the source line reach the terminal and never the file.** That is the same
# seam every other finding in this package sits on: the licence rule governs what gets committed,
# and `ledger.save` keeps only its own field list — so no amount of reviewing here can put a
# corpus line into `audit/ledger.json`.

CARD_SKIP = ("```", "---", "")


def card_lines(card, keep=2):
    """The first `keep` lines of a hover card that say something. Fences and rules are not."""
    if not card:
        return []
    out = []
    for line in card.split("\n"):
        if line.strip() in CARD_SKIP or line.startswith("```"):
            continue
        out.append(line.strip())
        if len(out) >= keep:
            break
    return out


def _snippet(lines, at, context, column=None, word=None, indent="    "):
    """`context` lines either side of `at`, with the cursor marked when there is one."""
    for row in range(max(0, at - context), min(len(lines), at + context + 1)):
        mark = ">" if row == at else " "
        print(f"{indent}{mark} {row + 1:5} | {lines[row].rstrip()[:104]}")
        if row == at and column is not None:
            print(f"{indent}  {'':5} | {' ' * column}^ {word or ''}")


def _shorten(corpus, target):
    """A target path as something short enough to read: relative here, gem-and-file elsewhere."""
    path = path_of(target)
    if path is None:
        return target, None
    try:
        return str(Path(path).resolve().relative_to(Path(corpus.dir).resolve())), path
    except ValueError:
        return ".../" + "/".join(path.rsplit("/", 3)[-3:]), path


def present(corpus, drawn, answers, batch, context=1, targets=2):
    """Print one batch of residue positions for review, numbered.

    **The target's own source is printed too**, because the question a verdict answers is "did it
    land on the right thing" and a path with a line number does not answer it — a reviewer would
    open the file, so this opens it. Reading is unrestricted for every corpus and for every gem;
    it is copying *out* that the licence rule forbids, and nothing here reaches the ledger.
    """
    source = {}

    def lines_of(where):
        if where not in source:
            try:
                source[where] = Path(where).read_text(encoding="utf-8",
                                                      errors="replace").split("\n")
            except OSError:
                source[where] = []
        return source[where]

    for number, (index, path, line, _, shape, _) in enumerate(batch, 1):
        _, _, _, _, column, _, word = drawn[index]
        card = card_of(answers.get((index, "textDocument/hover")))
        found = locations(answers.get((index, "textDocument/definition")))
        # The ledger key, printed because the batch file is **sorted** and the number above is
        # therefore not an index into it. Recording a verdict by position-in-batch put one on
        # the wrong row before this line existed.
        print(f"[{number}] {shape} `{word}`   {path}:{line + 1}"
              f"   [{ledger.key(path, drawn[index][5])}]")
        print()
        _snippet(lines_of(str(corpus.dir / path)), line, context, column, word)
        print()
        rung = tier(card) or "no card"
        said = card_lines(card, keep=3)
        print(f"    hover       {rung}" + (f"   {said[0]}" if said else ""))
        for extra in said[1:]:
            print(f"    {'':11} {extra}")
        if not found:
            print(f"    definition  (nothing)")
            print()
            continue
        print(f"    definition  {len(found)} target" + ("s" if len(found) != 1 else ""))
        for target, span in found[:targets]:
            short, full = _shorten(corpus, target)
            at = span["start"]["line"]
            print(f"      -> {short}:{at + 1}")
            if full:
                _snippet(lines_of(full), at, context, indent="        ")
        if len(found) > targets:
            rest = ", ".join(_shorten(corpus, t)[0] for t, _ in found[targets:targets + 4])
            more = f" (+{len(found) - targets - 4} more)" if len(found) - targets > 4 else ""
            print(f"      -> and {len(found) - targets}: {rest}{more}")
        print()


def cmd_adjudicate(args):
    """Present a batch of lane 3's residue for a person to judge, and write the blank rows.

    Runs the whole pipeline, because residue is *defined* by what the other lanes decided — a
    batch drawn without them would offer positions a key already graded.
    """
    if not os.path.exists(args.server):
        sys.exit(f"no server at {args.server}; `make release` first")
    held = ledger.load(Path(args.ledger))
    out = {}
    for corpus in sweepable(args.only):
        drift = check_clean(corpus)
        if drift:
            print(f"{corpus.name:10} SKIPPED  {drift}")
            continue
        _, answers, counts, _, residue = measure(corpus, args, held)
        drawn = positions(corpus, args.seed, args.per_file)
        rows = [row for row in residue if not args.shape or row[4] == args.shape]
        batch = rows[args.start:args.start + args.n_batch]
        print(f"{corpus.name}  {len(batch)} of {len(rows)} residue positions"
              + (f" of shape `{args.shape}`" if args.shape else "")
              + f", starting at {args.start}\n")
        if not batch:
            continue
        present(corpus, drawn, answers, batch, args.context)
        out[corpus.name] = ledger.pending(corpus, batch)
    if out and args.out:
        where = ledger.save(dict(held, **out), Path(args.out))
        rows = sum(len(c["positions"]) for c in out.values())
        print(f"{rows} blank rows written to {where} — fill `verdict` "
              f"({', '.join(ledger.VERDICTS)}) and `why`, then re-run `score`")


def cmd_report(args):
    """This run against the last committed one. Reads two files, starts no server, opens no corpus.

    **A report is a comparison of two recordings, and that is worth more than saving a sweep.** It
    means the diff can be re-read, re-cut by corpus and re-run in CI from an artifact, months after
    the machine that produced it went away — the same property lane 1's `graded` keys have, for the
    same reason. `audit score --record PATH` produces the recording.

    `--save` writes the run in as the new baseline, and it **merges** rather than replaces: a run
    over one corpus must not silently drop the other four's numbers from a committed file. Which
    corpora were carried over unchanged is printed, because a carried-over row is a number nobody
    measured today and a reader has to be told which ones those are.
    """
    now = baseline.load(Path(args.record))
    if len(now) <= 1:
        sys.exit(f"no run recorded at {args.record}; "
                 f"`audit score --record {args.record}` first")
    before = baseline.load(Path(args.baseline))
    if len(before) <= 1:
        print(f"{'':10} no baseline at {args.baseline} — this run is the first one")
    wanted = set(args.only or ())
    new, gone, counters, compared = 0, 0, 0, 0
    for name in sorted(set(now) | set(before)):
        if name == "version" or (wanted and name not in wanted):
            continue
        if name not in now:
            print(f"{name:10} NOT MEASURED  in the baseline, not in this run")
            continue
        against = report.moved(name, before.get(name), now[name], args.show)
        if against is None:
            continue
        new, gone, counters = (new + against[0], gone + against[1], counters + against[2])
        compared += 1
    print()
    print(f"{'compared':10} {compared} corpora, {new} findings new, {gone} gone, "
          f"{counters} counters moved")
    if not args.save:
        return
    merged = dict(before, **{k: v for k, v in now.items() if k != "version"})
    carried = sorted(set(merged) - set(now) - {"version"})
    where = baseline.save(merged, Path(args.baseline))
    print(f"{'':10} saved {len(merged) - 1} corpora to {where}"
          + (f"; {', '.join(carried)} carried over unmeasured" if carried else ""))
