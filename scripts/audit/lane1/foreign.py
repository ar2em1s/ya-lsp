"""Every method name defined by something that is **not** one of the corpora.

Machine state, cached, and the one input to this lane that is not a function of the pins.
"""

import glob
import json
import os
from pathlib import Path

from audit.config import ROOT
from audit.lane1 import patterns

CACHE = ROOT / "tmp" / "audit-foreign.json"


def foreign_names():
    """Every method name defined by a gem or by Ruby's own library, on this machine.

    The length-and-underscore heuristic in `neutral.knowable` is not enough on its own, and
    lobsters says why: `valid?` is six characters with a `?`, and the application really does
    define `def valid?` exactly once — in `app/models/short_id.rb`. So does ActiveModel, which is
    the right answer at every call site the sample reached, and a key built on the first fact
    scores the right answer wrong.

    The fix is to ask the machine rather than to lengthen the heuristic: a name **anything else**
    defines is not a name this corpus can be the answer key for. It over-blocks — some gem on
    the disk defines `find_by_url` and every corpus loses that position — and over-blocking is
    the safe direction **here**, because in a key it only ever removes a question. It is not
    safe in a filter on the draw, which is why `routes.non_helpers` does not consult this.

    **It is machine state, and that is a caveat the report diff has to carry.** The corpora
    install their bundles into each Ruby's own gem home, which is exactly what this walk reads,
    so bundling a sixth project changes the key and therefore the denominator. Two runs on
    different machines are not comparable on this lane; two runs on this one are, as long as the
    cache is not rebuilt between them. Delete `tmp/audit-foreign.json` to rebuild.
    """
    if CACHE.exists():
        return set(json.loads(CACHE.read_text()))
    names = set()
    roots = glob.glob(os.path.expanduser("~/.asdf/installs/ruby/*/lib/ruby/gems/*/gems"))
    roots += glob.glob(os.path.expanduser("~/.asdf/installs/ruby/*/lib/ruby/[0-9]*"))
    for root in roots:
        for here, dirs, files in os.walk(root):
            dirs[:] = [d for d in dirs if d not in (".git", "node_modules")]
            for name in files:
                if not name.endswith(".rb"):
                    continue
                try:
                    text = Path(here, name).read_text(encoding="utf-8", errors="replace")
                except OSError:
                    continue
                names.update(m.group(1) for m in patterns.DEF.finditer(text))
                for found in patterns.MACRO.finditer(text):
                    for symbol in patterns.SYMBOL.finditer(found.group(2)):
                        names.add(symbol.group(1).rstrip("="))
    CACHE.parent.mkdir(parents=True, exist_ok=True)
    CACHE.write_text(json.dumps(sorted(names)))
    return names
