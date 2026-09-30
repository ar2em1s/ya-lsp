"""How to read one reply: which tier the card is, where it pointed, and what kind of place that is.

Nothing here is a check. Each function turns one LSP reply into something two checks can compare.
They live together because a second reader of the same reply drifts from the first.
"""

import bisect
import re
import urllib.parse
from collections import Counter
from pathlib import Path

# **Two tiers, because a card says one thing about its confidence** (decided 2026-09-29): an answer
# resting on a name alone says *Guessed from name alone.*, and every other answer says nothing. So
# the tier is `guessed` where the card carries that line and `resolved` (sure) everywhere else: a
# derived answer reads exactly as sure as one the code names, and is held to the same standard.
# - The line is quoted exactly from `hover.rs` (`GUESSED`).
# - A reworded line would silently score every card as sure and turn the second check off.
#   `report` refuses a corpus with no guessed card, so that failure cannot pass quietly.
#
# `the_two_tiers_a_reader_sees_drawn_side_by_side` pins the wording.
GUESSED = ("Guessed from name alone.",)
# A footnote is a whole line in italics, and `hover.rs` writes nothing else that way.
FOOTNOTE = re.compile(r"^\*(.+)\*$", re.M)

# Where a **Resolved** answer lands, as a vocabulary.
#
# The top tier claims *the code names the type*, so its target must be code the running application
# loads. A `def` that exists only in a spec is one the program never sees.
#
# **The rule is a deny-list of test-only trees, not an allow-list of `app/` and `lib/`.** Real,
# loaded, first-party code lives elsewhere too:
# - lobsters autoloads `extras/`;
# - mastodon declares `module Mastodon` in `config/application.rb`;
# - forem reopens `Sidekiq::Job` in `config/initializers/sidekiq.rb`.
# An allow-list of two directory names calls all of those defects.
#
# `app` and `lib` are matched as segments **anywhere**, not as a prefix: solidus is an engine
# monorepo (`core/app/models/`) and chatwoot has an `enterprise/app/` overlay. They name the
# breakdown; they do not gate it.
SOURCE_SEGMENTS = ("app", "lib")

# Where the server keeps the two lists this file judges against.
ENVIRONMENT = Path(__file__).resolve().parents[2] / "src" / "analysis" / "environment.rs"


def _fence_names(const):
    """The `&str` list one `const` in `environment.rs` holds, read out of the source.

    **Read, not copied.** The list the *server* fences on decides the answers this check judges, so
    a hand copy that drifted would score the server against a rule it does not have, and nothing
    would say so.

    **Loud, not empty.** A rename upstream leaves this regex matching nothing, and an exemption list
    that silently reads `()` turns a check off while every counter still prints. So a missing list
    is a `SystemExit` naming the constant, the same stance
    `every_method_says_it_arrived_and_says_what_it_answered` takes towards `dispatch`.
    """
    found = re.search(rf"^(?:pub )?const {const}: \[&str; \d+\] = \[([^\]]*)\];",
                      ENVIRONMENT.read_text(encoding="utf-8"), re.M)
    names = tuple(re.findall(r'"([^"]+)"', found.group(1))) if found else ()
    if not names:
        raise SystemExit(f"audit: no `const {const}: [&str; N]` to read in {ENVIRONMENT}. "
                         "The audit reads the server's fence lists rather than holding a copy; "
                         "if the constant was renamed, rename it here too.")
    return names


# Trees that exist only for a test run. Checked **before** the segments above, so
# `spec/dummy/app/models` is a spec, not an app. That is real: forem ships a whole Rails application
# under `vendor/cache/`, and solidus a dummy app under `spec/`.
TEST_TREES = _fence_names("TEST_TREES")
# The trees that turn the server's fence off for a **cursor** and are on no target's list.
#
# The asymmetry is `environment.rs`', inherited, not restated. A name here only ever stops a
# finding, so being wrong costs only the protection. That is why the target vocabulary above stays
# at the four names every corpus agrees on.
TEST_SUPPORT = _fence_names("TEST_SUPPORT")
NOT_SOURCE = TEST_TREES + ("vendor", "node_modules", "tmp", "coverage", ".ruby-lsp", ".git",
                           "bin", "script", "public", "storage", "doc", "docs")
# The schema files under `db/` that `workspace/rails/` maps a generated declaration onto; `where`
# adds `config/routes.rb`.
#
# A column is declared by the schema and a route helper by the routes file. Both sit outside `app/`
# and `lib/` and are still where the member was really declared, so they are not defects. They are
# named explicitly, not folded into the allowed set: "the schema declared it" is a different fact
# from "a `def` declared it", and a report that merged them would hide a whole generator going
# wrong.
GENERATED_SOURCES = ("schema.rb", "structure.sql")


