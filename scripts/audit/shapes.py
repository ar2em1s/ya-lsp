"""Which *kind* of cursor a position is, and every candidate one file holds.

This is the second of the sample's two strata (directory is the first). A benchmark that asks only
`receiver.member` measures only the part of a server that answers that shape.
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

# ------------------------------------------------------------------------------ declarations
#
# The cursors `DEF_SITE` and `DEFINING` **exclude** from the draw, drawn here on purpose and apart.
# - Excluding them is right for the six shapes above: at a `def`'s own name, `definition` would ask
#   where its own answer is, and `sample.positions` measures navigation.
# - But `prepareRename`, `prepareTypeHierarchy`, `prepareCallHierarchy` and `implementation` are
#   only ever sent at exactly those cursors.
# So they are a stratum of their own, under their own counters, and no counter from the six shapes
# moves.

# **An operator `def` is not drawn, and the alternation keeps it from being drawn wrongly.** With
# one optional `self\.` group, `def self./(other)` matches by capturing `self`: a cursor on the
# receiver keyword, asking four requests about nothing.
# - The first arm takes a named singleton method.
# - The second takes a plain one, and refuses any name a dot follows.
# Neither can spell `/`, `==` or `[]`, so those are out of the stratum: a handful of cursors lost,
# against a class of cursor that is not one.
DECLARED = (
    ("method", re.compile(
        r"(?:^|[^\w.:])def\s+(?:self\s*\.\s*([A-Za-z_][A-Za-z0-9_]*[?!=]?)"
        r"|([A-Za-z_][A-Za-z0-9_]*[?!=]?)(?![A-Za-z0-9_?!=])(?!\s*\.))")),
    ("type", re.compile(
        r"(?:^|[^\w.:])(?:class|module)\s+([A-Z][A-Za-z0-9_]*(?:::[A-Z][A-Za-z0-9_]*)*)")),
)

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

    `not_routes` is `routes.non_helpers`' answer for this corpus: `_path`/`_url` names the corpus
    writes itself, which Rails therefore did not generate. Without it the `route` shape is a suffix
    match, and samples columns (`normalized_url`), plain methods (`avatar_url`) and SQL aliases far
    more often than real helpers.
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
            # **A `member` needs its dot in code.** `MEMBER`'s `\s*` matches a newline and the
            # pattern runs on raw text, so a comment ending in a full stop would reach across the
            # line break and draw the next line's first word (`def`, `class`, `module`), asking
            # `definition` where a keyword is defined. `found.start()` is the dot itself (the
            # lookbehind is zero-width). A dot ending a line of real code is a line continuation and
            # stays drawn.
            if shape == "member" and not inside(found.start()):
                continue
            if shape == "call" and word in KEYWORDS:
                continue
            if shape == "route" and word in not_routes:
                continue
            # **A qualified constant is two questions.** `CONSTANT` captures
            # `Admin::UsersController` whole, and the cursor goes at the start of the match: the
            # namespace, `Admin`. The two halves answer differently: `Admin` may resolve to no place
            # (a namespace implied by the directory, with no `module Admin` anywhere), while the
            # leaf resolves to its file. Both are drawn: the namespace is a real interaction, and
            # the leaf is where the answer lives.
            here = [(start, word)]
            if shape == "constant" and "::" in word:
                leaf = word.rsplit("::", 1)[-1]
                here.append((start + len(word) - len(leaf), leaf))
            # A definition site is not a position a developer navigates *from*. `MEMBER` reads
            # `def self.foo` as `.foo` (the character before the dot is `f`, not another dot), and
            # such a cursor asks `definition` where its own answer is. `DEF_SITE` covers both
            # spellings, so the exclusion is one rule for every shape.
            if DEF_SITE.search(text[max(0, start - 24):start]):
                continue
            if shape == "constant" and DEFINING.search(text[max(0, start - 12):start]):
                continue
            for at, name in here:
                line, column = at_offset(starts, at)
                out[shape].append((line, column, at, name))
    # **One cursor is one question, whichever shapes match it.** A route helper is also a bare call,
    # and `. foo(` (a dot, a space, a name, a paren) is a `member` to one pattern and a `call` to
    # the other, because `CALL`'s lookbehind only refuses a dot directly before it. Drawn twice,
    # that cursor is asked twice, counted twice by every lane-2 check, and named by one
    # `audit.site`, so lane 3 subtracting one finding would strike both.
    #
    # `KEEP` settles every collision with one order, most specific first, instead of a rule per
    # pair:
    # - a sigil or a macro keyword names the shape outright;
    # - a leading dot names a receiver;
    # - a `_path` suffix is a suffix plus `routes.non_helpers`;
    # - a trailing paren is the weakest evidence any pattern reads.
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


def declared(text, is_erb=False):
    """Every `def`, `class` and `module` name in one file, as [(kind, line, column, offset, word)].

    The mirror of `find` for the declaration stratum: same masking, same regions, same reason (a
    name inside a string, a comment or a heredoc is not a cursor anyone uses). A qualified
    `class Foo::Bar` is drawn at the **leaf**, where the declaration is. The `constant` shape
    already draws the namespace half; drawing it here too would ask two strata one question.
    """
    regions = ruby_regions(text, is_erb)
    hidden = masked(text)
    starts = line_starts(text)
    out = []
    for kind, pattern in DECLARED:
        for found in pattern.finditer(text):
            # Whichever arm of the alternation matched; a pattern with one group has one.
            group = next(n for n in range(1, (found.re.groups or 1) + 1) if found.group(n))
            at, word = found.start(group), found.group(group)
            if not any(lo <= at < hi for lo, hi in regions) or hidden[at]:
                continue
            if "::" in word:
                leaf = word.rsplit("::", 1)[-1]
                at, word = at + len(word) - len(leaf), leaf
            line, column = at_offset(starts, at)
            out.append((kind, line, column, at, word))
    out.sort(key=lambda row: row[3])
    return out
