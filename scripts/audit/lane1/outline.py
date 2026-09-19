"""The outline key: `documentSymbol` against Ruby's own parse of the same file.

Lane 1, because the repository states the answer: what a file declares is not an opinion, and Prism
says so. Every `class`, `module`, `def`, constant, `attr_*`, `alias` and `class << self` Prism sees
in a sampled file must be a row in the outline, and a row of any other kind breaks the "nothing
else" half.

**Use the Prism the server links.** `ruby -rprism` should be the version `Cargo.toml` pins
`ruby-prism` to; then a disagreement is about what ya-lsp did with the tree, not which tree it saw.
One Ruby process per corpus reads every sampled file; one per file would cost more than the
comparison.

**Per file, not per position**: this key's denominator is a file. It asks
`textDocument/documentSymbol` once per file the draw reached, which costs almost nothing.

# What is compared, and what is not

By **kind, name and line**, both ways:
- a declaration Prism saw and the outline lacks is `outline-missing`;
- an outline row Prism did not see is `outline-extra`.
Nesting is not compared: the tree is `parent_of`'s business, built from rubydex's own lexical
nesting, and a second opinion on it needs its own key.

**Ruby files only.** Templates are left out on both sides: Prism cannot parse raw ERB, and feeding
it the server's Ruby view would mean reimplementing `erb::ruby_view`. A template's outline is
`symbols.rs`' own question.

Dropped on both sides, because the two spellings are the same fact:

  - `private :foo`: a visibility statement, which `symbols.rs` excludes as not a definition.
  - an instance, class or global variable: rubydex records one per assignment.
  - a name Prism recovered from a half-typed `def`, whose name span is whitespace.
"""

import json
import subprocess

from audit import site
from audit.client import open_document, uri
from audit.ruby import line_starts

NAME = "outline"
ASKS = True
TOTAL = "outline-rows"
FINDINGS = ("outline-missing", "outline-extra")

# `documentSymbol`'s `SymbolKind` numbers: the three `symbols::kind_of` can answer with, plus the
# two namespace kinds. Read as names on both sides, so a kind disagreement reads as one, not as two
# integers.
KINDS = {2: "module", 5: "class", 6: "method", 7: "property", 14: "constant"}

# Ruby's half. Prints one JSON object per line: {"path": …, "rows": [[kind, name, line], …]}.
#
# Written in Ruby, not scanned in Python, on purpose: a regex over Ruby source would be a second
# implementation of the thing under test, and a *key*'s answer must come from something that is not
# ya-lsp.
PARSE = r"""
require "prism"
require "json"

def simple(name)
  name.to_s.split("::").last.to_s
end

# `scope` is the enclosing class or module name, which one row needs: rubydex names a singleton
# class after what it is the singleton *of*, so `class << self` is listed as `<< Person`, and the
# word `self` never reaches the outline.
def rows_of(node, rows, scope = nil)
  return if node.nil?
  inner = scope
  case node
  when Prism::ClassNode
    inner = simple(node.constant_path.slice)
    rows << ["class", inner, node.constant_path.location.start_line - 1]
  when Prism::ModuleNode
    inner = simple(node.constant_path.slice)
    rows << ["module", inner, node.constant_path.location.start_line - 1]
  when Prism::SingletonClassNode
    of = node.expression.is_a?(Prism::SelfNode) ? scope.to_s : simple(node.expression.slice)
    rows << ["class", "<< #{of}", node.expression.location.start_line - 1]
  when Prism::DefNode
    prefix = case node.receiver
             when nil then ""
             when Prism::SelfNode then "self."
             when Prism::ConstantReadNode, Prism::ConstantPathNode then "#{node.receiver.slice}."
             else ""
             end
    rows << ["method", "#{prefix}#{node.name}", node.name_loc.start_line - 1] unless
      node.name.to_s.strip.empty?
  when Prism::AliasMethodNode
    rows << ["method", simple(node.new_name.slice.sub(":", "")),
             node.new_name.location.start_line - 1]
  when Prism::ConstantWriteNode
    rows << ["constant", node.name.to_s, node.name_loc.start_line - 1]
  when Prism::ConstantPathWriteNode
    rows << ["constant", simple(node.target.slice), node.target.location.start_line - 1]
  when Prism::CallNode
    arguments = node.arguments&.arguments || []
    if node.receiver.nil? && %w[attr_reader attr_writer attr_accessor].include?(node.name.to_s)
      arguments.each do |argument|
        next unless argument.is_a?(Prism::SymbolNode)
        rows << ["property", argument.unescaped.to_s, argument.location.start_line - 1]
      end
    end
    # `alias_method :new, :old` installs `new`, and it is a *call*, not the `alias` keyword.
    if node.receiver.nil? && node.name.to_s == "alias_method" &&
       arguments.first.is_a?(Prism::SymbolNode)
      rows << ["method", arguments.first.unescaped.to_s,
               arguments.first.location.start_line - 1]
    end
  end
  node.compact_child_nodes.each { |child| rows_of(child, rows, inner) }
end

$stdin.read.split("\n").each do |path|
  next if path.empty?
  rows = []
  begin
    rows_of(Prism.parse_file(path).value, rows)
  rescue StandardError => error
    puts({ path: path, error: error.message }.to_json)
    next
  end
  puts({ path: path, rows: rows }.to_json)
end
"""


