"""The Rails key: the half the neutral key throws away, scored against a **set** of places.

`neutral` drops every macro- and column-declared name, for a sound reason: at `story.title`, a
server answering `db/schema.rb` and one answering `app/models/story.rb` are both defensible, so
exact-location equality cannot judge them. But *"the location is not scorable"* does not mean *"the
question is not asked"*. Three things stay scorable without naming one right place:

  **answered or silent**      recall needs no adjudication
  **admissible or wrong**     not one right place but a *set*; a name-based guess landing on an
                              unrelated class's `def title` is outside it
  **asserted or hedged**      a guess labelled as a guess differs from one presented as fact,
                              and the tier is exactly that distinction

The discarded half is large: from a fifth to over a third of sampled positions, depending on the
corpus, are macro- or column-derived, and on a Rails app it is where a developer spends the day.

**The receiver problem, and why this module is small.** At `story.title` no regex can prove `story`
is a `Story`, so nothing here infers a type. A position qualifies only where the corpus itself pins
the receiver: a **bare read inside an instance method of the model that declares the name**, where
`self` is that model by the language's rules. That shape is decidable from text. The draw does not
target it, so this key opens its own files and asks its own questions.

**The bias hazard, and how it is contained.** A key built from ya-lsp's Rails knowledge would grade
ya-lsp against itself. So:
- this module is written from the corpus text alone, never from `workspace/rails/`;
- the admissible set is **generous**: the schema, the model, its concerns, its superclass, or
  anywhere outside the repository (answering ActiveRecord's own `def id` is defensible);
- so precision is nearly free, and what this measures is **recall**. `wrong` is reserved for an
  unrelated file *in this repository*: the failure a name-based guesser has and a resolver does not.

ya-lsp was built to pass this scale. It is reported on its own line, never folded into the neutral
key's. Read it as "how big the discarded half is, and who answers it", not "who is better".
"""

import os
import random
import re
from pathlib import Path

from audit import site
from audit.answers import card_of, locations, path_of, tier
from audit.client import ask_all
from audit.ruby import masked

NAME = "rails"
FINDINGS = ("rails-wrong",)
TOTAL = "asked"
# This key poses its own questions, so it runs while the server is alive **and before `ask_rebased`
# edits the buffers**. See `lane1.asked`.
ASKS = True
# Positions per corpus. A corpus yields thousands after `build`'s own caps, at two requests each;
# 250 fits the budget's headroom. Drawn with the sample's seed, so this set moves only when the pin
# does.
WANTED = 250
METHODS = ("textDocument/hover", "textDocument/definition")

MODEL_DIR = "app/models"
BASES = ("ApplicationRecord", "ActiveRecord::Base")

CLASS = re.compile(
    r"^([ \t]*)class[ \t]+([A-Z][A-Za-z0-9_:]*)[ \t]*(?:<[ \t]*([A-Za-z0-9_:]+))?", re.M)
# `[ \t]`, never `\s`: `\s` matches the newline before the line, which puts the match one line early
# and leaves a `\n` in the captured indent. Then `indent + "end"` never matches, and the "body" runs
# to the end of the file.
INSTANCE_DEF = re.compile(
    r"^([ \t]+)def[ \t]+(?!self[ \t]*\.)([a-z_][A-Za-z0-9_]*[?!=]?)([^\n]*)$", re.M)
ANY_DEF = re.compile(r"^\s*def\b")
INCLUDE = re.compile(r"^[ \t]*include[ \t]+([A-Z][A-Za-z0-9_:]*)", re.M)

# Every macro whose names this key claims, and how the names are spelled in the call.
NAMED = re.compile(
    r"^[ \t]*(belongs_to|has_many|has_one|has_and_belongs_to_many|scope|enum|attribute"
    r"|attr_accessor|attr_reader|attr_writer|alias_attribute|store_accessor|composed_of"
    r"|delegate)\b([^\n]*)$", re.M)
