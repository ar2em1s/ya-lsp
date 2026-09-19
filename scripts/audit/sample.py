"""The seeded, twice-stratified draw: which files, and which positions in them."""

import os
import random

from audit.routes import non_helpers
from audit.shapes import SHARES, declared, find

# ------------------------------------------------------------------------------------ strata
#
# Which *part of the repository* a position comes from.
#
# - **Grounded in a file count over the six corpora**, not in a guess at what a Rails app contains.
# - **A stratum is a list of the spellings one concept takes.** A background job is `app/jobs` in
#   some corpora and `app/workers` in others, so either name alone is often empty.

STRATA = (
    # name            directories                                                       files
    ("models",        ("app/models",),                                                      20),
    ("controllers",   ("app/controllers",),                                                 16),
    ("views",         ("app/views",),                                                       14),
    ("services",      ("app/services", "app/lib", "app/builders", "app/operations"),        12),
    # 20, not 12 like `services`: `lib/` is large in most corpora, and it is the **only non-Rails
    # Ruby in the draw**, so it keeps the sample from being about Rails alone. A corpus with a small
    # `lib/` shrinks the draw instead of borrowing; that is `OVERDRAW` doing its job.
    ("lib",           ("lib",),                                                             20),
    ("jobs",          ("app/jobs", "app/workers", "app/sidekiq"),                            8),
    ("poro",          ("app/policies", "app/presenters", "app/validators", "app/queries",
                       "app/forms", "app/decorators", "app/channels", "app/components",
                       "app/finders"),                                                       8),
    ("serializers",   ("app/serializers", "app/views/api"),                                  6),
    ("helpers",       ("app/helpers",),                                                      6),
    ("mailers",       ("app/mailers",),                                                      4),
    # **The test tree, as a stratum of its own, not a widened glob.**
    # - Most corpora keep more Ruby under `spec/` than anywhere else. (Count through
    #   `candidate_files`: a root-level `find` misses solidus' `core/app/models/`, because the
    #   matcher is a suffix test.)
    # - It is the only stratum `environment.rs` fences: a cursor *inside* `spec/` reads the wider
    #   cursor list and may get answers a cursor in `app/` may not.
    # - Lane 2's check 2 cares most: a **Resolved** card landing in a test tree is its whole
    #   subject.
    #
    # 20, not a share of the tree: the point is to reach the shape, not to weight the draw by how
    # many specs a project writes.
    ("specs",         ("spec", "test", "features"),                                        20),
)
ERB_STRATUM = "views"
# Which stratum the test tree is, and which directory names make one.
#
# **Exclusive both ways:** a directory under a test root belongs to this stratum only, and every
# other stratum takes only directories outside the test roots. Both halves are needed. mastodon
# keeps `spec/lib/`, so a glob that merely admitted the tree would file specs under `lib`, and a
# `lib` count meaning "library code" in some corpora and "library code plus specs" in others cannot
# be compared across rows.
SPEC_STRATUM = "specs"
TEST_ROOTS = ("spec", "test", "features")
# How far a shape may exceed its `SHARES` target when a thinner shape falls short. **1.0: it may
# not.**
#
# Measured, not argued:
# - unbounded puts `member` at 57% against a stated 45%;
# - 1.25 puts it at 56%: the bound working, on the wrong job;
# - 1.0 lands the whole mix within a point of `SHARES`.
# Slack here buys only sample *size*, and size is what `--per-file` is for. See `positions`.
OVERDRAW = 1.0
# The directories `lane1.closures`' own scan skips, test roots included.
#
# **Keep the test roots here.** An RSpec file is wall-to-wall `describe … do` at two-space
# indentation, exactly what closures' `OPENS` matches, so dropping them would flood that key with
# positions it was never designed for. The draw skips `NOT_SAMPLED` instead.
SKIP = ("/.git/", "/node_modules/", "/vendor/", "/spec/", "/test/", "/features/",
        "/tmp/", "/log/", "/public/")
# What no stratum may reach: `SKIP` minus the test roots, because `SPEC_STRATUM` reaches those.
NOT_SAMPLED = tuple(skip for skip in SKIP if skip.strip("/") not in TEST_ROOTS)


