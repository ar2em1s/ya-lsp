"""The one shape whose candidates a suffix match cannot find: the Rails route helper.

A route helper is a name Rails *generates*, and therefore one **nothing writes down**. So the
filter is the inverse of the shape: everything the corpus writes down itself is not one.
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


def non_helpers(corpus):
    """Names ending `_path`/`_url` that are provably **not** Rails route helpers.

    The `route` shape exists to ask the one question a route helper poses, and matching on the
    suffix alone does not ask it: on lobsters that sampled `normalized_url` (a `stories` column
    used as a keyword argument), `avatar_url` (a `def` in `user.rb`) and `confidence_order_path`
    (a SQL alias inside a heredoc) — one real helper in fifty-two.

    So the filter is what the corpus writes down: anything it `def`s or assigns to a local, and
    anything a schema declares as a column.

    **The gem half of `truth.py`'s version is deliberately not here, and dropping it is a
    correction rather than a simplification.** Over-blocking is safe in a *key*, where it removes
    a question from the denominator, and unsafe in a *filter on the draw*, where it removes the
    question from the sample — and measured over the five corpora it removed exactly the wrong
    ones: `root_path`, `user_path` and `settings_path` are all in `foreign_names()`, and all
    three are real helpers of lobsters' own `config/routes.rb` (`root to:`, `get "/settings"`).
    The corpus half cuts 4 to 14 names per corpus; the gem half added 0 to 3 more and they were
    the helpers. What defines them is unrelated gems that happen to share a name: octokit's
    `def user_path` builds a GitHub API URL, sidekiq's web UI and the `language_server-protocol`
    gem both write `def root_path`, and datadog writes `def settings_path`. None of them is
    evidence about this corpus' routes, and two of the three are on the disk because of *this*
    repository rather than because of any corpus.

    Dropping the gem half also keeps the draw a pure function of `(seed, sha, --per-file)`.
    `foreign_names()` is a walk of whatever is installed on this machine, so a draw that
    consulted it would move when an unrelated project is bundled — and stage 4 diffs runs.
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
