"""How to read one reply: which tier the card is, where it pointed, and what kind of place that is.

Nothing here is a check. Every function is a way of turning one LSP reply into something two
checks can compare, and they live together because a second reader of the same reply drifts from
the first.
"""

import bisect
import re
import urllib.parse
from collections import Counter
from pathlib import Path

# **The card never prints the word "Resolved".** A resolved answer is one with nothing to
# caveat, so it carries no caveat, and the tier is what the card does *not* say — that is
# `hover.rs`'s design and not an oversight. Reading the tier back therefore means recognising
# the sentences the other two tiers do carry, which is why these are quoted from `hover.rs`
# rather than matched loosely: a reworded footnote would silently reclassify every card in the
# corpus as Resolved, which turns the second check off without ever failing it. `report`
# refuses to report a corpus in which it saw no derived and no guessed card at all, so that
# particular way of being wrong cannot pass quietly.
# `hover.rs` writes four sentences opening on the first of these and they are deliberately
# matched on the clause they share: one says the receiver had no type at all, and three say it
# had one and the member is not on it. The tier is the same for all four — a list matched on a
# name is a guess whichever of the two put it there — so the entry stays one string rather than
# four, and `the_three_tiers_of_answer_drawn_side_by_side` is where the full wording is pinned.
GUESSED = ("Matched on the method name alone", "Type guessed from the name")
DERIVED = ("Type derived through", "Type taken from the assignment on line",
           "Type taken from the signature for",
           "Type taken from where",
           "the controller Rails renders this template from",
           "Reached through the view context", "Reached through `helper_method`",
           "Found on an instance of")
# A footnote is the whole line, italic, and `hover.rs` writes nothing else that way.
FOOTNOTE = re.compile(r"^\*(.+)\*$", re.M)

# Where a **Resolved** answer lands, as a vocabulary. The top tier's claim is that *the code
# names the type*, so the declaration it points at has to be one the running application loads —
# a `def` that exists only in a spec is a `def` the program never sees, and a card that resolves
# onto one has promised something the code does not say.
#
# **The plan says "outside `app/`, `lib/`, or a gem" and that phrasing does not survive contact
# with the corpora.** Measured over all five: lobsters autoloads `extras/` and 12 Resolved
# answers land there correctly; mastodon declares `module Mastodon` in `config/application.rb`
# and four land there; forem reopens `Sidekiq::Job` in `config/initializers/sidekiq.rb`. All of
# those are real, loaded, first-party code, and an allow-list of two directory names calls every
# one of them a defect. Six Ruby repositories do not agree on where source lives, so the rule
# that generalises is the **deny**-list: the trees that only run under a test harness.
#
# `app` and `lib` are still recognised as segments **anywhere** rather than as a prefix, because
# solidus is an engine monorepo whose models are `core/app/models/` and chatwoot has an
# `enterprise/app/` overlay — but they are now vocabulary for the breakdown, not a gate.
SOURCE_SEGMENTS = ("app", "lib")
# Trees that only exist for a test run. Checked **before** the segments above, so
# `spec/dummy/app/models` is a spec and not an app — not hypothetical, forem ships a whole Rails
# application under `vendor/cache/` and solidus a dummy app under `spec/`.
TEST_TREES = ("spec", "test", "tests", "features")
NOT_SOURCE = TEST_TREES + ("vendor", "node_modules", "tmp", "coverage", ".ruby-lsp", ".git",
                           "bin", "script", "public", "storage", "doc", "docs")
# The three files `workspace/rails/` legitimately maps a generated declaration onto: a column
# was declared by the schema, a route helper by `config/routes.rb`. Those are outside `app/` and
# `lib/` and they are still the place the member was really declared, so they are not defects —
# but they are named here explicitly rather than folded into the allowed set, because "the
# schema declared it" is a different fact about an answer than "a `def` declared it" and a
# report that cannot tell them apart is a report that hides a whole generator going wrong.
GENERATED_SOURCES = ("schema.rb", "structure.sql")


