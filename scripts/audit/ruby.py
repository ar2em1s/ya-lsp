"""Which characters of a file are Ruby a cursor could sit in, and where the lines start.

A cheap scan, not a parser, and that is enough: it runs before the server sees the file, so whatever
it misclassifies, it misclassifies for every position equally. The heredoc is the one case it must
handle.
"""

import hashlib
import re

ERB_TAG = re.compile(r"<%(?!%)[-=#]?(.*?)[-]?%>", re.S)
# **Two forms, and they are not equally safe to widen.**
# - `<<-TAG` and `<<~TAG` are always heredocs (nothing else spells a dash or tilde there), so the
#   tag may be any identifier. `<<-EndOfTests` needs that: under the screaming-case rule it matches
#   `E` alone, and the hunt for a line reading `E` runs to the end of the file.
# - A **bare** `<<TAG` collides with `arr <<item`, the push operator without a space, so it keeps
#   the screaming-case convention.
# Too narrow is worse than too wide: a partial match still opens a heredoc, then never closes it.
HEREDOC = re.compile(r"<<[-~][\"']?([A-Za-z_][A-Za-z0-9_]*)[\"']?"
                     r"|<<[\"']?([A-Z_][A-Z0-9_]*)[\"']?")


def ruby_regions(text, is_erb):
    """The ranges that hold Ruby: a whole `.rb` file, or only the tags of a template."""
    if not is_erb:
        return [(0, len(text))]
    out = []
    for found in ERB_TAG.finditer(text):
        if found.group(0).startswith("<%#"):
            continue
        out.append((found.start(1), found.end(1)))
    return out


