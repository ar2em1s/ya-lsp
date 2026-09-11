"""Which *kind* of cursor a position is, and every candidate one file holds.

Stratifying by this as well as by directory is the second of the two strata the sample is drawn
on, and it exists because a benchmark that asks only `receiver.member` measures only the part of
a server that answers that shape.
"""

import re

from audit.ruby import at_offset, line_starts, masked, ruby_regions

MEMBER = re.compile(r"(?<![.\d])\.\s*([a-z_][A-Za-z0-9_]*[?!]?)")
CONSTANT = re.compile(r"(?<![.:\w])([A-Z][A-Za-z0-9_]*(?:::[A-Z][A-Za-z0-9_]*)*)")
DEFINING = re.compile(r"(?:^|[^\w.])(?:class|module)\s+$")
DEF_SITE = re.compile(r"(?:^|[^\w.])def\s+(?:self\s*\.\s*)?$")
CALL = re.compile(r"(?<![.:@$\w])([a-z_][A-Za-z0-9_]*[?!]?)\s*\(")
ROUTE = re.compile(r"(?<![.:@$\w])([a-z_][A-Za-z0-9_]*_(?:path|url))\b(?!:)")
IVAR = re.compile(r"(?<!@)@([a-z_][A-Za-z0-9_]*)\b")
MACROS = ("belongs_to", "has_many", "has_one", "has_and_belongs_to_many",
          "validates", "validate", "scope", "delegate", "before_save", "after_save",
          "before_create", "after_create", "before_destroy", "after_destroy",
          "before_validation", "after_validation", "after_commit", "before_action",
          "after_action", "skip_before_action", "around_action", "enum", "attribute")
SYMBOL = re.compile(r"(?<![.:@$\w])(?:" + "|".join(MACROS) + r")\s+:([a-z_][A-Za-z0-9_]*[?!]?)")

KEYWORDS = {
    "if", "unless", "while", "until", "for", "in", "do", "end", "then", "else", "elsif",
    "case", "when", "begin", "rescue", "ensure", "raise", "return", "yield", "super", "self",
    "nil", "true", "false", "and", "or", "not", "def", "class", "module", "require", "loop",
    "puts", "print", "lambda", "proc", "new", "attr_accessor", "attr_reader", "attr_writer",
}

SHARES = (("member", 0.45), ("constant", 0.15), ("call", 0.15),
          ("ivar", 0.10), ("symbol", 0.10), ("route", 0.05))

PATTERNS = (("member", MEMBER), ("constant", CONSTANT), ("call", CALL),
            ("route", ROUTE), ("ivar", IVAR), ("symbol", SYMBOL))


def find(text, is_erb=False, not_routes=frozenset()):
    """Every candidate position in one file, as {shape: [(line, column, offset, word)]}.

    `not_routes` is `routes.non_helpers`' answer for this corpus: names ending `_path`/`_url`
    that the corpus itself writes down and Rails therefore did not generate. Without it the
    `route` shape is a suffix match, and a suffix match samples columns (`normalized_url`),
    plain methods (`avatar_url`) and SQL aliases — one real helper in fifty-two on lobsters.
    """
    regions = ruby_regions(text, is_erb)
    hidden = masked(text)
    starts = line_starts(text)

    def inside(offset):
        return any(lo <= offset < hi for lo, hi in regions) and not hidden[offset]

    out = {name: [] for name, _ in SHARES}
    for shape, pattern in PATTERNS:
        for found in pattern.finditer(text):
            start, word = found.start(1), found.group(1)
            if not inside(start):
                continue
            # **The dot is where a `member` comes from, and it was never checked.** `MEMBER`'s
            # `\s*` matches a newline and the pattern runs on the raw text, so a comment ending
            # in a full stop reached across the line break and drew the next line's first word.
            # Measured: 44 positions over five corpora — `def` 16, `class` 15, `module` 8 and
            # four more — each of them asking `definition` where a Ruby keyword is defined.
            # `found.start()` is the dot itself, the lookbehind being zero-width. A dot that ends
            # a line of real code is a line continuation and stays drawn.
            if shape == "member" and not inside(found.start()):
                continue
            if shape == "call" and word in KEYWORDS:
                continue
            if shape == "route" and word in not_routes:
                continue
            # **A qualified constant is two questions, and the match only ever asked one.**
            # `CONSTANT` captures `Api::V1::Statuses::BaseController` whole and the cursor goes
            # at the start of the match — which is `Api`, the namespace. Measured: 48 positions
            # over five corpora sat on a namespace, and a column walk on lobsters showed
            # col 29-31 of `class Mod::MailsController < Mod::ModController` resolve `Mod` with
            # no place (correct — nothing declares `module Mod`, it is implicit from the
            # directory) while col 34 resolves `class Mod::ModController` to its file. So the
            # class in a qualified constant was never a cursor at all. Both are drawn now: the
            # namespace is a real interaction and the leaf is where the answer lives.
            here = [(start, word)]
            if shape == "constant" and "::" in word:
                leaf = word.rsplit("::", 1)[-1]
                here.append((start + len(word) - len(leaf), leaf))
            # A definition site is not a position a developer navigates *from*, and the
            # `member` shape reached one the `call` shape was already excluding: `def self.foo`
            # is a `.foo` by `MEMBER`'s reading, because the character before the dot is `f` and
            # not another dot. 44 of 4,929 drawn positions were `def self.` names, and every one
            # of them asked `definition` where its own answer is. `DEF_SITE` covers both
            # spellings, so the exclusion is now the same rule for every shape.
            if DEF_SITE.search(text[max(0, start - 24):start]):
                continue
            if shape == "constant" and DEFINING.search(text[max(0, start - 12):start]):
                continue
            for at, name in here:
                line, column = at_offset(starts, at)
                out[shape].append((line, column, at, name))
    # **One cursor is one question, whichever shapes match it.** A route helper is also a bare
    # call, and `. foo(` — a dot, a space, a name, a paren — is a `member` to one pattern and a
    # `call` to the other, because `CALL`'s lookbehind only refuses a dot it is standing on. Drawn
    # twice, that cursor is asked twice, counted twice by every lane-2 check, and named by a
    # single `audit.site` — so lane 3 subtracting one finding would strike both off. Measured:
    # 2 of 4,633 drawn positions over five corpora, both `call`/`member`.
    #
    # `KEEP` is that decision as one order, most specific first, rather than one pairwise rule
    # per collision: a sigil or a macro keyword names the shape outright, a leading dot names a
    # receiver, a `_path` suffix is a suffix plus `routes.non_helpers`, and a trailing paren is
    # the weakest evidence any of these patterns reads.
    KEEP = ("ivar", "symbol", "constant", "member", "route", "call")
    taken = set()
    for name in KEEP:
        kept = []
        for candidate in out[name]:
            offset = candidate[2]
            if offset in taken:
                continue
            taken.add(offset)
            kept.append(candidate)
        out[name] = kept
    return out