SYMBOL = re.compile(r":([a-z_][A-Za-z0-9_]*[?!]?)")
# The macros that name **one** member, then say something else about it. `scope :recent, ->`
# declares `recent`; the symbols after it belong to the body.
FIRST_ONLY = ("scope", "enum", "attribute", "alias_attribute", "composed_of")

CREATE_RB = re.compile(r'^\s*create_table\s+[\'"]([a-z_0-9]+)[\'"](.*?)^\s*end', re.M | re.S)
COLUMN_RB = re.compile(r'^\s*t\.\w+\s+[\'"]([a-z_][A-Za-z0-9_]*)[\'"]', re.M)
CREATE_SQL = re.compile(r"CREATE TABLE (?:ONLY\s+)?(?:[a-z_]+\.)?([a-z_0-9]+)\s*\((.*?)^\);",
                        re.M | re.S)
COLUMN_SQL = re.compile(r"^\s{4}([a-z_][A-Za-z0-9_]*)\s+[a-zA-Z]", re.M)

WORD = re.compile(r"[a-z_][A-Za-z0-9_]*")
ASSIGNED = re.compile(r"^[ \t]*([a-z_][A-Za-z0-9_]*)\s*(?:\|\||&&)?=[^=~]", re.M)
BLOCK_PARAMS = re.compile(r"\|([^|\n]*)\|")
# `\??` takes `deleted_at?` whole, a name this key claims for every column (`build`). A `!` after
# the name still drops the match: that is another method, which this key does not claim.
BARE_READ = re.compile(r"(?<![.:@$\w])([a-z_][A-Za-z0-9_]*\??)(?![\w:(?!])")

IRREGULAR = {"person": "people", "man": "men", "woman": "women", "child": "children",
             "foot": "feet", "tooth": "teeth", "goose": "geese", "mouse": "mice",
             "datum": "data", "medium": "media", "analysis": "analyses", "status": "statuses"}


def underscore(name):
    name = name.split("::")[-1]
    name = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", name)
    name = re.sub(r"([a-z\d])([A-Z])", r"\1_\2", name)
    return name.lower()


def pluralize(word):
    """Enough of Rails' inflector for a model name. Wrong is safe: the table is not found and the
    model is skipped, which removes questions instead of inventing them.
    """
    if word in IRREGULAR:
        return IRREGULAR[word]
    for singular, plural in IRREGULAR.items():
        if word.endswith("_" + singular):
            return word[:-len(singular)] + plural
    if re.search(r"[^aeiou]y$", word):
        return word[:-1] + "ies"
    if word.endswith(("s", "x", "z", "ch", "sh")):
        return word + "es"
    if word.endswith("fe"):
        return word[:-2] + "ves"
    return word + "s"


def schema(corpus):
    """`{table: {column}}`, and the relative paths the dumps live at."""
    tables, files = {}, []
    # Anywhere, not only `db/` at the root: solidus is an engine monorepo with its dump in
    # `core/db/`. `db/migrate` is excluded, because a `create_table` in a migration is history, not
    # the schema.
    for here, dirs, names in os.walk(corpus.dir):
        dirs[:] = [d for d in dirs
                   if d not in (".git", "node_modules", "migrate", "tmp", "vendor")]
        if os.path.basename(here) != "db":
            continue
        for name in names:
            path = Path(here, name)
            relative = str(path.relative_to(corpus.dir))
            if "migrate" in relative:
                continue
            if not (name.endswith("schema.rb") or name.endswith(".sql")):
                continue
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            files.append(relative)
            if name.endswith("schema.rb"):
                for table, body in CREATE_RB.findall(text):
                    tables.setdefault(table, set()).update(COLUMN_RB.findall(body))
                    tables[table].add("id")
            else:
                for table, body in CREATE_SQL.findall(text):
                    tables.setdefault(table, set()).update(COLUMN_SQL.findall(body))
    return tables, files


