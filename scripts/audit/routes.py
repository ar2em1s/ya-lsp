"""The one shape whose candidates a suffix match cannot find: the Rails route helper.

Rails *generates* a route helper, so **nothing writes it down**. The filter is the inverse of the
shape: anything the corpus writes down itself is not one.
"""

import os
import re
from pathlib import Path

KEY_HELPER_DEF = re.compile(
    r"^[ \t]*def\s+(?:self\s*\.\s*)?([a-z_][A-Za-z0-9_]*_(?:path|url))\b", re.M)
KEY_HELPER_COL = re.compile(r"^\s*t\.\w+\s+[\"']([a-z_][A-Za-z0-9_]*_(?:path|url))[\"']", re.M)
KEY_HELPER_SQL = re.compile(r"^\s{4}([a-z_][A-Za-z0-9_]*_(?:path|url))\s+[a-z]", re.M)
# `stored_url = session.delete(...)` is a local variable, not something Rails generated.
KEY_HELPER_VAR = re.compile(r"^[ \t]*([a-z_][A-Za-z0-9_]*_(?:path|url))\s*(?:\|\|)?=[^=~]", re.M)
# A parameter is a local too: `def non_matching_uri_hosts?(base_url, comparison_url)` and
# `.each do |template_path|`. The list is split and only each parameter's own name is read, so a
# default that calls a real helper (`def back(to = root_path)`) is not taken for one.
KEY_HELPER_PARAMS = re.compile(r"\bdef\s+[\w.]+[?!=]?\s*\(([^)]*)\)|\|([^|\n]*)\|")
KEY_HELPER_PARAM = re.compile(r"^\s*[*&]{0,2}([a-z_][A-Za-z0-9_]*_(?:path|url))\s*(?:[:=]|$)")


def parameters(text):
    """Every `_path`/`_url` name a method or a block in one file takes as a parameter.

    **Per file, not per corpus** like [`non_helpers`]: a parameter is a local of its own method, and
    solidus' admin component takes `account_path:` and `logout_path:` while its storefront calls
    the helpers of those names. Read corpus-wide, one keyword argument would strike a real helper
    from every file's draw.
    """
    found = set()
    for listed in KEY_HELPER_PARAMS.finditer(text):
        for piece in (listed.group(1) or listed.group(2) or "").split(","):
            named = KEY_HELPER_PARAM.match(piece)
            if named:
                found.add(named.group(1))
    return found


def non_helpers(corpus):
    """Names ending `_path`/`_url` that are provably **not** Rails route helpers.

    The `route` shape asks the one question a route helper poses, and a suffix match alone does not
    ask it. It samples columns used as keyword arguments (`normalized_url`), plain `def`s
    (`avatar_url`) and SQL aliases in heredocs far more often than real helpers.

    So the filter is what the corpus writes down: anything it `def`s or assigns to a local, and
    anything a schema declares as a column.

    **No gem-defined names here, on purpose.** Over-blocking is safe in a *key*, where it removes a
    question from the denominator, and unsafe in a *filter on the draw*, where it removes the
    question from the sample.
    - Gems define exactly the wrong names: `root_path`, `user_path` and `settings_path` are all in
      `foreign_names()` (octokit, sidekiq's web UI, `language_server-protocol`, datadog), and all
      are also real helpers a corpus' own `config/routes.rb` generates.
    - Unrelated gems sharing a name are no evidence about this corpus' routes.
    - It keeps the draw a pure function of `(seed, sha, --per-file)`. `foreign_names()` walks
      whatever is installed on this machine, so the draw would move whenever an unrelated project is
      bundled, and the report diffs runs.
    """
    found = set()
    for here, dirs, names in os.walk(corpus.dir):
        dirs[:] = [d for d in dirs if d not in (".git", "node_modules")]
        for name in names:
            if not name.endswith((".rb", ".rake", ".sql")):
                continue
            try:
                text = Path(here, name).read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            found |= {m.group(1) for m in KEY_HELPER_DEF.finditer(text)}
            found |= {m.group(1) for m in KEY_HELPER_VAR.finditer(text)}
            if name.endswith("schema.rb"):
                found |= {m.group(1) for m in KEY_HELPER_COL.finditer(text)}
            elif name.endswith(".sql"):
                found |= {m.group(1) for m in KEY_HELPER_SQL.finditer(text)}
    return found