def _block_comments(lines, starts, hidden):
    """Hide `=begin` … `=end`, Ruby's other comment and the walk's third source of an unbalanced
    quote.

    **Why:** the character walk assumes valid Ruby has no unbalanced quote outside a comment or a
    heredoc. A `=begin` block holds prose, and one `doesn't` in it opens a string the walk never
    closes. The walk then loses its place and draws nothing from the rest of the file.

    **First of the three passes, before heredocs**, because a `<<~SQL` inside a block comment is not
    an opener either.

    Ruby's own rule, exactly: `=begin` and `=end` count only at column 0, so `startswith` is the
    whole test.
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
    """Which columns of **one line, read alone**, are code: outside a quote it opened, and before a
    `#` outside a quote.

    Single-line and approximate on purpose. Its one caller decides whether a `<<~TAG` on this line
    opens a heredoc, and that runs before anything knows where the multi-line literals are (the
    ordering `masked` is built on). It must get two shapes right, because each costs a whole file:

      `# DB.exec <<~SQL`        a commented-out heredoc: its terminator is commented out too,
                                so the search for a bare `SQL` runs to the end of the file
      `emit "SQL = <<~SQL"`     an opener quoted inside a string: a code generator writing Ruby,
                                not a heredoc

    A bare regex reads both as openers.
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
    """Hide every heredoc body, because a heredoc does not misclassify symmetrically.

    A heredoc is where SQL lives, and SQL is full of Ruby-shaped words: `SELECT x AS parent_path`
    inside a `<<~SQL` reads to the scan as a Rails route helper. A few percent of all candidate
    positions sit inside one.

    **An opener has to be code, and `_code_columns` decides that.** A bare pattern search reads a
    commented-out `# DB.exec <<~SQL` or a quoted `emit "… <<~SQL"` as an opener, hunts for a
    terminator that is commented out or absent, and hides the rest of the file.
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


# A percent literal. The letter is required for every form but the operand-position one.
#
# `%w[a b]` is an array of strings, `%i[a b]` of symbols, `%q()`/`%Q{}`/`%()` a string and `%r{}` a
# regex. The words inside all of them scan as Ruby identifiers and are nothing of the kind.
PERCENT = re.compile(r"%([wWiIqQrsxX])?([^\sA-Za-z0-9])")
# Where a bare `%` may start a literal instead of being modulo.
#
# `a % b` and `"%s" % x` are the ordinary case, and hiding either would blank real code. So the bare
# form counts only in **operand position**: at the start of a line, or after an opener or an
# operator. After an identifier, a number or a closing bracket, it is modulo.
OPERAND = "([{,=|&!~<>+-*/:;?\n\t "
PAIRS = {"(": ")", "[": "]", "{": "}", "<": ">"}


def _percent_end(text, start):
    """Where the percent literal at `start` ends, delimiters included, or None if it is not one.

    Called from the character walk while it reads **code**, the only place the question has an
    answer: a `%` inside a comment, a string or a heredoc body is a character, and `a % b` is
    modulo. The walk already knows where it stands, so this asks only:
    - whether a letter names a form;
    - whether a bare `%` is in **operand position**.

    **It must run inside the walk, not as a pass after it.** The walk reaches the literal first, and
    a regex like `%r{[^"]+}` holds a `"` that would open a string running to the end of the file.
    """
    found = PERCENT.match(text, start)
    if not found:
        return None
    letter, opener = found.group(1), found.group(2)
    # **An ERB tag is not a percent literal**, though both its delimiters look like one: `%>` is a
    # `%` opened on `>`, and `<%=` a `%` opened on `=`. Without this guard the first blanks
    # everything up to the next `>` in the markup, and the `views` stratum loses about half its
    # positions.
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


# **A `/` opens a regex in operand position and is division everywhere else.**
#
# `a / b` and `s.gsub(/,/, "")` differ only in what precedes the slash. So the scan reads back over
# spaces and asks what it lands on:
# - an opener or an operator: a regex;
# - an identifier, a number or a closing bracket: division;
# - a keyword: a regex (`when /re/`), the one word-shaped exception.
# Left unmasked, a character class reads as code: `/[^A-Za-z0-9]/` offers `A` and `Z` as
# **constants**.
REGEX_OPENS = set("([{,=~!|&<>+-*/%;:?")
REGEX_WORDS = frozenset({"when", "and", "or", "not", "if", "unless", "while", "until",
                         "return", "case", "then", "do", "else", "elsif", "begin", "in",
                         "yield", "puts", "match", "split", "gsub", "sub", "scan", "grep"})
WORD_END = re.compile(r"[A-Za-z_][A-Za-z0-9_]*$")
# `$\`` and `$'` are the pre- and post-match globals; `$"` is the loaded-feature list and `$/` the
# input separator. Each is spelled with a character that opens a literal.
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

    Not on this line means **not masked**. An `/x` regex may span lines, but a scan that guessed
    where one ends would swallow the rest of the file, like an unbalanced quote. So an unterminated
    slash reads as the division it most likely is.
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

    A slash that ends its line cannot be division, because division needs a right operand. So this
    is the one case where scanning forward over newlines is safe, and it must be handled: forem
    writes a ten-line `/mx` regex whose character class holds a **backtick**, which the walk would
    read as an opening command literal and mask the rest of the file.
    - `limit` bounds the damage if the reading is wrong.
    - Finding no terminator masks nothing, like the single-line scan.
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

    **Heredocs are hidden first, and that ordering lets a string span a newline.** Valid Ruby has an
    unbalanced quote only inside a comment or a heredoc (an apostrophe in a heredoc body is the
    usual one). With heredocs already hidden, the walk can carry an open quote across newlines, so a
    multi-line SQL string in a `scope` is masked instead of offering `OR` as a **constant**.

    **`#{...}` is scanned as code and masked as string.**
    - *Scanned*, because a string inside an interpolation closes on its own quote, not the outer
      one. Read naively, `"a #{b.strftime("%Y")} c"` ends at its fourth quote, and `%Y-%m-%d`
      becomes a drawn position.
    - *Masked*, because this is a scanner, not a parser: an interpolation nests arbitrarily, and
      pretending to read the code in one would misread it some new way.
    The cost is a stated blind spot: a cursor inside an interpolation is never sampled.
    """
    hidden = bytearray(len(text))
    lines = text.split("\n")
    starts, at = [], 0
    for line in lines:
        starts.append(at)
        at += len(line) + 1
    _block_comments(lines, starts, hidden)
    _heredocs(lines, starts, hidden)
    # - `quote`: the delimiter being read, or None while reading code.
    # - `stack`: the literals a `#{` suspended.
    # - `depth`: the braces opened inside the innermost `#{`, so the `}` that resumes a string is
    #   told apart from a hash's `}` inside the interpolation.
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
            # **Four of Ruby's punctuation globals are quote characters.** In ``a, b = $`, $'`` the
            # backtick and the apostrophe would each open a literal the file never closes. They are
            # read as the two-character tokens they are.
            if ch == "$" and line[index + 1:index + 2] in QUOTE_GLOBALS:
                hidden[offset] = hidden[offset + 1] = 1
                index += 2
                continue
            if ch in "\"'`":
                quote = ch
                hidden[offset] = 1
            # A `#` inside an interpolation starts a comment like any other. It must, because the
            # comment can hold an apostrophe (`# article's title`), even inside a multi-line `#{`.
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
                        # steps over masked characters while reading code, so the walk resumes by
                        # itself right after the closing delimiter.
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

    **Code points, not bytes.** `text` is a decoded `str` everywhere in this package, and both
    numbers index into one. `client.start` negotiates `utf-32` because its code unit is the code
    point, so a `column` is legal on the wire *and* as a slice of a decoded line.
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

    A hash, not the line, as a licence requirement: `audit/` is committed into an MIT repository,
    four of these corpora are copyleft, and one has a proprietary subtree. The hash keeps the one
    property the text was there for: a position whose line changed must be re-adjudicated.
    """
    row, _ = at_offset(line_starts(text), offset)
    line = text.split("\n")[row]
    return hashlib.sha256(line.encode("utf-8")).hexdigest()[:16]

# ------------------------------------------------------------------------ the mask, self-checked
#
# One line each, with what `masked` must produce for it. Every scanner bug becomes a row here,
# written as the line it used to read wrong. `audit sample --check` runs them, beside the
# corpus-wide invariant.
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

# **Multi-line cases: each bug here is a mask that ran *off* the line it began on.** Each one loses
# the walk its place, so `keeps_place` goes false and the rest of the file goes undrawn.
#
#   a commented-out heredoc    the terminator is commented out too, so the hunt for a bare `SQL`
#                              runs to the end of the file
#   a quoted opener            `emit "… <<~SQL"` is a generator writing Ruby, not a heredoc
#   `=begin` … `=end`          Ruby's other comment: its prose holds the apostrophe that opens
#                              a string nothing closes
#   a mixed-case tag           `<<-EndOfTests` under a screaming-case pattern matches `E` alone,
#                              then looks for a line reading `E` for ever
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
    """One line with every masked character replaced by a dot: how `EXAMPLES` is written."""
    hidden = masked(line)
    return "".join("." if hidden[at] else ch for at, ch in enumerate(line))


def check():
    """Every worked example that `masked` reads wrong, as `(source, wanted, got)`. Empty is right.

    Both lists, because a one-line example cannot hold a bug that runs off the end of its line,
    which is every bug in `BLOCKS`.
    """
    return [(source, want, got) for source, want in EXAMPLES + BLOCKS
            if (got := shown(source)) != want]


def keeps_place(text):
    """Whether the walk still knows where it is at the end of the file.

    A Ruby file's last top-level `end` is code by construction, so a mask that hides it lost its
    place. That is the scanner's one catastrophic failure. A file with no top-level `end` (a
    template) answers True.
    """
    starts, lines = line_starts(text), text.split("\n")
    hidden = masked(text)
    for row in range(len(lines) - 1, -1, -1):
        if lines[row] == "end":
            return not hidden[starts[row]]
    return True