def class_at(text):
    """The one class a file declares, or None.

    A file declaring two classes is skipped: which one a bare word belongs to would need a parser,
    and over-skipping only removes questions. Enclosing `module`s are fine, and must be: solidus
    nests every model as `module Spree; class Order`, and rejecting that would leave almost nothing
    to ask.
    """
    if len(CLASS.findall(text)) != 1:
        return None
    found = CLASS.search(text)
    if found.group(1) and re.search(r"^\s*(?:def|do)\b", text[:found.start()], re.M):
        return None
    return found.group(2), found.group(3)


def models(corpus):
    """`{class name: {"file", "super", "includes", "text"}}` for every model file in the corpus."""
    out = {}
    for here, dirs, names in os.walk(corpus.dir):
        dirs[:] = [d for d in dirs if d not in (".git", "node_modules", "tmp", "spec", "test")]
        parts = os.path.relpath(here, corpus.dir).replace(os.sep, "/")
        if MODEL_DIR not in parts:
            continue
        for name in names:
            if not name.endswith(".rb"):
                continue
            path = Path(here, name)
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            declared = class_at(text)
            if not declared:
                continue
            klass, parent = declared
            out[klass] = {"file": str(path.relative_to(corpus.dir)).replace(os.sep, "/"),
                          "super": parent, "includes": INCLUDE.findall(text), "text": text}
    return out


def macro_names(text):
    """`{name: macro}` for every member the Rails macros in one body install.

    Only the name itself and its writer, not the many further names `enum` or an association also
    installs. A narrow claim cannot be argued with.
    """
    out = {}
    for macro, rest in NAMED.findall(text):
        if macro == "delegate":
            # `delegate :a, :b, to: :owner`: the names come before `to:`, the target after.
            rest = rest.split(" to:")[0]
        names = SYMBOL.findall(rest)
        if macro in FIRST_ONLY:
            names = names[:1]
        for name in names:
            out.setdefault(name, macro)
    return out


def reads(path, text, names, places):
    """Bare reads of a declared name inside an instance method of the model body."""
    lines = text.split("\n")
    hidden = masked(text)
    starts, at = [], 0
    for line in lines:
        starts.append(at)
        at += len(line) + 1
    out = []
    # Every `def` in the file, so a body the indent scan cannot close (a one-line `def`, an unusual
    # indent) stops at the next `def` instead of swallowing the rest of the file. Without it, a
    # `def self.x(arg)` parameter can come back as a `belongs_to` read thirty lines below its
    # method.
    any_def = [row for row, line in enumerate(lines) if ANY_DEF.match(line)]
    for hit in INSTANCE_DEF.finditer(text):
        indent, signature = hit.group(1), hit.group(3)
        start = text[:hit.start()].count("\n")
        limit = next((row for row in any_def if row > start), len(lines))
        close = start + 1
        while close < limit and lines[close].rstrip() != indent + "end":
            close += 1
        body = "\n".join(lines[start + 1:close])
        # Parameters, locals and block parameters all shadow the macro. All three are cheap to see,
        # and all three are fatal.
        local = set(WORD.findall(signature))
        local |= set(ASSIGNED.findall(body))
        for params in BLOCK_PARAMS.findall(body):
            local |= set(WORD.findall(params))
        for row in range(start + 1, min(close, len(lines))):
            for word in BARE_READ.finditer(lines[row]):
                name = word.group(1)
                if name not in names or name in local:
                    continue
                if hidden[starts[row] + word.start(1)]:
                    continue                  # a comment, a string or a heredoc
                out.append((path, row, word.start(1), starts[row] + word.start(1),
                            name, names[name], places))
    return out


