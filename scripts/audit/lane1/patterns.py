"""What installs a method name in Ruby, read as text.

Shared by every key in this lane, and by `foreign`'s walk of the machine's gems. Each pattern exists
because a run got a name wrong without it, and its reason sits beside the regex: a filter whose
reason is gone is a filter the next person deletes.
"""

import re

DEF = re.compile(r"^[ \t]*def\s+(?:self\s*\.\s*)?([a-z_][A-Za-z0-9_]*[?!]?)", re.M)
# Everything that installs a method without writing `def`. A name any of these produces has more
# than one defensible target, so it is dropped, not adjudicated.
# - Group 1 is the macro's own name: `enum` installs four names per value, so it must be told apart.
# - Group 2 is the arguments.
MACRO = re.compile(
    r"^[ \t]*(has_many|has_one|belongs_to|has_and_belongs_to_many|scope|attr_accessor"
    r"|attr_reader|attr_writer|attr_internal|delegate|enum|attribute|alias_method|alias_attribute"
    r"|store_accessor|mattr_accessor|cattr_accessor|class_attribute|thread_mattr_accessor"
    r"|composed_of|serialize|normalizes|encrypts|helper_method|define_method|alias)\b(.*)$", re.M)
# A macro call whose argument list wraps. `MACRO` ends `(.*)$`, one line, so a `delegate(` followed
# by thirty symbols on thirty lines would let every one of those names escape the block list. Each
# would become "knowable", keyed to some unrelated `def` of the same name, and the correct answer
# would score wrong.
CONTINUES = re.compile(r"^[ \t]*[:'\"]")
# No trailing `\b`. With it, `:any_admin?,` captures `any_admin`: there is no word boundary between
# `?` and `,`, so the optional suffix backtracks away. The key then looks up `any_admin?` and never
# finds it, and every predicate a macro declares escapes the block list.
SYMBOL = re.compile(r"[:\"']([a-z_][A-Za-z0-9_]*[?!=]?)")
# **A macro name spelled as a hash key, which `SYMBOL` cannot see.**
# `enum status: { active: 0, archived: 1 }` installs `active?`, and the only colon in `active:` is
# the *trailing* one. Without this, `active?` becomes knowable, keyed to some `def active?` in a
# concern, and a server answering the model (where the enum put it) scores **wrong**. Same hole as
# the prefixed delegate: a macro installing a name no symbol spells.
HASH_KEY = re.compile(r"(?<![:\w])([a-z_][A-Za-z0-9_]*):(?!:)")
# What `enum` installs for each value, from `enums.rs`' own list. Blocking only the bare name would
# leave `active?` (the spelling the position is actually on) still keyed.
ENUM_SUFFIXES = ("", "?", "!")
COLUMN = re.compile(r"^\s*t\.\w+\s+[\"']([a-z_][A-Za-z0-9_]*)[\"']", re.M)
# What Rails installs for each column: the reader, the writer, and the query method, which Rails
# defines for **every** column, not only a boolean one. Without `?`, a column's predicate was keyed
# to an unrelated `def` of the same name, and a server answering the column scored wrong.
COLUMN_SUFFIXES = ("", "=", "?")
# The same columns, read from a database's own SQL dump, because `COLUMN` reads `schema.rb` and not
# every corpus has one. Otherwise a name that is both a column and a serializer `def` becomes
# scorable with the serializer as its only right answer, while a server answering `structure.sql`
# gives the other defensible answer this key promises not to adjudicate.
COLUMN_SQL = re.compile(r"^\s{4}([a-z_][A-Za-z0-9_]*)\s+[a-zA-Z]", re.M)

# **A delegate with `prefix:` installs a name written nowhere in the source.**
# `delegate :confirmed?, to: :user, prefix: true` installs `user_confirmed?`, so reading symbols out
# of the call never blocks it. If an unrelated class holds a `def user_confirmed?`, the key points
# there and scores the right answer wrong. Rare, and only findable by looking, which is why it is
# fixed rather than noted.
DELEGATE = re.compile(
    r"^[ \t]*delegate\b(.*?)(?=^[ \t]*(?:def|end|delegate|\w+\s+:|$))", re.S | re.M)
TO = re.compile(r"to:\s*:([a-z_][A-Za-z0-9_]*)")
PREFIX = re.compile(r"prefix:\s*(?:true|:([a-z_][A-Za-z0-9_]*))")


def macro_body(text, found):
    """A macro call's arguments, including the lines it wraps onto.

    Conservative on purpose: a continuation is a following line starting with a symbol or a string.
    A line starting with `:` is not plausibly an unrelated statement; one starting with a bare word
    (`to: :authorizer`, `end`) might be, and over-reading blocks names the key should score.
    """
    body = [found.group(2)]
    for line in text[found.end():].split("\n")[1:]:
        if not CONTINUES.match(line):
            break
        body.append(line)
    return "\n".join(body)


def prefixed(text):
    """Every `<target>_<method>` a `prefix:` delegate installs in one file."""
    out = set()
    for call in DELEGATE.finditer(text):
        body = call.group(1)
        if "prefix:" not in body:
            continue
        to, prefix = TO.search(body), PREFIX.search(body)
        if not to or not prefix:
            continue
        stem = prefix.group(1) or to.group(1)
        for name in SYMBOL.findall(body.split("to:")[0]):
            out.add(f"{stem}_{name}")
    return out