def candidate_files(corpus):
    """Every file the strata reach, as {stratum: [path relative to the corpus]}.

    A stratum **matches a suffix**, not a path joined onto the corpus root. solidus is an engine
    monorepo whose models are `core/app/models/`, so joining would measure a different part of it
    than of the other corpora.
    """
    found = {name: [] for name, _, _ in STRATA}
    for here, dirs, names in os.walk(corpus.dir):
        dirs[:] = [d for d in dirs if not d.startswith(".")]
        marked = "/" + os.path.relpath(here, corpus.dir).replace(os.sep, "/").strip("./") + "/"
        if any(skip in marked for skip in NOT_SAMPLED):
            continue
        # Which side of the test boundary this directory is on, asked once, not per stratum.
        in_test = any(marked.rstrip("/").endswith("/" + root) or ("/" + root + "/") in marked
                      for root in TEST_ROOTS)
        for name, directories, _ in STRATA:
            # **The exclusivity rule, checked before the suffix match.** The test stratum takes the
            # test tree and nothing else; every other stratum takes everything else. Without it,
            # `spec/models/` would be a model file and `spec/lib/` library code.
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

    Returns [(stratum, shape, path, line, column, offset, word)].

    **Biased toward the residue on purpose.** The shape shares put 55% of the draw outside `member`,
    which is where the existing keys apply and so where the unknown is not.

    **The shape targets are corpus-wide, not per file.** A per-file draw fails twice:
    - a 5% share of a twelve-position file rounds to **zero**, so `route` (where the Rails half of
      an answer lives) goes unsampled in most files;
    - a shortfall left where it falls goes to `member` every time, because no file is short of
      `member`. The per-file draw put `member` at 54% against a stated 45%.
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
    # Per file, per shape, spread evenly through the file, and never more than twice a file's fair
    # share of one shape. Otherwise one huge model is the whole `constant` column.
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
        """Take up to `budget` of one shape, across files first, so one file cannot supply it all.
        """
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
    # A thin shape's shortfall is spent round-robin over the shapes that still have candidates, but
    # **no shape may exceed `OVERDRAW` times its target.**
    # - Without the bound the shortfall goes to `member` every time: 57% against a stated 45%,
    #   re-confirming positions a key already covers.
    # - A corpus that cannot fill its target under the bound **stays short**. A corpus holds only so
    #   many route helpers.
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
    # Trim against the total actually drawn, to a fixed point.
    #
    # `total` counts every file the draw opened, including ones with no candidate, so a ceiling
    # computed from it is looser than it reads: the bound above did not bind, and `member` stayed at
    # 57%. Trimming lowers the total, which lowers every ceiling, and a few passes converge. This
    # pass is what makes the mix the stated one.
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
    # Shuffled deterministically; only the *order* changes. The pools fill one shape at a time, so
    # unshuffled, any prefix is a single shape. `score -n` takes a prefix, so the cheap run everyone
    # does would be 100% `member`.
    rng.shuffle(out)
    return out


# How many declaration cursors each sampled file gives. **Two**, spread through the file, so a long
# model gives its first `def` and one from the middle, not its first two.
DECLARATIONS_PER_FILE = 2


def declarations(corpus, seed, per_file, drawn=None):
    """The declaration-site stratum: the cursors `sample.positions` excludes by design.

    Rows have `positions`' shape, `(stratum, shape, path, line, column, offset, word)`, so
    everything that reads a drawn row reads these. `stratum` names this one; `shape` says whether
    the cursor is on a `def` or on a `class`/`module` name.

    **The same files, not a second file draw.** This is a different *question* about the documents
    the sample already reached, so each is opened once. `drawn` is the run's own draw; without it
    the file draw repeats from the same seed, which gives the same list.
    """
    rng = random.Random(f"{seed}:{corpus.name}:{corpus.sha}:declarations")
    files = sorted({row[2] for row in drawn}) if drawn else \
        sorted({path for _, path in draw_files(corpus, rng)})
    out = []
    for path in files:
        try:
            text = (corpus.dir / path).read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        found = declared(text, path.endswith(".erb"))
        if not found:
            continue
        step = max(1, len(found) // DECLARATIONS_PER_FILE)
        for kind, line, column, offset, word in found[::step][:DECLARATIONS_PER_FILE]:
            out.append(("declarations", kind, path, line, column, offset, word))
    return out