def build(corpus):
    """Every scorable Rails-derived position in the corpus.

    A row is `(path, line, column, offset, word, macro, admissible)`.
    - `admissible` is the set of repository-relative paths an answer may land in.
    - `offset` is carried because the ledger keys a position on the sha256 of its line
      (`ruby.line_key`), and a finding names it with `audit.site`.
    A cursor is produced only inside an **instance method** of the declaring model, where `self` is
    that model by the language's rules, with no inference.
    """
    tables, dumps = schema(corpus)
    found = models(corpus)
    rows = []
    for klass, info in found.items():
        text = info["text"]
        names = macro_names(text)
        for column in tables.get(pluralize(underscore(klass)), set()):
            names.setdefault(column, "column")
            # Rails' query method, which it defines for every column, not only a boolean one. The
            # admissible places are the column's own.
            names.setdefault(column + "?", "predicate")
        if not names:
            continue
        # Anything the model writes a `def` for is a plain method: the neutral key's scale, not this
        # one. Anything declared twice under two macros is dropped for the same reason.
        for hit in INSTANCE_DEF.finditer(text):
            names.pop(hit.group(2).rstrip("=?!"), None)
            names.pop(hit.group(2), None)
        places = {info["file"]} | set(dumps)
        for included in info["includes"]:
            for candidate in found.values():
                if candidate["file"].endswith("/" + underscore(included) + ".rb"):
                    places.add(candidate["file"])
            places.add(MODEL_DIR + "/concerns/"
                       + underscore(included).replace("::", "/") + ".rb")
        if info["super"] and info["super"] not in BASES:
            parent = found.get(info["super"])
            if parent:
                places.add(parent["file"])
        rows += reads(info["file"], text, names, frozenset(places))
    # One model reading `topic` in forty methods is one question asked forty times. Cap it: two per
    # name per file, twelve per file.
    seen, per_name, per_file, kept = set(), {}, {}, []
    for row in rows:
        path, line, column, word = row[0], row[1], row[2], row[4]
        if (path, line, column) in seen:
            continue
        seen.add((path, line, column))
        if per_name.get((path, word), 0) >= 2 or per_file.get(path, 0) >= 12:
            continue
        per_name[(path, word)] = per_name.get((path, word), 0) + 1
        per_file[path] = per_file.get(path, 0) + 1
        kept.append(row)
    return kept


def draw(corpus, seed, want=WANTED):
    """`want` of the corpus' Rails positions, seeded on the pin exactly as the sample is.

    Sorted by file after the draw: `ask_all` opens each document the first time it reaches it, so
    file order opens each file once. Only the asking order changes, not the draw.
    """
    rows = build(corpus)
    rng = random.Random(f"{seed}:{corpus.name}:{corpus.sha}:rails")
    rng.shuffle(rows)
    return sorted(rows[:want], key=lambda row: (row[0], row[1], row[2]))


