"""Which bytes of a file are Ruby a cursor could sit in, and where the lines start.

A cheap scan rather than a parser, and it does not need to be one: it runs over the file before
the server sees it, so whatever it misclassifies it misclassifies for every position equally.
The heredoc is the one case that had to be handled rather than shrugged at.
"""

import hashlib
import re

ERB_TAG = re.compile(r"<%(?!%)[-=#]?(.*?)[-]?%>", re.S)
# **Two forms, and they are not equally safe to widen.** `<<-TAG` and `<<~TAG` are unambiguously
# heredocs — no other Ruby construct spells a dash or a tilde there — so the tag may be any
# identifier, which is what `<<-EndOfTests` needs: read under the screaming-case rule below it
# matched the `E` alone, and the hunt for a line reading `E` ran to the end of the file. A **bare**
# `<<TAG` collides with `arr <<item`, the push operator written without a space, so it keeps the
# convention that tells the two apart. Being too narrow here is worse than being too wide, because
# a partial match still opens a heredoc and then never closes it.
HEREDOC = re.compile(r"<<[-~][\"']?([A-Za-z_][A-Za-z0-9_]*)[\"']?"
                     r"|<<[\"']?([A-Z_][A-Z0-9_]*)[\"']?")


def ruby_regions(text, is_erb):
    """The byte ranges that hold Ruby. A whole `.rb` file; only the tags of a template."""
    if not is_erb:
        return [(0, len(text))]
    out = []
    for found in ERB_TAG.finditer(text):
        if found.group(0).startswith("<%#"):
            continue
        out.append((found.start(1), found.end(1)))
    return out


def _block_comments(lines, starts, hidden):
    """Hide `=begin` … `=end`, which is Ruby's other comment and the walk's third unbalanced quote.

    **The character walk below assumes valid Ruby has no unbalanced quote outside a comment or a
    heredoc, and that sentence was one construct short.** A `=begin` block holds prose, prose holds
    apostrophes, and one `doesn't` a hundred lines into a class' documentation opened a string the
    walk never closed — it lost its place for the rest of the file and drew nothing from it.
    discourse writes one; the other five corpora do not, which is why five corpora read 0 of 12,146
    and six read 9 of 24,058.

    **First of the three passes, ahead of heredocs**, because a `<<~SQL` written inside a block
    comment is not an opener either — the same mistake one construct over.

    Ruby's own rule, exactly: `=begin` opens only at column 0 and `=end` closes only at column 0,
    so there is nothing to infer and `startswith` is the whole test.
    """
    index = 0
    while index < len(lines):
        if not lines[index].startswith("=begin"):
            index += 1
            continue
        close = index + 1
        while close < len(lines) and not lines[close].startswith("=end"):
            close += 1
        for row in range(index, min(close + 1, len(lines))):
            for offset in range(starts[row], starts[row] + len(lines[row])):
                hidden[offset] = 1
        index = close + 1


def _code_columns(line):
    """Which columns of **one line, read alone**, are code: outside a quote it opened, and before
    a `#` that is not inside one.

    Deliberately single-line and deliberately approximate. It exists for one caller — deciding
    whether a `<<~TAG` on this line is a heredoc opener — and that caller runs before anything
    knows where the multi-line literals are, which is the ordering `masked` is built on. What it
    has to get right is the two shapes that cost a whole file:

      `# DB.exec <<~SQL`        a commented-out heredoc, whose terminator is commented out too,
                                so the search for a bare `SQL` runs to the end of the file
      `emit "SQL = <<~SQL"`     an opener quoted inside a string, which is a code generator
                                writing Ruby rather than a heredoc

    Both read as openers to a bare regex, and both swallowed a discourse file whole.
    """
    code = bytearray(len(line))
    quote, index = None, 0
    while index < len(line):
        ch = line[index]
        if quote is not None:
            if ch == "\\":
                index += 2
                continue
            if ch == quote:
                quote = None
            index += 1
            continue
        # A `#` outside a quote runs to the end of the line, and everything after it is comment.
        if ch == "#":
            break
        if ch in "\"'`":
            quote = ch
            index += 1
            continue
        code[index] = 1
        index += 1
    return code


def _opener(line):
    """The first `<<~TAG` on this line that is actually code, or `None`."""
    code = _code_columns(line)
    for found in HEREDOC.finditer(line):
        if code[found.start()]:
            return found
    return None


