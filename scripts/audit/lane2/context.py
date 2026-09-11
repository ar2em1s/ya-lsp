"""What every lane-2 check is handed: one position's answers, and the corpus around them.

Read **once** per position rather than once per check. Five checks each calling `card_of` on the
same reply is five chances for two of them to disagree about what the reply said, and the whole
lane is built on the answers being one fixed thing the checks argue about.
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
        # How many lines `ask_rebased` inserted into each document, as of each position. `None`
        # for an eager-only run, and it is the **rebased** pass's business alone — see
        # `answers.targets`.
        self.shifted = shifted
        self._lines = {}
        self._kind = {}
        self._library = {}

    def lines(self, path):
        """One sampled file, split. In memory for the length of the run and never written out."""
        if path not in self._lines:
            try:
                self._lines[path] = (self.dir / path).read_text(
                    encoding="utf-8", errors="replace").split("\n")
            except OSError:
                self._lines[path] = []
        return self._lines[path]

    def kind(self, target):
        """`answers.where`, memoised: one `Spree::Base` fans out over 200 identical spec paths."""
        if target not in self._kind:
            self._kind[target] = where(self.corpus, target)
        return self._kind[target]

    def library(self, target):
        """`answers.library_of`, memoised for `kind`'s reason: a 539-place list is 539 lookups
        over a handful of distinct libraries, and every one of them resolves a path."""
        if target not in self._library:
            self._library[target] = library_of(self.corpus, target)
        return self._library[target]


class Row:
    """One drawn position and every reply about it."""

    __slots__ = ("index", "site", "stratum", "shape", "path", "line", "column", "offset",
                 "word", "uri", "at", "hover", "card", "tier", "named", "found", "origins",
                 "lit", "deferred", "after_card", "after", "listed", "offered")

    def __init__(self, index, drawn, answers, rebased, place):
        self.index = index
        (self.stratum, self.shape, self.path, self.line, self.column,
         self.offset, self.word) = drawn
        self.uri = uri(place.dir / self.path)
        # **Two names for the position, and they are for two different readers.** `at` is what a
        # finding's text says, so a person can go and look: it carries the word and a 1-based
        # line. `site` is what a finding is *keyed* on — `audit.site`, the same string a ledger
        # row uses — so lane 3 can subtract it and a committed baseline can carry it. The word is
        # deliberately not in `site`: an identifier is a fragment, and the baseline is committed.
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

        # `deferred` separates "there was no second pass" from "the second pass said nothing",
        # which are the two things check 5 must never confuse.
        self.deferred = rebased is not None
        self.after_card = card_of(rebased.get((index, "textDocument/hover"))) \
            if self.deferred else None
        self.after = locations(rebased.get((index, "textDocument/definition"))) \
            if self.deferred else []

        # **The one reply here lane 2 did not ask for.** `lane1.completion` poses it, at these
        # same cursors and in these same coordinates, and writes it into this dict — see the note
        # there. A check that holds a card against the list beside it is comparing two of the
        # server's own answers, which is lane 2's definition, but only lane 1 has a reason to send
        # the second request; asking it a third time here would be a third request per position on
        # the largest thing in the budget.
        #
        # **`listed` and `offered` are two facts and collapsing them loses the check.** A cursor
        # the key never posed — a bare word, a setter, `--no-key` — has no list; a cursor it posed
        # and got nothing back from has an empty one, and that is an answer. Check 5 keeps
        # `deferred` apart from its replies for the same reason.
        offer = answers.get((index, "textDocument/completion")) if answers else None
        self.listed = bool(answers) and (index, "textDocument/completion") in answers
        items = offer.get("items") if isinstance(offer, dict) else offer
        self.offered = items if isinstance(items, list) else []
