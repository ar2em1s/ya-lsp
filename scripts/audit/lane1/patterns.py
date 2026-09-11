"""What installs a method name in Ruby, read as text.

Shared by every key in this lane, and by `foreign`'s walk of the machine's gems. Each pattern is
here because a run got a name wrong without it, and the reason travels with the regex rather than
being summarised elsewhere — a filter whose reason is gone is a filter the next person deletes.
"""

import re

DEF = re.compile(r"^[ \t]*def\s+(?:self\s*\.\s*)?([a-z_][A-Za-z0-9_]*[?!]?)", re.M)
# Everything that installs a method without writing `def`. A name any of these produces has more
# than one defensible target, so it is dropped rather than adjudicated. Group 1 is the macro's
# own name — `enum` installs four names per value and has to be told apart — and group 2 the
# arguments.
MACRO = re.compile(
    r"^[ \t]*(has_many|has_one|belongs_to|has_and_belongs_to_many|scope|attr_accessor"
    r"|attr_reader|attr_writer|attr_internal|delegate|enum|attribute|alias_method|alias_attribute"
    r"|store_accessor|mattr_accessor|cattr_accessor|class_attribute|thread_mattr_accessor"
    r"|composed_of|serialize|normalizes|encrypts|helper_method|define_method|alias)\b(.*)$", re.M)
# A macro call whose argument list wraps. `MACRO` ends `(.*)$`, which is one line, and forem
# writes `delegate(` followed by thirty symbols on thirty lines — so every one of those names
# escaped the block list and became "knowable" with the `def` in `Authorizer` as its one right
# answer, scoring the correct answer wrong 11 times in 14.
CONTINUES = re.compile(r"^[ \t]*[:'\"]")
# No trailing `\b`. With it, `:any_admin?,` captures `any_admin` — there is no word boundary
# between `?` and `,`, so the optional suffix backtracks away and the `?` is dropped, while the
# key looks the name up as `any_admin?` and never finds it. Every predicate a macro declares
# escaped the block list that way, in every corpus.
SYMBOL = re.compile(r"[:\"']([a-z_][A-Za-z0-9_]*[?!=]?)")
# **A macro name spelled as a hash key, which `SYMBOL` cannot see.** `enum role: { agent: 0,
# administrator: 1 }` installs `agent?` on `AccountUser`, and the only colon in `agent:` is the
# *trailing* one — so the block list missed it, `agent?` became knowable with the single
# `def agent?` in a concern as its one right answer, and ya-lsp answering `account_user.rb`
# (which is where the enum put it) was scored **wrong**. Same shape of hole as the prefixed
# delegate: a macro installing a name that is not written as a symbol anywhere.
HASH_KEY = re.compile(r"(?<![:\w])([a-z_][A-Za-z0-9_]*):(?!:)")
# What `enum` installs for each of its values, from `enums.rs`' own list. Blocking only the bare
# name would leave `agent?` — the spelling the position was actually on — still keyed.
ENUM_SUFFIXES = ("", "?", "!")
COLUMN = re.compile(r"^\s*t\.\w+\s+[\"']([a-z_][A-Za-z0-9_]*)[\"']", re.M)
# The same columns out of a database's own dump, because `COLUMN` reads `schema.rb` and not
# every corpus has one. A name that is both a column and a `def` in a serializer would otherwise
# become scorable with the serializer as its only right answer, and a server answering
# `structure.sql` is giving the other defensible answer this key promises not to adjudicate.
COLUMN_SQL = re.compile(r"^\s{4}([a-z_][A-Za-z0-9_]*)\s+[a-zA-Z]", re.M)

# **The mis-key the port of `truth.py` exists to fix.** `delegate :confirmed?, to: :user,
# prefix: true` installs `user_confirmed?` — a name that appears **nowhere in the source**, so
# reading the symbols out of the macro call never blocks it. If some unrelated class happens to
# hold a `def user_confirmed?`, the key points there and scores the right answer wrong. Measured
# before the fix at 4 entries of 1,488 over the five corpora: small, and small in a way that is
# only knowable by looking, which is why it is fixed rather than noted.
DELEGATE = re.compile(
    r"^[ \t]*delegate\b(.*?)(?=^[ \t]*(?:def|end|delegate|\w+\s+:|$))", re.S | re.M)
TO = re.compile(r"to:\s*:([a-z_][A-Za-z0-9_]*)")
PREFIX = re.compile(r"prefix:\s*(?:true|:([a-z_][A-Za-z0-9_]*))")


def macro_body(text, found):
    """A macro call's arguments, including the lines it wraps onto.

    Conservative on purpose: a continuation is a following line beginning with a symbol or a
    string, because a line starting with `:` is not plausibly an unrelated statement while one
    starting with a bare word (`to: :authorizer`, `end`) might be — and over-reading here blocks
    names the key should be scoring.
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