def counters():
    return {"outline-files": 0, "outline-rows": 0, "outline-seen": 0,
            "outline-missing": 0, "outline-extra": 0, "outline-anonymous": 0,
            "outline-kinds": {}, "outline-unparsed": 0}


def ask(corpus, client, seed, opened=None, drawn=None, answers=None):
    counts = counters()
    findings = []
    files = sorted({row[2] for row in (drawn or []) if row[2].endswith(".rb")})
    if not files:
        return counts, findings
    opened = set() if opened is None else opened
    asked = {}
    for path in files:
        if path not in opened:
            open_document(client, corpus, path)
            opened.add(path)
        client.post(path, "textDocument/documentSymbol",
                    {"textDocument": {"uri": uri(corpus.dir / path)}})
        for key, result in client.drain(down_to=32):
            asked[key] = result
    for key, result in client.drain():
        asked[key] = result

    for path, rows in _parsed(corpus, files).items():
        counts["outline-files"] += 1
        if rows is None:
            counts["outline-unparsed"] += 1
            continue
        listed = _listed(asked.get(path))
        counts["outline-rows"] += len(rows)
        counts["outline-seen"] += len(listed)
        for kind, name, line in rows:
            counts["outline-kinds"][kind] = counts["outline-kinds"].get(kind, 0) + 1
        _compare(corpus, path, rows, listed, counts, findings)
    return counts, findings


def _parsed(corpus, files):
    """Prism's reading of every sampled file, as {path: [(kind, name, line)]}, or None for a file it
    could not parse. A missing `ruby` gives an empty answer and `TOTAL` prints nothing: the honest
    outcome for a key missing its other half.
    """
    try:
        done = subprocess.run(["ruby", "-e", PARSE], text=True, capture_output=True,
                              input="\n".join(str(corpus.dir / path) for path in files))
    except OSError:
        return {}
    root = str(corpus.dir) + "/"
    out = {}
    for line in (done.stdout or "").split("\n"):
        if not line.strip():
            continue
        try:
            said = json.loads(line)
        except ValueError:
            continue
        path = str(said.get("path", "")).removeprefix(root)
        out[path] = None if "error" in said else [tuple(row) for row in said.get("rows") or []]
    return out


