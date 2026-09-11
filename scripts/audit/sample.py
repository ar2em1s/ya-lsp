"""The seeded, twice-stratified draw: which files, and which positions in them."""

import os
import random

from audit.routes import non_helpers
from audit.shapes import SHARES, find

# ------------------------------------------------------------------------------------ strata
#
# Which *part of the repository* a position comes from. Grounded in a file count over the six
# corpora rather than in a guess at what a Rails application contains, and a stratum is a **list**
# of the spellings one concept takes: a background job is `app/jobs` in three corpora and
# `app/workers` in two, so either name alone is empty half the time.

STRATA = (
    # name            directories                                                       files
    ("models",        ("app/models",),                                                      20),
    ("controllers",   ("app/controllers",),                                                 16),
    ("views",         ("app/views",),                                                       14),
    ("services",      ("app/services", "app/lib", "app/builders", "app/operations"),        12),
    # 20 rather than 12: the count matched `services` when the tree does not — `candidate_files`
    # reaches 2,081 files under `lib/` in discourse, 433 in solidus, 162 in forem, 127 in chatwoot
    # and 75 in mastodon — and it is the **only non-Rails Ruby in the draw**, so it is what keeps
    # the sample from being a statement about Rails alone. Only lobsters falls short, with 3 files,
    # and it fell short at 12 too; a stratum that cannot fill shrinks the draw rather than
    # borrowing, which is `OVERDRAW` doing its job.
    ("lib",           ("lib",),                                                             20),
    ("jobs",          ("app/jobs", "app/workers", "app/sidekiq"),                            8),
    ("poro",          ("app/policies", "app/presenters", "app/validators", "app/queries",
                       "app/forms", "app/decorators", "app/channels", "app/components",
                       "app/finders"),                                                       8),
    ("serializers",   ("app/serializers", "app/views/api"),                                  6),
    ("helpers",       ("app/helpers",),                                                      6),
    ("mailers",       ("app/mailers",),                                                      4),
    # **The test tree, and it is a stratum rather than a widened glob.** Five of the six corpora
    # keep more Ruby under `spec/` than anywhere else. Counted through `candidate_files` itself,
    # which is the only count that means anything here — a root-level `find` undercounts solidus by
    # a factor of five, because its models are `core/app/models/` and the matcher is a suffix test:
    # **specs** 128 / 913 / 1,238 / 958 / 1,416 / 4,430 against **models** 48 / 375 / 248 / 179 /
    # 167 / 547, in the order the pin table lists the corpora. So a draw that skipped the tree whole
    # was leaving out the largest thing in every one of them. It is also the only stratum whose
    # positions `environment.rs` fences: a cursor *inside* `spec/` reads the wider cursor list and
    # is entitled to answers a cursor in `app/` may not be given, and nothing in the draw used to
    # exercise that. Lane 2's check 2 is the one that cares most — a **Resolved** card landing in a
    # test tree is its whole subject, and until now no drawn cursor was in one.
    #
    # 20 and not a share of 2,254, because the point is to reach the shape rather than to weight
    # the draw by how many specs a project happens to write.
    ("specs",         ("spec", "test", "features"),                                        20),
)
ERB_STRATUM = "views"
# Which stratum the test tree is, and which directory names make one. **A directory under one of
# these may be claimed by that stratum and by no other, and every other stratum may claim only
# directories outside them.** Both halves are needed, for the reason the note on `SKIP` gives:
# mastodon keeps `spec/lib/`, so a glob that merely admitted the tree would file somebody's specs
# under the `lib` stratum, and a stratum meaning "library code" in three corpora and "library code
# and its specs" in two is one whose counts cannot be compared across the rows they print in.
SPEC_STRATUM = "specs"
TEST_ROOTS = ("spec", "test", "features")
# How far a shape may exceed its `SHARES` target when a thinner shape falls short. **1.0, so it
# may not**, and the two numbers that settle it were measured rather than argued: unbounded puts
# `member` at 57% against its stated 45%, 1.25 puts it at exactly 56% (which is 1.25 x 45, the
# bound doing its job and the job being the wrong one), and 1.0 lands the whole mix within a
# point of what `SHARES` says. Slack here buys only sample *size*, and size is what `--per-file`
# is for — so paying for it in mix drift is paying in the wrong currency. See `positions`.
OVERDRAW = 1.0
# `/spec/` and `/test/` are skipped whole rather than only their fixtures, and the reason is
# stratification rather than taste: mastodon keeps `spec/lib/`, so a glob that admits it files
# somebody's specs under the **`lib` stratum** — and a stratum that means "library code" in three
# corpora and "library code plus its specs" in two is a stratum whose counts are not comparable
# across the rows they are printed in. A spec file is a real thing to have a cursor in and it
# deserves a stratum of its own; adding one is a change to the sample design, which is a decision
# to take deliberately and not to inherit from a directory glob.
# **Unchanged, and deliberately still naming the test roots.** `audit.lane1.closures` imports this
# tuple to decide which files its own scan reads, and an RSpec file is wall-to-wall `describe … do`
# at two-space indentation — exactly what its `OPENS` pattern matches — so widening this would
# flood a different key with positions it was never designed for. The draw's own exclusion is
# `NOT_SAMPLED` below, which is this list minus the test roots. The two are different questions and
# were one tuple only because nothing sampled a spec.
SKIP = ("/.git/", "/node_modules/", "/vendor/", "/spec/", "/test/", "/features/",
        "/tmp/", "/log/", "/public/")