def _heredocs(lines, starts, hidden):
    """Hide every heredoc body, because it does not misclassify symmetrically.

    A heredoc is where SQL lives, and SQL is full of words shaped exactly like Ruby ones:
    `cast(x as blob) as confidence_order_path` inside a `<<~SQL` reads to the scan below as a
    Rails route helper. Between 1.7% and 5.1% of every candidate position in these corpora is
    inside one.

    **An opener has to be code, and `_code_columns` is what decides that.** A bare search for the
    pattern reads a commented-out `# DB.exec <<~SQL` and a quoted `emit "… <<~SQL"` as openers,
    then hunts for a terminator that is commented out or does not exist — and hides the rest of
    the file on the way. Six discourse files, four of them migrations somebody had commented out.
    """
    index = 0
    while index < len(lines):
        # A line a block comment already hid is not code, so nothing on it opens anything.
        if lines[index] and hidden[starts[index]]:
            index += 1
            continue
        found = _opener(lines[index])
        if not found:
            index += 1
            continue
        # Either alternative of `HEREDOC`, whichever matched.
        tag, close = (found.group(1) or found.group(2)), index + 1
        while close < len(lines) and lines[close].strip() != tag:
            close += 1
        for row in range(index + 1, min(close + 1, len(lines))):
            for offset in range(starts[row], starts[row] + len(lines[row])):
                hidden[offset] = 1
        index = max(close, index + 1)


# A percent literal, and the letter is required for every form but the operand-position one.
# `%w[a b]` is an array of strings, `%i[a b]` of symbols, `%q()`/`%Q{}`/`%()` a string and
# `%r{}` a regex — and the words inside every one of them scan as Ruby identifiers while being
# nothing of the kind.
PERCENT = re.compile(r"%([wWiIqQrsxX])?([^\sA-Za-z0-9])")
# Where a bare `%` may start a literal rather than be the modulo operator. `a % b` and
# `"%s" % x` are the ordinary case and hiding either would blank real code, so the bare form is
# only read in **operand position**: at the start of a line, or after an opener or an operator.
# Anything after an identifier, a number or a closing bracket is modulo.
OPERAND = "([{,=|&!~<>+-*/:;?\n\t "
PAIRS = {"(": ")", "[": "]", "{": "}", "<": ">"}


def _percent_end(text, start):
    """Where the percent literal at `start` ends, delimiters included, or None if it is not one.

    Called from the character walk while it is reading **code**, which is the only place the
    question can be answered: `%w[a b]` is an array of strings and `%r{...}` a regex, but a `%`
    inside a comment, a string or a heredoc body is a character and `a % b` is modulo. The walk
    already knows which of those it is standing in, so this asks only the two things it does not
    know — whether a letter names a form, and whether a bare `%` is in **operand position**.

    It used to be a separate pass, run last so that a `%w[]` inside a comment was already hidden.
    That ordering could not work, because the walk reaches the literal first: forem writes
    `URL_REGEX = %r{https?://[^\\s<>"{}|\\\\^`\\[\\]]+}`, whose `"` opened a string that ran to
    the end of the file and masked 93% of it. Reading percent literals *inside* the walk removes
    the ordering rather than reversing it.
    """
    found = PERCENT.match(text, start)
    if not found:
        return None
    letter, opener = found.group(1), found.group(2)
    # **An ERB tag is not a percent literal**, and both of its delimiters read as one. `%>` is a
    # `%` opened on `>`, and `<%=` a `%` opened on `=`; the first blanked everything up to the
    # next `>` anywhere in the markup. Measured before this guard: the `views` stratum fell from
    # 145 positions to 76 on lobsters and 110 to 42 on mastodon — half a stratum, silently, in
    # the one file type where a template is the whole point.
    if (start and text[start - 1] == "<") or text[start + 1:start + 2] == ">":
        return None
    if letter is None:
        before = text[start - 1] if start else "\n"
        if before not in OPERAND:
            return None                    # `a % b`: the modulo operator, not a literal
    closer, depth, index = PAIRS.get(opener, opener), 1, found.end()
    while index < len(text) and depth:
        ch = text[index]
        if ch == "\\":
            index += 2
            continue
        if ch == opener and opener in PAIRS:
            depth += 1
        elif ch == closer:
            depth -= 1
        index += 1
    return index


