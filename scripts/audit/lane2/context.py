"""What every lane-2 check is handed: one position's answers, and the corpus around them.

Read **once** per position, not once per check. Five checks each calling `card_of` on one reply is
five chances for two of them to disagree about what it said, and the lane depends on the answers
being one fixed thing the checks argue about.
"""

from audit import site
from audit.answers import card_of, library_of, locations, origins, spans, tier, where
from audit.client import uri


class Corpus:
    """The corpus under test, plus the two lookups a check would otherwise redo per position."""

    def __init__(self, corpus, shifted=None):
        self.corpus = corpus
        self.dir = corpus.dir
        self.name = corpus.name
        # How many lines `ask_rebased` inserted into each document, as of each position. `None` for
        # an eager-only run. Only the **rebased** pass uses it; see `answers.targets`.
        self.shifted = shifted
        self._lines = {}
        self._kind = {}
        self._library = {}

    def lines(self, path):
        """One sampled file, split. Kept in memory for the run, never written out."""
        if path not in self._lines:
            try:
                self._lines[path] = (self.dir / path).read_text(
                    encoding="utf-8", errors="replace").split("\n")
            except OSError:
                self._lines[path] = []
        return self._lines[path]

    def kind(self, target):
        """`answers.where`, memoised: one namespace can fan out over hundreds of identical paths."""
        if target not in self._kind:
            self._kind[target] = where(self.corpus, target)
        return self._kind[target]

    def library(self, target):
        """`answers.library_of`, memoised for `kind`'s reason: a long place list resolves one path
        per place, over a handful of distinct libraries.
        """
        if target not in self._library:
            self._library[target] = library_of(self.corpus, target)
        return self._library[target]


class Row:
    """One drawn position and every reply about it."""

    __slots__ = ("index", "site", "stratum", "shape", "path", "line", "column", "offset",
                 "word", "uri", "at", "hover", "card", "tier", "named", "found", "origins",
                 "lit", "deferred", "after_card", "after",
                 "referenced", "references", "asked_rename", "renaming",
                 "prepared", "items", "callers")

    def __init__(self, index, drawn, answers, rebased, place):
        self.index = index
        (self.stratum, self.shape, self.path, self.line, self.column,
         self.offset, self.word) = drawn
        self.uri = uri(place.dir / self.path)
        # **Two names for the position, for two different readers.**
        # - `at` is what a finding's text says, so a person can go and look. It carries the word and
        #   a 1-based line.
        # - `site` is what a finding is *keyed* on: `audit.site`, the same string a ledger row uses,
        #   so lane 3 can subtract it and a committed baseline can carry it. The word is kept out of
        #   `site` on purpose: an identifier is a fragment, and the baseline is committed.
        self.at = f"{self.shape} `{self.word}` at {self.path}:{self.line + 1}"
        self.site = site(self.path, self.offset)

        self.hover = answers.get((index, "textDocument/hover"))
        self.card = card_of(self.hover)
        self.tier = tier(self.card)
        # The span `hover` says the cursor is on, which check 3 holds `definition` to.
        self.named = self.hover.get("range") if isinstance(self.hover, dict) else None
        definition = answers.get((index, "textDocument/definition"))
        self.found = locations(definition)
        self.origins = origins(definition)
        self.lit = spans(answers.get((index, "textDocument/documentHighlight")))

        # `deferred` separates "there was no second pass" from "the second pass said nothing": the
        # two things check 5 must never confuse.
        self.deferred = rebased is not None
        self.after_card = card_of(rebased.get((index, "textDocument/hover"))) \
            if self.deferred else None
        self.after = locations(rebased.get((index, "textDocument/definition"))) \
            if self.deferred else []

        # **Asked and answered are two facts.** `references` is posted at three of the six shapes,
        # and at every *n*th of those. So a position with no key here was **not asked**, and
        # one with an empty list was asked and told nothing. Check 6 keeps them apart throughout:
        # the first is the harness's choice and belongs in no denominator; the second is an answer
        # that can contradict `highlight` and `definition`.
        self.referenced = bool(answers) and (index, "textDocument/references") in answers
        self.references = locations(answers.get((index, "textDocument/references")))

        # **The same two facts again.** `prepareRename` is posted at every position and refused at
        # almost all of them, so *asked* and *answered* are further apart here than anywhere in the
        # sweep, and only *asked* is a denominator. The reply is one `Range`: the span the cursor
        # stands in, where an editor puts its rename box.
        self.asked_rename = bool(answers) and (index, "textDocument/prepareRename") in answers
        renaming = answers.get((index, "textDocument/prepareRename"))
        self.renaming = renaming if isinstance(renaming, dict) and "start" in renaming else None

        # Check 8's two hops, kept apart for the same reason plus one: the second is posted only
        # where the first answered, so an empty `callers` beside a filled `items` is the call tree's
        # own answer, not an unasked question.
        prepared = (index, "textDocument/prepareCallHierarchy")
        self.prepared = bool(answers) and prepared in answers
        items = answers.get(prepared)
        self.items = items if isinstance(items, list) else []
        callers = answers.get((index, "callHierarchy/incomingCalls"))
        self.callers = callers if isinstance(callers, list) else []