def _listed(reply):
    """Every row of a `documentSymbol` reply, flattened, as (kind, name, line).

    **Both reply shapes; this harness gets the flat one.** `documentSymbol` answers a nested
    `DocumentSymbol[]` to a client that says it can read one, and a flat `SymbolInformation[]`
    otherwise (`symbols.rs` does both). The `initialize` here declares neither, so the reply is
    flat: the span is under `location.range`, with no `children` to walk. Reading only the nested
    shape would put every row at line 1.
    """
    out = []
    for symbol in reply if isinstance(reply, list) else []:
        kind = KINDS.get(symbol.get("kind"))
        where = (symbol.get("selectionRange") or (symbol.get("location") or {}).get("range")
                 or symbol.get("range") or {})
        if kind:
            out.append((kind, str(symbol.get("name", "")),
                        (where.get("start") or {}).get("line", 0)))
        out.extend(_listed(symbol.get("children")))
    return out


def _compare(corpus, path, rows, listed, counts, findings):
    """Both ways, on (kind, name, line). A duplicate on one side is a row on that side.

    A finding is keyed on `audit.site` (`path:offset`), so a row's line is turned back into an
    offset against the file. Lane 3 subtracts findings from the draw by that string, and a
    `path:line` of its own would collide with one.
    """
    from collections import Counter

    try:
        starts = line_starts((corpus.dir / path).read_text(encoding="utf-8", errors="replace"))
    except OSError:
        starts = [0]

    def at(row):
        return site(path, starts[row[2]] if row[2] < len(starts) else 0)

    seen, said = Counter(rows), Counter(listed)
    for row, count in (seen - said).items():
        counts["outline-missing"] += count
        findings.append(("outline-missing", at(row),
                         f"{row[0]} `{row[1]}` at {path}:{row[2] + 1} is not in the outline"))
    for row, count in (said - seen).items():
        if _anonymous(row) or _singleton_copy(row, seen):
            counts["outline-anonymous"] += count
            continue
        counts["outline-extra"] += count
        findings.append(("outline-extra", at(row),
                         f"{row[0]} `{row[1]}` at {path}:{row[2] + 1} is in the outline and not "
                         f"in the file"))


def _singleton_copy(row, rows):
    """A `self.foo` row beside a plain `foo` on the same line: `module_function`.

    `module_function` makes every `def` below it both an instance method **and** a singleton one,
    and rubydex records both, so the outline has two rows where Prism's tree has one `def`.
    Modelling it on the Prism side would mean tracking a second kind of section fence, which is a
    parser. The row is legal, so it is counted apart, with the reason here.
    """
    kind, name, line = row
    return kind == "method" and name.startswith("self.") and \
        (kind, name.removeprefix("self."), line) in rows


def _anonymous(row):
    """A body with no name of its own: a call in Prism's tree, and the thing it made in the outline.

    `Module.new do … end` declares a module at that line. rubydex records the body as a definition,
    and `render::qualified_name` names the row after the call, because a row keyed by rubydex's own
    number would read `<0>`. Prism sees a `CallNode`, and this key has no business teaching it
    otherwise. So the row is legal, and counted apart instead of filtered.
    """
    return row[0] in ("class", "module") and row[1].endswith(".new")


def line(counts):
    return (f"{counts['outline-missing']} of {counts['outline-rows']} declarations missing from "
            f"the outline, {counts['outline-extra']} of {counts['outline-seen']} rows not "
            f"declared, over {counts['outline-files']} files")


summary = line


def under(counts):
    said = []
    kinds = "  ".join(f"{name} {count}" for name, count in
                      sorted(counts["outline-kinds"].items(), key=lambda kv: -kv[1]))
    if kinds:
        said.append(f"by kind   {kinds}")
    if counts["outline-anonymous"]:
        said.append(f"anonymous {counts['outline-anonymous']} rows for a body with no name of "
                    f"its own and singleton copies `module_function` made — counted, not "
                    f"reported")
    if counts["outline-unparsed"]:
        said.append(f"unparsed  {counts['outline-unparsed']} files Prism refused")
    return said