# **A `/` opens a regex literal in operand position and is division everywhere else**, and there
# is no cheaper way to tell the two apart: `a / b` and `s.gsub(/,/, "")` differ only in what
# precedes the slash. So the scan reads backwards over spaces and asks what it lands on — an
# opener or an operator opens a regex; an identifier, a number or a closing bracket is division;
# and a keyword is the one word-shaped exception (`when /re/`). Left unmasked, a character class
# is read as code: `/[^A-Za-z0-9]/` offered `A` and `Z` as **constants**, on five positions over
# five corpora.
REGEX_OPENS = set("([{,=~!|&<>+-*/%;:?")
REGEX_WORDS = frozenset({"when", "and", "or", "not", "if", "unless", "while", "until",
                         "return", "case", "then", "do", "else", "elsif", "begin", "in",
                         "yield", "puts", "match", "split", "gsub", "sub", "scan", "grep"})
WORD_END = re.compile(r"[A-Za-z_][A-Za-z0-9_]*$")
# `$\`` and `$'` are the pre- and post-match globals; `$"` is the loaded-feature list and
# `$/` the input separator. Every one of them is spelled with a character that opens a literal.
QUOTE_GLOBALS = frozenset("`'\"/")


def _opens_regex(line, index):
    back = index - 1
    while back >= 0 and line[back] in " \t":
        back -= 1
    if back < 0:
        return True                       # first thing on the line
    if line[back] in REGEX_OPENS:
        return True
    word = WORD_END.search(line[:back + 1])
    return bool(word and word.group(0) in REGEX_WORDS)


def _regex_end(line, index):
    """Where the regex opened at `index` closes, flags included, or None if not on this line.

    Not on this line means **not masked**. An `/x` regex may span lines, and a scan that guessed
    at where one ended would swallow the rest of the file exactly the way an unbalanced quote
    used to — so an unterminated slash is read back as the division it far more likely is.
    """
    at, klass = index + 1, False
    while at < len(line):
        ch = line[at]
        if ch == "\\":
            at += 2
            continue
        if klass:
            klass = ch != "]"
        elif ch == "[":
            klass = True
        elif ch == "/":
            at += 1
            while at < len(line) and line[at] in "imxounse":
                at += 1
            return at
        at += 1
    return None


def _regex_spans_lines(lines, starts, row, index, limit=40):
    """The end offset of a regex opened at the **end** of line `row`, or None.

    A slash that is the last thing on its line cannot be division, because division needs a right
    operand — so this is the one case where scanning forward over newlines is safe, and it has to
    be handled rather than skipped: forem writes a `/mx` regex over ten lines whose character
    class holds a **backtick**, which the walk below read as an opening command literal and used
    to mask the remaining 93% of the file with. `limit` bounds the damage if the reading is wrong
    anyway; finding no terminator masks nothing, exactly as the single-line scan does.
    """
    klass, col = False, index + 1
    for ahead in range(row, min(row + limit, len(lines))):
        line, base = lines[ahead], starts[ahead]
        col = col if ahead == row else 0
        while col < len(line):
            ch = line[col]
            if ch == "\\":
                col += 2
                continue
            if klass:
                klass = ch != "]"
            elif ch == "[":
                klass = True
            elif ch == "/":
                col += 1
                while col < len(line) and line[col] in "imxounse":
                    col += 1
                return base + col
            col += 1
    return None