def tier(card):
    """Which of the two tiers a hover card is, or `None` when there was no card."""
    if not card:
        return None
    notes = FOOTNOTE.findall(card)
    if any(any(mark in note for mark in GUESSED) for note in notes):
        return "guessed"
    return "resolved"


def card_of(answer):
    """The markdown out of a `textDocument/hover` reply, or None."""
    if not isinstance(answer, dict):
        return None
    contents = answer.get("contents")
    if isinstance(contents, dict):
        return contents.get("value")
    return contents if isinstance(contents, str) else None


def locations(answer):
    """A `textDocument/definition` reply as `[(uri, range)]`, whichever shape it came in.

    Three shapes are legal: one `Location`, a list of them, or a list of `LocationLink`. This
    harness asks for links, but reads the plain shapes too, so a reply that changed shape does not
    read as "no answers".
    """
    if not answer:
        return []
    rows = answer if isinstance(answer, list) else [answer]
    out = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        if "targetUri" in row:
            out.append((row["targetUri"],
                        row.get("targetSelectionRange") or row.get("targetRange")))
        elif "uri" in row:
            out.append((row["uri"], row.get("range")))
    return [(u, r) for u, r in out if r]


def spans(answer):
    """A `textDocument/documentHighlight` reply as `[range]`."""
    if not isinstance(answer, list):
        return []
    return [row["range"] for row in answer if isinstance(row, dict) and row.get("range")]


def origins(answer):
    """The spans `definition` thought the cursor was on. Empty unless `linkSupport` is on."""
    if not answer:
        return []
    rows = answer if isinstance(answer, list) else [answer]
    return [row["originSelectionRange"] for row in rows
            if isinstance(row, dict) and row.get("originSelectionRange")]


class Shifts:
    """How many lines each document had taken by the time each position was asked.

    **Not one number per document, because the count moves during the pass.** `ask_rebased` inserts
    a line into a document each time a position points into it, so a target is off by the insertions
    that document had taken *at that position*, which the end of the pass no longer knows. So this
    records the positions and counts those at or before the one being scored.
    """

    def __init__(self):
        self._at = {}

    def record(self, target, index):
        self._at.setdefault(target, []).append(index)

    def at(self, target, index):
        return bisect.bisect_right(self._at.get(target, ()), index)


def targets(found, shifted=None, index=0):
    """A definition answer as a comparable set, with the audit's own edits taken back out.

    The server reports a target span in **the target file's** coordinates, so a jump into a document
    this pass inserted lines into comes back that many lines lower. That is correct, and would read
    as a changed answer without the subtraction.

    **A [`Shifts`], not a set of URIs**, because `ask_rebased` edits a document once per position
    pointing into it: the same target is one line down early in the pass and many lines down late,
    so the count is read *as of `index`*.

    **Pass it for the rebased pass only; the eager pass gets an empty one.** Passing it to both
    moves every eager target up too, the two sets miss each other, and positions report as *moved*
    while printing the same target count on both sides.
    """
    out = set()
    for target, span in found:
        line = span["start"]["line"] - (shifted.at(target, index) if shifted else 0)
        out.add((target, line, span["start"]["character"]))
    return out


def point(position):
    return (position.get("line", 0), position.get("character", 0))


def covers(outer, inner):
    """Does `outer` contain `inner`? Containment, not equality, on purpose.

    `definition` answers with the *selection* range (the name), and `documentHighlight` lights the
    same name, so the two are usually equal. Containment keeps a check about knowledge from failing
    on a span disagreement: a highlight lighting `@name` where the definition named `name` is not
    the defect this check looks for.
    """
    return point(outer["start"]) <= point(inner["start"]) \
        and point(inner["end"]) <= point(outer["end"])


def path_of(target):
    """A `file:` URI as a filesystem path. Anything else (a generated URI) is not a place."""
    if not isinstance(target, str) or not target.startswith("file://"):
        return None
    return urllib.parse.unquote(urllib.parse.urlsplit(target).path)


def where(corpus, target):
    """Which kind of place a definition landed in: the vocabulary the Resolved check judges on.

    `gem` is anything outside the corpus' own tree: a gem, Ruby's own lib, the vendored RBS. It
    needs no breakdown here, because this check asks about *this* repository's shape.
    """
    path = path_of(target)
    if path is None:
        return "generated-uri"
    try:
        relative = Path(path).resolve().relative_to(Path(corpus.dir).resolve())
    except ValueError:
        return "gem"
    parts = relative.parts
    # A bundle installed into the corpus is still a gem. `vendor/` alone is **not**: forem vendors a
    # whole Rails application under `vendor/cache/`, and a Resolved card landing in that dummy app
    # is the same defect as one landing in a spec. So it falls through and is reported as `vendor`.
    if "gems" in parts or ("vendor" in parts and "bundle" in parts):
        return "gem"
    for part in parts[:-1]:
        if part in NOT_SOURCE:
            return part
    leaf = parts[-1]
    if leaf == "routes.rb" or ("db" in parts and leaf.endswith(GENERATED_SOURCES)):
        return "generated-source"
    for part in parts[:-1]:
        if part in SOURCE_SEGMENTS:
            return part
    return parts[0] if len(parts) > 1 else "root"


