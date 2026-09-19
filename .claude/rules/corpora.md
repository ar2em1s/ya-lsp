---
paths:
  - "scripts/corpora.py"
  - "scripts/corpora.toml"
  - "scripts/canary.py"
  - "scripts/audit/**"
  - "Makefile"
---

# The corpora

Six real Ruby apps, cloned into `tmp/corpora/`, pinned by `scripts/corpora.toml` and set up by
`scripts/corpora.py`.

## Rule 1: never commit corpus source text

**Not a line, not a fragment, not in a fixture, not as a ledger key.**

| Corpus | Licence | Why it matters |
|---|---|---|
| lobsters, solidus | BSD-3-Clause | would need a notice |
| chatwoot | MIT, except `enterprise/` | `enterprise/` and `spec/enterprise/` are proprietary: forbidden outright |
| mastodon, forem | AGPL-3.0-or-later | copyleft inside an MIT repo |
| discourse | GPL-2.0-or-later | copyleft inside an MIT repo |

- **Store a hash, not the text.** The audit ledger keys on `(corpus, sha, path, offset,
  sha256(line))`, which still detects a changed line.
- **Reading is always fine**, including chatwoot's `enterprise/`. Only copying *out* is forbidden.
- **Paths are fine.** `app/models/user.rb:41` may appear in a commit message.

## The pin and the manifest

1. **`corpora.toml` holds the only copy of each commit.** The `Makefile` holds counts only
   (`canary.md`).
2. **Never compare absolute counts across two manifests.** `make corpora-status ARGS=--json` records
   commit, Ruby, bundle state and gem count. Diffs within one manifest are safe.
3. **The script never installs Ruby.** It prints `asdf install ruby X`. `--allow-nearest` takes
   another patch of the same MAJOR.MINOR, and the manifest says so. lobsters pins `4.0.0` exactly.
4. **`clone` needs no Ruby, because CI's canary job has none.** The asdf check is per command (the
   `needs_ruby` column of `STEPS`), never at the top of `main`.
5. **Check for `<dir>/.git` directly, never with `git -C <dir>`.** Git searches upwards into ya-lsp
   (`canary.md`).

## Setting one up

`make corpora` runs six idempotent steps. Each is also its own target, and `gems` and `docs` are
the slow ones.

- **Setup dirties the clone on purpose.** `WRITTEN` whitelists what it writes, and `status` measures
  dirtiness against that list. On forem, the config overwrites a tracked file, and the step says
  so.
- **solidus needs `Gemfile-custom`**, which adds `rubocop-rails`. Without it, RuboCop raises inside
  ruby-lsp's `initialized` handler and ruby-lsp never indexes.
- **ruby-lsp composes its own bundle and adds `ruby-lsp-rails` itself**, so the addon is not in
  `LSP_GEMS`. `RAILS_ENV` must be set in the server's environment.
- **`docs` falls back to caching gem by gem.** One gem with bad RBS (chatwoot's `snaky_hash`)
  otherwise fails the whole corpus under rbs 4. That is an upstream bug. Neither half of the step is
  fatal.
- **A healthy competitor barely changes accuracy.** ya-lsp needs none of this setup. The claim is
  that ruby-lsp has more ways to be silently wrong, not that it is less accurate.

## The strscan trap

**Run solargraph from a composed bundle, never from a global install.** kramdown uses
`StringScanner`, and `strscan` is a C extension. Two copies loaded means two `StringScanner`
classes:

    [TypeError] wrong argument type StringScanner (expected StringScanner)

Every hover raises, while `definition` works. **If hover answers far less often than definition on
the same server, suspect this first.** In the global install, Ruby also prints
`already initialized constant StringScanner::Version`.

- **`step_solargraph` writes `.solargraph-bundle/Gemfile`.** It `eval_gemfile`s the corpus' own
  Gemfile and adds solargraph, the same way ruby-lsp does. `.gitignore` contains `*`.
- **Never re-add a gem the corpus already declares.** Bundler refuses two different requirements at
  parse time. forem declares `~> 0.45`, so `solargraph_gemfile` writes a comment instead.
- **The solargraph version therefore differs per corpus**, and `status` prints it. Never report a
  cross-corpus solargraph total without that column.
- **`docs` runs through the bundle too.** The cache is keyed by solargraph's version.
- **There are two causes:**
  - A stray `strscan` in the Ruby's gem home. `lsps` installs there and never sets `GEM_HOME`;
    `status` prints the `gem uninstall` line.
  - The corpus lockfile pinning `strscan` (mastodon, discourse). That is harmless once solargraph
    runs from the bundle.
- **Test for a C extension loaded twice**, not for "a default gem pinned away". Pure-Ruby
  duplicates are harmless.