def masked(text):
    """Offsets inside a comment, a string, a heredoc body, a percent literal or a regex.

    **Heredocs are hidden first, and that ordering is what lets a string span a newline.** The
    character walk used to forget an open quote at every newline, on the reasoning that an
    unbalanced quote would otherwise swallow the rest of the file — and the thing that produces
    an unbalanced quote is an apostrophe inside a heredoc body, which the walk was reading as
    code because the heredoc pass ran after it. Hiding heredocs first removes that source, and
    valid Ruby has no other: an unbalanced quote outside a comment or a heredoc is a syntax
    error.

    What the old ordering cost: lobsters writes a four-line SQL string inside `scope :moderators`,
    and `OR` in it was drawn as a **constant**. 17 positions over five corpora sat inside a
    multi-line string that way — the same family of bug as the unmasked `%w[]`, and found the
    same way, by a residue class that made no sense.

    **`#{...}` is scanned as code and masked as string.** It has to be *scanned* because a string
    inside an interpolation closes on its own quote and not on the outer one: reading
    `"a #{b.strftime("%Y")} c"` character by character, the fourth quote **ends** the literal and
    everything after it reads as code — which is how `%Y-%m-%d` came to be a drawn position. It
    is *masked* because this is a scanner and not a parser: an interpolation nests arbitrarily,
    and a scanner that pretended to read the code in one would misread it in some new way. The
    cost is that a real cursor inside an interpolation is never sampled, and that is a stated
    blind spot rather than a claim.
    """
    hidden = bytearray(len(text))
    lines = text.split("\n")
    starts, at = [], 0
    for line in lines:
        starts.append(at)
        at += len(line) + 1
    _block_comments(lines, starts, hidden)
    _heredocs(lines, starts, hidden)
    # `quote` is the delimiter being read, or None while reading code. `stack` holds the literals
    # a `#{` suspended, and `depth` the braces opened inside the innermost one — so the `}` that
    # resumes a string is told apart from the `}` of a hash written inside the interpolation.
    quote, stack, depth = None, [], 0
    for row, line in enumerate(lines):
        at, index = starts[row], 0
        while index < len(line):
            offset, ch = at + index, line[index]
            # A heredoc body is not code, so nothing in it opens a string or a comment.
            if hidden[offset] and quote is None and not stack:
                index += 1
                continue
            if quote is not None:
                hidden[offset] = 1
                if ch == "\\":
                    if index + 1 < len(line):
                        hidden[offset + 1] = 1
                    index += 2
                    continue
                if ch == "#" and quote != "'" and line[index + 1:index + 2] == "{":
                    hidden[offset + 1] = 1
                    stack.append((quote, depth))
                    quote, depth = None, 0
                    index += 2
                    continue
                if ch == quote:
                    quote = None
                index += 1
                continue
            if stack:
                hidden[offset] = 1
            # **Four of Ruby's punctuation globals are quote characters.** lobsters writes
            # `before, user, after = $`, $1, $'` — a backtick and an apostrophe, each of which
            # opened a literal the file never closes, and the remaining 27% of `markdowner.rb`
            # was masked as a string. They are read as the two-character tokens they are.
            if ch == "$" and line[index + 1:index + 2] in QUOTE_GLOBALS:
                hidden[offset] = hidden[offset + 1] = 1
                index += 2
                continue
            if ch in "\"'`":
                quote = ch
                hidden[offset] = 1
            # A `#` inside an interpolation starts a comment like any other, and it has to,
            # because the comment can hold an apostrophe: forem writes `# ... article's title`
            # inside a `#{I18n.t(` that spans five lines, and the whole tail of the file went
            # with it.
            elif ch == "#":
                for rest in range(index, len(line)):
                    hidden[at + rest] = 1
                break
            elif stack and ch == "{":
                depth += 1
            elif stack and ch == "}":
                if depth:
                    depth -= 1
                else:
                    quote, depth = stack.pop()
            elif ch == "%" and _percent_end(text, offset) is not None:
                stop = _percent_end(text, offset)
                for off in range(offset, min(stop, len(text))):
                    hidden[off] = 1
                index = min(stop - at, len(line))
                continue
            elif ch == "/" and _opens_regex(line, index):
                end = _regex_end(line, index)
                if end is not None:
                    for rest in range(index, end):
                        hidden[at + rest] = 1
                    index = end
                    continue
                if not line[index + 1:].strip():
                    stop = _regex_spans_lines(lines, starts, row, index)
                    if stop is not None:
                        for off in range(at + index, stop):
                            hidden[off] = 1
                        # The rest of the body is masked, and the guard at the top of this loop
                        # steps over a masked byte while reading code — so the walk resumes of
                        # its own accord on the first byte after the closing delimiter.
                        index = len(line)
                        continue
            index += 1
    return hidden


def line_starts(text):
    starts, at = [], 0
    for line in text.split("\n"):
        starts.append(at)
        at += len(line) + 1
    return starts


def at_offset(starts, offset):
    """The (line, column) one offset sits at, by binary search over `line_starts`.

    **Code points, not bytes**, because `text` is a decoded `str` everywhere in this package and
    both numbers are indices into one. `client.start` negotiates `utf-32` for that reason: its
    code unit is the code point, so a `column` is legal on the wire *and* legal as a slice of a
    decoded line. It said "byte offset" here until 2026-09-14 and the handshake believed it, which
    posed every cursor after a multi-byte character that many bytes too far left.
    """
    low, high = 0, len(starts) - 1
    while low < high:
        middle = (low + high + 1) // 2
        if starts[middle] <= offset:
            low = middle
        else:
            high = middle - 1
    return low, offset - starts[low]


def line_key(text, offset):
    """The ledger's key for one position: the sha256 of the line it sits on.

    A hash rather than the line itself, and that is a licence requirement rather than a
    preference: `audit/` is committed into an MIT repository and four of these corpora are
    copyleft, one with a proprietary subtree. The hash keeps the only property the text was
    there for — a position whose line changed is a position that must be re-adjudicated.
    """
    row, _ = at_offset(line_starts(text), offset)
    line = text.split("\n")[row]
    return hashlib.sha256(line.encode("utf-8")).hexdigest()[:16]