def tier(card):
    """Which of the three tiers a hover card is, or `None` when there was no card."""
    if not card:
        return None
    notes = FOOTNOTE.findall(card)
    if any(any(mark in note for mark in GUESSED) for note in notes):
        return "guessed"
    if any(any(mark in note for mark in DERIVED) for note in notes):
        return "derived"
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

    Three are legal — one `Location`, a list of them, and a list of `LocationLink` — and this
    harness asks for the link shape. The plain shapes are read anyway because a reply that
    silently stopped matching would otherwise read as "no answers".
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

    **Not one number per document, because the count moves during the pass.** `ask_rebased`
    inserts a line into a document every time a position points into it, so a target's line is
    off by however many insertions that document had taken *at that position* — a figure the end
    of the pass no longer knows. Recording the positions instead and counting those at or before
    the one being scored is the whole of it; a total of a few thousand entries per corpus against
    a snapshot per position.
    """

    def __init__(self):
        self._at = {}

    def record(self, target, index):
        self._at.setdefault(target, []).append(index)

    def at(self, target, index):
        return bisect.bisect_right(self._at.get(target, ()), index)


def targets(found, shifted=None, index=0):
    """A definition answer as a comparable set, with the audit's own edits taken back out.

    `mod.rs` moves a target span into **the target file's** coordinates, so a jump that lands in
    a document this pass inserted lines into comes back that many lines lower — correct
    behaviour that would otherwise read as a changed answer. Subtracting them is the only way
    the two passes compare at all.

    **A [`Shifts`] and not a set of URIs**, because `ask_rebased` edits a document once per
    position that points into it: the same target is one line down early in the pass and eleven
    late, so the count has to be read *as of `index`*. It is the **rebased** pass's business and
    empty for the eager one. Passing it to both is the bug this signature makes hard to write: it
    moves every eager target up as well, the two sets miss each other, and 54 of 193 lobsters
    positions reported as *moved* while printing `1 targets eagerly, 1 after the edit`.
    """
    out = set()
    for target, span in found:
        line = span["start"]["line"] - (shifted.at(target, index) if shifted else 0)
        out.add((target, line, span["start"]["character"]))
    return out


def point(position):
    return (position.get("line", 0), position.get("character", 0))


def covers(outer, inner):
    """Does `outer` contain `inner`? Containment rather than equality on purpose.

    `definition` answers with the *selection* range — the name — and `documentHighlight` lights
    the same name, so the two are equal in the ordinary case. Containment is what keeps a check
    that is about knowledge from failing on a disagreement about spans: a highlight that lit
    `@name` where the definition named `name` is not the defect this check is looking for.
    """
    return point(outer["start"]) <= point(inner["start"]) \
        and point(inner["end"]) <= point(outer["end"])


def path_of(target):
    """A `file:` URI as a filesystem path. Anything else — a generated URI — is not a place."""
    if not isinstance(target, str) or not target.startswith("file://"):
        return None
    return urllib.parse.unquote(urllib.parse.urlsplit(target).path)


def where(corpus, target):
    """Which kind of place a definition landed in: the vocabulary the Resolved check judges on.

    `gem` is anything outside the corpus' own tree — a gem, Ruby's own lib, the vendored RBS —
    and it needs no further breakdown here, because the question this check asks is about
    *this* repository's shape and a gem has its own.
    """
    path = path_of(target)
    if path is None:
        return "generated-uri"
    try:
        relative = Path(path).resolve().relative_to(Path(corpus.dir).resolve())
    except ValueError:
        return "gem"
    parts = relative.parts
    # A bundle installed into the corpus is still a gem. `vendor/` on its own is **not**: forem
    # vendors a whole Rails application under `vendor/cache/`, and a Resolved card landing in
    # that dummy app is the same defect as one landing in a spec, so it falls through and is
    # reported as `vendor` rather than waved past as third-party code.
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


# The unit a name mostly lives in, as a path prefix: one installed gem, one checkout of a
# corpus, one Ruby's own library. Coarser than a directory and finer than `where`'s `gem`, which
# folds every gem in the bundle into one word — the question here is *which* gem, because a
# constant written in 69 files of which 60 are `sidekiq-8.1.7` has a library it mostly lives in.
LIBRARIES = (re.compile(r"^(.*/gems/[^/]+)/"), re.compile(r"^(.*/lib/ruby/\d+\.\d+\.\d+)/"))


def library_of(corpus, target):
    """Which library a definition landed in, or `None` for a place that is not a file.

    The corpus' own checkout is **one** library however many directories it has: an engine
    monorepo writes one namespace across `core/`, `admin/` and `api/`, and calling those three
    libraries would report a project reopening its own module as a project disagreeing with
    itself.
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
    """Where the **first** place of a list comes from, against where the rest of them do.

    `None` for a list of fewer than two, which has no order to measure. Otherwise one of three
    words: `one-library` when every place is in the same one and there was nothing to choose,
    `majority` when the first place is in the library most of them are in, and `minority` when it
    is not.

    **A measurement and never a verdict**, exactly as `lane3.signature` is. It does not say the
    majority library is the right answer — a name genuinely spread over forty gems has no
    meaningful majority, and the `member` shape is full of those. What it can say is that the
    ordering of a place list changed at all, which is the one thing `shape/tier/places` cannot:
    that signature counts how many places an answer named and never which of them came first.
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


# rubydex's own spelling for a class no file gave a name to: two integers and this word. Angle
# brackets alone are not the tell — `ActiveRecord::Base::<Base>` is how a singleton class is
# written and a person can read it — so the word is matched and not the brackets.
ANONYMOUS = "<anonymous>"


def names_an_id(card):
    """Does a card print a declaration rubydex identified by number rather than by name?

    Neutral: an internal id is not something any file wrote and not something a reader can type,
    so this is a fact about the text rather than an opinion about the answer.
    """
    return bool(card) and ANONYMOUS in card


def is_defect(kind, cursor_kind):
    """Is landing in `kind`, from a cursor sitting in `cursor_kind`, a defect?

    A cursor already inside a spec resolving to something in the spec tree is not one: that code
    really is loaded where the cursor is, and a developer with a cursor in a spec is exactly who
    the answer is for. It is application code resolving onto a spec that is the defect.
    """
    if kind in TEST_TREES:
        return cursor_kind != kind
    return kind == "vendor"