# The unit a name mostly lives in, as a path prefix: one installed gem, one corpus checkout, or one
# Ruby's own library.
#
# Coarser than a directory and finer than `where`'s `gem`, which folds every gem into one word. The
# question here is *which* gem: a constant written in many files, nearly all in one gem, mostly
# lives in that gem.
LIBRARIES = (re.compile(r"^(.*/gems/[^/]+)/"), re.compile(r"^(.*/lib/ruby/\d+\.\d+\.\d+)/"))


def library_of(corpus, target):
    """Which library a definition landed in, or `None` for a place that is not a file.

    The corpus' own checkout is **one** library however many directories it has. An engine monorepo
    writes one namespace across `core/`, `admin/` and `api/`, and three libraries would make a
    project reopening its own module look like a project disagreeing with itself.
    """
    path = path_of(target)
    if path is None:
        return None
    try:
        Path(path).resolve().relative_to(Path(corpus.dir).resolve())
    except ValueError:
        pass
    else:
        return str(corpus.dir)
    for pattern in LIBRARIES:
        found = pattern.match(path)
        if found:
            return found.group(1)
    return path.rsplit("/", 1)[0]


def first_place(corpus, found, library=None):
    """Where the **first** place of a list comes from, against where the rest come from.

    `None` for a list of fewer than two, which has no order. Otherwise:
    - `one-library`: every place is in the same library, so there was nothing to choose;
    - `majority`: the first place is in the library most places are in;
    - `minority`: it is not.

    **A measurement, never a verdict**, like `lane3.signature`. It does not say the majority library
    is right: a name spread over forty gems has no meaningful majority, and the `member` shape is
    full of those. It says the order of a place list changed, which `shape/tier/places` cannot,
    because that signature counts places and never which came first.
    """
    if len(found) < 2:
        return None
    read = library or (lambda target: library_of(corpus, target))
    libraries = [read(target) for target, _ in found]
    counted = Counter(name for name in libraries if name is not None)
    if not counted:
        return None
    if len(counted) == 1:
        return "one-library"
    return "majority" if libraries[0] == counted.most_common(1)[0][0] else "minority"


# rubydex's spelling for a class no file named: two integers and this word. Angle brackets alone are
# not the tell (`ActiveRecord::Base::<Base>` is a readable singleton class), so the word is matched,
# not the brackets.
ANONYMOUS = "<anonymous>"


def names_an_id(card):
    """Does a card print a declaration rubydex identified by number instead of by name?

    Neutral: an internal id is not something any file wrote or a reader can type, so this is a fact
    about the text, not an opinion about the answer.
    """
    return bool(card) and ANONYMOUS in card


def in_test_support(uri):
    """Does this **cursor** sit in a tree that turns the server's fence off on its own?

    `environment::TEST_SUPPORT`, asked of the cursor only. solidus publishes shared factories and
    examples under a `testing_support` segment inside no test tree, for other suites to require. The
    server treats a cursor there as a cursor in a suite, and a spec's `def` is exactly the answer it
    wants.

    **`where` has no such list, on purpose:** `where` is the target vocabulary, where a wrong name
    deletes a real answer. This list only ever withdraws a finding.

    The server's cursor rule has a third clause, a migration, that this lacks: no corpus has
    produced a migration cursor with a *Resolved* answer in a test tree to rule on. Adding it is one
    line and one measurement.
    """
    path = path_of(uri)
    return path is not None and any(part in TEST_SUPPORT for part in path.split("/"))


def is_defect(kind, cursor_kind, supported=False):
    """Is landing in `kind`, from a cursor in `cursor_kind`, a defect?

    A cursor inside a spec resolving into the spec tree is fine: that code really is loaded where
    the cursor is, and a developer in a spec is who the answer is for. Application code resolving
    onto a spec is the defect.

    `supported` is `in_test_support` of the cursor: the same exemption, through the server's other
    cursor-side list. It is passed in because a *kind* cannot answer it: `where` reads
    `core/lib/spree/testing_support/` as `lib`, which is true, so without it every spec reopening of
    a namespace from an unfenced cursor would be called wrong.
    """
    if kind in TEST_TREES:
        return cursor_kind != kind and not supported
    return kind == "vendor"