# ------------------------------------------------------------------------ the mask, self-checked
#
# One line each, with what `masked` must produce for it. Six bugs have been found in this scanner
# and every one of them was found by hand, from a residue class that made no sense — so the fix
# for each is written down here as the line that used to be read wrong. `audit sample --check`
# runs them, and runs the corpus-wide invariant beside them.
EXAMPLES = (
    ('x = "a #{h.at.strftime("%Y-%m")} b"',           'x = ...............................'),
    ('x = s.gsub(/[^A-Za-z0-9]/, "_")',               'x = s.gsub(.............., ...)'),
    ('if line =~ /Foo|Bar/',                          'if line =~ .........'),
    ('when /Admin/ then Bar',                         'when ....... then Bar'),
    ('total = count / Page::SIZE',                    'total = count / Page::SIZE'),
    ('x = (a) / (b) + Foo',                           'x = (a) / (b) + Foo'),
    ('URL = %r{https?://[^\\s"`]+}',                    'URL = .....................'),
    ('x = %w[Alpha Beta]',                            'x = ..............'),
    ('pct = done % total',                            'pct = done % total'),
    ('msg = "%05d" % count',                          'msg = ...... % count'),
    ('<%= link_to Foo, bar_path %>',                  '<%= link_to Foo, bar_path %>'),
    ("before, user, after = $`, $1, $'",              'before, user, after = .., $1, ..'),
    ("x = Foo # article's title",                     'x = Foo .................'),
    ('# cleared by expire_page_cache.',               '...............................'),
)

# **Four more, and they need more than one line because the bug each holds is a mask that ran
# *off* the line it began on.** Every one of them swallowed a discourse file whole: the walk lost
# its place, `keeps_place` went false, and every position in the rest of that file went undrawn.
# Found on 2026-09-14 by the invariant rather than by a residue class that made no sense, which is
# the first time round that way.
#
#   a commented-out heredoc    the terminator is commented out too, so the hunt for a bare `SQL`
#                              runs to the end of the file. Four of the six were migrations
#                              somebody had commented out.
#   a quoted opener            `emit "… <<~SQL"` is a generator writing Ruby, not a heredoc
#   `=begin` … `=end`          Ruby's other comment, which the walk did not know about at all —
#                              and prose in one holds the apostrophe that opens a string nothing
#                              closes. The third source of an unbalanced quote, where the module
#                              used to say there were two.
#   a mixed-case tag           `<<-EndOfTests` read under a screaming-case pattern matches the
#                              `E` alone, and then looks for a line reading `E` for ever
BLOCKS = (
    ("# DB.exec <<~SQL\n#   SELECT 1\n# SQL\nclass Foo\nend\n",
     "................\n............\n.....\nclass Foo\nend\n"),
    ('emit "  SQL = <<~SQL"\nemit "  x"\nclass Foo\nend\n',
     "emit ................\nemit .....\nclass Foo\nend\n"),
    ("=begin\nit doesn't close\n=end\nclass Foo\nend\n",
     "......\n................\n....\nclass Foo\nend\n"),
    ("src = <<-EndOfTests\n  require 'x'\nEndOfTests\nclass Foo\nend\n",
     "src = <<-EndOfTests\n.............\n..........\nclass Foo\nend\n"),
)


def shown(line):
    """One line with every masked byte replaced by a dot — how `EXAMPLES` is written."""
    hidden = masked(line)
    return "".join("." if hidden[at] else ch for at, ch in enumerate(line))


def check():
    """Every worked example that `masked` reads wrong, as `(source, wanted, got)`. Empty is right.

    Both lists, because a one-line example cannot hold a bug whose whole nature is running off
    the end of its line — which is every bug `BLOCKS` records.
    """
    return [(source, want, got) for source, want in EXAMPLES + BLOCKS
            if (got := shown(source)) != want]


def keeps_place(text):
    """Whether the walk still knows where it is at the end of the file.

    The last top-level `end` of a Ruby file is code by construction, so a mask that hides it is a
    mask that lost its place — which is the only way this scanner fails catastrophically, and the
    way it failed on a backtick inside a multi-line regex and on the post-match global. Files
    answer True, having nothing to say.
    """
    starts, lines = line_starts(text), text.split("\n")
    hidden = masked(text)
    for row in range(len(lines) - 1, -1, -1):
        if lines[row] == "end":
            return not hidden[starts[row]]
    return True