# What no stratum may reach. `SKIP` without the test roots, because `SPEC_STRATUM` reaches those.
NOT_SAMPLED = tuple(skip for skip in SKIP if skip.strip("/") not in TEST_ROOTS)


def candidate_files(corpus):
    """Every file the strata reach, as {stratum: [path relative to the corpus]}.

    A stratum **matches a suffix** rather than joining onto the corpus root, and that is not
    tidiness: solidus is an engine monorepo whose models are `core/app/models/`, so a rule that
    joins measures a different part of that repository than of the other five.
    """
    found = {name: [] for name, _, _ in STRATA}
    for here, dirs, names in os.walk(corpus.dir):
        dirs[:] = [d for d in dirs if not d.startswith(".")]
        marked = "/" + os.path.relpath(here, corpus.dir).replace(os.sep, "/").strip("./") + "/"
        if any(skip in marked for skip in NOT_SAMPLED):
            continue
        # Which side of the test boundary this directory is on, asked once rather than per stratum.
        in_test = any(marked.rstrip("/").endswith("/" + root) or ("/" + root + "/") in marked
                      for root in TEST_ROOTS)
        for name, directories, _ in STRATA:
            # **The exclusivity rule, and it runs before the suffix match rather than after.** The
            # test stratum takes the test tree and nothing else; every other stratum takes
            # everything else and none of it. Without this line `spec/models/` is a model file and
            # `spec/lib/` is library code in the two corpora that keep one.
            if (name == SPEC_STRATUM) != in_test:
                continue
            if not any(marked.rstrip("/").endswith("/" + d) or ("/" + d + "/") in marked
                       for d in directories):
                continue
            for leaf in names:
                if leaf.endswith(".rb") or leaf.endswith(".erb"):
                    found[name].append(os.path.relpath(os.path.join(here, leaf), corpus.dir))
            break
    return found


def draw_files(corpus, rng):
    """The files to ask about, as [(stratum, relative path)], seeded and reproducible."""
    pool = candidate_files(corpus)
    picked = []
    for name, _, want in STRATA:
        rows = sorted(pool.get(name) or [])
        if not rows:
            continue                       # lobsters has no serializer; a short stratum is fine
        rng.shuffle(rows)
        picked.extend((name, path) for path in rows[:want])
    return picked