def ask(corpus, client, seed, opened=None, drawn=None, answers=None):
    """Draw this key's own positions, ask them, and score the replies.

    `drawn` and `answers` are the run's own, and this key uses neither: Rails macro sites are not a
    shape the sample targets, so it draws its own.

    Runs while the server is alive and **before `ask_rebased`**. That pass inserts a line into every
    sampled document, so a model file the draw also reached would answer one line off. `opened` is
    the sample's own set of `didOpen`ed paths, shared so a file both reached is opened once (see
    `client.ask_all`).
    """
    drawn = draw(corpus, seed)
    counts = {"asked": 0, "answered": 0, "silent": 0, "single": 0, "single-admissible": 0,
              "admissible": 0, "outside": 0, "unplaced": 0, "wrong": 0,
              "card": 0, "hedged": 0, "by-macro": {}}
    findings = []
    if not drawn:
        return counts, findings
    # The seven-tuple `ask_all` reads. The first two fields are the sample's strata, which this key
    # does not use, so they carry the key's name.
    posed = [(NAME, macro, path, line, column, offset, word)
             for path, line, column, offset, word, macro, _ in drawn]
    answers = ask_all(client, corpus, posed, methods=METHODS, opened=opened)
    for index, (path, line, column, offset, word, macro, admissible) in enumerate(drawn):
        counts["asked"] += 1
        cell = counts["by-macro"].setdefault(macro, {"asked": 0, "answered": 0,
                                                     "admissible": 0, "wrong": 0})
        cell["asked"] += 1
        card = card_of(answers.get((index, "textDocument/hover")))
        rung = tier(card)
        if card:
            counts["card"] += 1
            # The tier *is* the hedge. A Guessed card says it is one; any other card asserts. Reading `answers.tier`, not a list of hedge strings, keeps one
            # definition of the tier in this harness.
            if rung != "resolved":
                counts["hedged"] += 1
        found = locations(answers.get((index, "textDocument/definition")))
        if not found:
            counts["silent"] += 1
            continue
        counts["answered"] += 1
        cell["answered"] += 1
        # "Some target was admissible" is too generous alone: forty name-based guesses, one of them
        # the model file, would pass. `single` asks the other half: did the server name *one* place?
        if len(found) == 1:
            counts["single"] += 1
        verdicts = set()
        for target, _ in found:
            verdicts.add(place(corpus, target, admissible))
        # One verdict per position, and admissible wins: where *any* target is defensible, the
        # server did not guess wrong.
        for verdict in ("admissible", "outside", "unplaced", "wrong"):
            if verdict not in verdicts:
                continue
            counts[verdict] += 1
            if verdict == "wrong":
                cell["wrong"] += 1
                # Recorded, not just counted. `wrong` is the one verdict here that names a defect,
                # not a gap, so it must be reportable at the position.
                got = [Path(path_of(target) or target).name for target, _ in found[:3]]
                # **The tier goes in the line, because they are different bugs.**
                # - A Guessed card landing on an unrelated file is the name rung doing what its
                #   label says.
                # - A **Resolved** card landing there is the top tier claiming the code names a
                #   place it does not.
                # Both are `wrong` here; only the second is a broken promise.
                findings.append(("rails-wrong", site(path, offset),
                                 f"[{rung}] {macro} `{word}` at {path}:{line + 1} -> "
                                 f"{', '.join(got)}"
                                 + (f" (+{len(found) - 3} more)" if len(found) > 3 else "")))
            else:
                cell["admissible"] += 1
                if len(found) == 1:
                    counts["single-admissible"] += 1
            break
    return counts, findings


def place(corpus, target, admissible):
    """One target, in this scale's vocabulary.

    `outside` is the repository's own boundary, and it is **admissible on purpose**: answering
    ActiveRecord's `def id` at `story.id` is defensible. Calling it wrong would assert that the
    generated declaration is the only right answer, which is the adjudication this scale exists to
    refuse.
    """
    path = path_of(target)
    if path is None:
        return "unplaced"            # a declaration ya-lsp wrote and declines to place
    try:
        relative = Path(path).resolve().relative_to(Path(corpus.dir).resolve())
    except ValueError:
        return "outside"             # a gem, or Ruby's own library
    return "admissible" if str(relative).replace(os.sep, "/") in admissible else "wrong"


def line(counts):
    return (f"{counts['answered']} answered ({counts['single']} single), "
            f"{counts['admissible'] + counts['outside'] + counts['unplaced']} admissible, "
            f"{counts['wrong']} wrong, of {counts['asked']} asked; "
            f"{counts['hedged']} hedged of {counts['card']} cards")


summary = line


def under(counts):
    """Per macro, because the scale's claim is about *which* Rails words get answered. A total hides
    an association family answered nowhere behind the columns that are.
    """
    return [f"{macro:9} {cell['answered']} answered, {cell['admissible']} admissible, "
            f"{cell['wrong']} wrong, of {cell['asked']}"
            for macro, cell in sorted(counts["by-macro"].items(),
                                      key=lambda kv: -kv[1]["asked"])]