def positions(corpus, seed, per_file):
    """The seeded, twice-stratified draw for one corpus.

    Returns [(stratum, shape, path, line, column, offset, word)]. Deliberately biased toward the
    residue: the shape shares put 55% of the draw outside `member`, which is where the existing
    keys apply and therefore where the unknown is not.

    **The shape targets are corpus-wide rather than per file**, and the first draw written the
    other way proved why twice. A 5% share of a twelve-position file rounds to **zero**, so
    `route` — the shape the Rails half of an answer lives in — is sampled not at all in most
    files; and a shortfall left where it falls is a shortfall handed to `member` every time,
    because `member` is the one shape no file is ever short of. Measured on the per-file draw:
    `member` came out at 54% against its stated 45%, which is the sample drifting back toward
    exactly the positions an existing key already covers.
    """
    rng = random.Random(f"{seed}:{corpus.name}:{corpus.sha}")
    not_routes = non_helpers(corpus)
    per_file_found = []
    for stratum, path in draw_files(corpus, rng):
        try:
            text = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        per_file_found.append((stratum, path,
                               find(text, path.endswith(".erb"), not_routes)))
    if not per_file_found:
        return []

    total = per_file * len(per_file_found)
    want = {name: int(round(total * share)) for name, share in SHARES}
    # Per file, per shape, spread evenly through the file, and never more than twice a file's
    # own fair share of one shape — otherwise a 2,000-line model is the whole `constant` column.
    pools = {}
    for name, share in SHARES:
        cap = max(2, int(2 * per_file * share))
        rows = []
        for stratum, path, found in per_file_found:
            candidates = found.get(name) or []
            keep = min(len(candidates), cap)
            step = max(1, len(candidates) // keep) if keep else 1
            rows.append([(stratum, name, path) + c for c in candidates[::step][:keep]])
        pools[name] = rows

    def spend(name, budget, picked):
        """Take up to `budget` of one shape, across files first so one file cannot supply all."""
        rows, index, guard = pools[name], 0, 0
        while len(picked[name]) < budget and any(rows):
            row = rows[index % len(rows)]
            if row:
                picked[name].append(row.pop(0))
            index += 1
            guard += 1
            if guard > len(rows) * 400:
                break

    picked = {name: [] for name, _ in SHARES}
    for name, _ in SHARES:
        spend(name, want[name], picked)
    # Whatever a thin shape could not supply is spent round-robin over the shapes that still
    # have candidates — but **no shape may exceed `OVERDRAW` times its own target**, and that
    # bound is the point rather than a detail. Without it the shortfall goes wherever there are
    # candidates left, which is `member` every time, and an unbounded top-up measured 57%
    # `member` against a stated 45%: the sample re-confirming the positions an existing key
    # already covers, which is the one thing this draw exists not to do. A corpus that cannot
    # fill its target under the bound **stays short**, because a corpus does not contain more
    # route helpers than it contains.
    ceilings = {name: int(want[name] * OVERDRAW) for name, _ in SHARES}
    short = total - sum(len(v) for v in picked.values())
    while short > 0:
        before = short
        for name, _ in SHARES:
            if short <= 0:
                break
            if any(pools[name]) and len(picked[name]) < ceilings[name]:
                spend(name, len(picked[name]) + 1, picked)
                short = total - sum(len(v) for v in picked.values())
        if short == before:
            break                          # every pool is empty or capped; short is honest
    # `total` counts every file the draw opened, including those that turned out to hold no
    # candidate of any shape, so a ceiling computed against it is looser than it reads and the
    # bound above did not bind at all — `member` stayed at 57%. Trim against the total actually
    # drawn instead, to a fixed point: trimming lowers the total, which lowers every ceiling,
    # and a few passes converge. This is the pass that makes the mix the stated one.
    for _ in range(8):
        drew = sum(len(v) for v in picked.values())
        over = False
        for name, share in SHARES:
            ceiling = max(1, int(drew * share * OVERDRAW))
            if len(picked[name]) > ceiling:
                picked[name] = picked[name][:ceiling]
                over = True
        if not over:
            break
    out = []
    for name, _ in SHARES:
        out.extend(picked[name])
    # Shuffled, deterministically, and only the *order* changes. The pools are filled one shape
    # at a time, so the draw comes out grouped — and any prefix of it is then a single shape.
    # `score -n` takes a prefix, so without this line the cheap run everyone actually does is
    # 100% `member`: the one shape the existing keys already cover.
    rng.shuffle(out)
    return out
