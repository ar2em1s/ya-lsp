---
paths:
  - "scripts/corpora.py"
  - "scripts/corpora.toml"
  - "scripts/canary.py"
  - "scripts/audit/**"
  - "Makefile"
---

# The corpora

Six real Ruby applications, cloned into `tmp/corpora/`, pinned by `scripts/corpora.toml` and set
up by `scripts/corpora.py`. This rule is what may be done to them and what may be taken out of
them.

## The licence rule, which is the one that can actually be broken

**No corpus source text is ever committed into this repository. Not a line, not a fragment, not as
a fixture, not as a ledger key.**

| corpus | licence | committing a line here would be |
|---|---|---|
| lobsters | BSD-3-Clause | permitted, with notice |
| solidus | BSD-3-Clause | permitted, with notice |
| chatwoot | MIT, except `enterprise/` | permitted, with notice |
| chatwoot `enterprise/`, `spec/enterprise/` | proprietary Enterprise Licence | **forbidden outright** |
| mastodon | AGPL-3.0-or-later | a fragment of an AGPL work inside an MIT one |
| forem | AGPL-3.0-or-later | the same |
| discourse | GPL-2.0-or-later | the same |

ya-lsp is MIT. Four of the six are copyleft and one has a proprietary subtree, so the blanket rule
is cheaper to hold than five per-corpus exceptions — and it costs nothing, because **a hash of a
line does the same job as the line.** The audit ledger keys on
`(corpus, sha, path, offset, sha256(line))`: it still drops a position whose line changed, which is
the only property the text was there for, and it carries no copyrighted content at all.

Reading is unrestricted for every one of them, including chatwoot's `enterprise/` tree, whose own
licence grants exactly this: *"you may copy and modify the Software for development and testing
purposes, without requiring a subscription"*. It is the copying **out** that is forbidden. GPL and
AGPL restrict conveying, not use, and no artifact ya-lsp ships contains a byte of any corpus — so
nothing is owed in `THIRD-PARTY-NOTICES.txt`, and `licensing.md`'s rule that the obligation is per
*artifact* rather than per repository is what makes that true. **It is a property of how they are
used**: vendor one file into `tests/`, or cache a tarball here, and the obligation attaches.

A corpus path is not a secret; a corpus *line* is the thing with an owner. Naming
`app/models/user.rb:41` in a plan or a commit message is fine.

## The pin, and why a manifest travels with every number

- **`corpora.toml` is the only copy of a commit.** The `Makefile` holds asserted *counts* and
  nothing else (`canary.md`), so `CANARY_SHA` was deleted rather than duplicated: two copies of a
  SHA means one stale copy, and the stale one is what somebody reads.
- **An absolute count is meaningless without the manifest.** `make corpora-status ARGS=--json`
  records the commit, the resolved Ruby, whether the bundle resolves and how many gems are locked.
  Bundle state has changed between sessions before and read as a behaviour change; a diff between
  two runs cannot separate *ya-lsp changed* from *the corpus changed* unless both sides carry this.
  Diffs across one manifest stay safe. Absolutes across two do not.
- **Ruby is out of scope and the script never installs one.** Each corpus declares a version; the
  script resolves it against asdf and prints `asdf install ruby X` when it cannot. lobsters admits
  no substitute — its Gemfile pins `ruby "4.0.0"` exactly, so bundler rejects 4.0.1.
  `--allow-nearest` takes another patch of the same MAJOR.MINOR, preferring one whose bundle
  already resolves over the highest one, and says which in the manifest. It is an escape hatch, not
  a default: a measurement taken under a substituted Ruby says so on its face.
- **discourse is swept, and was not until 2026-09-12.** The exclusion was measured against the
  *exhaustive* bench sweeps, where it is four times the next corpus and over an hour a side; the
  audit draws a stratified sample and there it costs 77s. `audit.md` carries the numbers and what
  they buy. Its row in the table says `role = "audit"` so the audit does not have to remember, and
  `static-only` remains the role for a corpus that may be counted and never asked.

## The strscan trap, and why solargraph is measured from a composed bundle

solargraph renders every hover card with **kramdown**, and kramdown scans with `StringScanner`.
`strscan` is a **C extension**, so two builds of it in one process means two distinct
`StringScanner` classes, and the type check fails against itself:

    [TypeError] wrong argument type StringScanner (expected StringScanner)

**Every hover raises. `definition` never touches kramdown and answers perfectly.** Nothing on
screen says why, so a comparison run reads it as solargraph being bad at hover rather than as
solargraph being unable to render one.

### Measured, mastodon, 2026-09-10

Same Ruby (4.0.1), same corpus commit, same 3,011-member key, same 1200 s budget and 20 s cap.
The only variable is where solargraph is run from.

| | global `gem install` | composed bundle |
|---|---|---|
| **request errors** | **513** | **0** |
| **hover answered** | **7 / 338 (2%)** | **427 / 811 (53%)** |
| definition answered | 163 / 338 (48%) | 396 / 811 (49%) |
| definition p50 | 1350 ms | 497 ms |
| positions reached | 338 | 811 |
| skipped for budget | 662 | 189 |
| truth exact / silent / wrong | 102 / 146 / 0 | 102 / 146 / 0 |

Three things in that table matter beyond the headline.

- **Hover lands level with definition** — 53% against 49%. That is what a healthy run looks like,
  and the gap is the check: a hover column far below the definition column on the same server is
  this bug until proven otherwise.
- **The broken configuration also loses most of the plan.** Definition ran 2.7x faster and the run
  reached 2.4x the positions in the same budget. The duplicate was not only killing hover, it was
  burning the CPU that the rest of the sweep needed.
- **`truth` is byte-identical across both.** That pass is keyed on definition, so it should not
  move, and it did not. It is the control.

The load path was measured directly, not inferred: the global install loads **two**
`strscan.bundle` C extensions and Ruby prints `warning: already initialized constant
StringScanner::Version`; the composed bundle loads **one** and is silent.

**One residual confound, stated because it is not resolvable from this pair.** The composed bundle
also moves solargraph 0.60.4 -> 0.59.2, because that is what mastodon's constraints allow. The
error count going to exactly zero follows the load path rather than the version bump, but the
*speedup* cannot be attributed from these two runs alone. Separating them needs a third run
pinning 0.60.4 inside a bundle, and bundler may refuse to resolve it.

### What the setup does about it

`step_solargraph` writes `.solargraph-bundle/Gemfile`, which `eval_gemfile`s the corpus' own and
adds solargraph on top — the same mechanism ruby-lsp uses for `.ruby-lsp/Gemfile`, and discourse
is the proof it degrades well: discourse declares `gem "ruby-lsp"` itself, so ruby-lsp composes
nothing and runs on the project's resolved version.

- **A gem the corpus already declares is not added again, at any version or none.** forem declares
  `gem "solargraph", "~> 0.45"`. Bundler reads a bare `gem "solargraph"` as `solargraph (>= 0)` —
  which is *a different version requirement* — and refuses the Gemfile at **parse** time, before
  resolution starts: *"You cannot specify the same gem twice with different version requirements.
  You specified: solargraph (~> 0.45) and solargraph (>= 0)."* So there is no constraint to
  negotiate and no floor to ask for; where the corpus declares it, the corpus' version is the only
  option, and `solargraph_gemfile` emits a comment in its place. **There is therefore no way to
  have both one strscan and one solargraph version across the six**, and the trade was decided by
  the table above: a run with 513 errors is not a measurement.
- **The resolved version is per corpus and `status` prints it.** Measured 2026-09-10:

  | corpus | solargraph | why |
  |---|---|---|
  | lobsters | 0.60.4 | nothing constrains it |
  | chatwoot | 0.60.4 | nothing constrains it |
  | solidus | 0.59.2 | the rest of the lock pulls it back |
  | mastodon | 0.59.2 | the rest of the lock pulls it back |
  | discourse | 0.59.2 | the rest of the lock pulls it back |
  | forem | **0.48.0** | forem declares `~> 0.45` itself |

  **No cross-corpus solargraph total may be reported without that column beside it**, and forem's
  0.48.0 is twelve minor versions behind the newest — an asterisk on every forem number, but a
  documented one rather than a hidden one.
- **`docs` runs through the bundle too.** solargraph's cache is keyed by its own version as well as
  the gem's, so caching under a global 0.60.4 and serving under a bundled 0.59.2 builds a cache the
  running server ignores.
- **The bundle self-ignores.** `.solargraph-bundle/.gitignore` contains `*`, like `.ruby-lsp/`, so a
  pinned clone stays clean without touching the corpus' own `.gitignore`.

### Two causes, and only one of them is ours

- **A stray copy in the Ruby's own gem home.** Installing solargraph into a contained `GEM_HOME`
  causes it, which is why `lsps` installs into the Ruby's own and never sets `GEM_HOME`. It also
  happens without anyone meaning it: mastodon was bundled under 4.0.1 before it moved to its
  declared 4.0.6 and left `strscan 3.1.8` behind there, where **solidus** inherited it with nothing
  in solidus asking for strscan at all. Removable; `status` prints the `gem uninstall` line.
- **The corpus' own lockfile pinning it.** mastodon and discourse both pin `strscan 3.1.8`. Not
  removable — and not a problem either, once solargraph runs from the bundle.

**The obvious generalisation is the wrong test.** "A default gem pinned away from the Ruby's
default" fires on five gems in lobsters and seventeen in chatwoot, both healthy: `json`, `timeout`,
`erb` and the rest are pure Ruby and double-load harmlessly. What matters is a *C extension* loaded
twice, and the cheap observable is a second `strscan` beside the default.

## Setting one up

`make corpora` runs six idempotent steps; each is also its own target, because the expensive ones
are `gems` and `docs` and re-running the cheap ones should not pay for them.

- **Ask git about `<dir>/.git`, never about `git -C <dir>`'s answer.** The corpora live under
  `tmp/`, inside this repository, and **git searches upwards**: `git -C tmp/x rev-parse --git-dir`
  in an empty `tmp/x` succeeds and answers about *ya-lsp*. The obvious spelling of "is this a
  repository yet?" therefore skips the `init`, adds a remote to ya-lsp, fetches a corpus into
  ya-lsp's object store and runs `checkout --detach` **on the working tree being developed in**.
  It was written that way once. `canary.md` carries the same warning for the same reason.
- **Setting a corpus up makes it dirty, and that is expected.** `.tool-versions`, `.solargraph.yml`,
  `Gemfile-custom`, `.ruby-lsp/`, `.yardoc/` and `.solargraph/` are written into a pinned clone.
  `corpora.py`'s `WRITTEN` list is the whitelist and `status` measures dirtiness against it — a
  cleanliness check that flags the files setup wrote is a check nobody reads, and *that* is how a
  real stray edit gets through. forem is the only corpus where the config overwrites a **tracked**
  file, and the step says so when it does.
- **solidus needs `Gemfile-custom` before it will index at all.** Its `.rubocop.yml` names
  `rubocop-rails` under `plugins:` and its Gemfile does not declare it, so RuboCop raises inside
  ruby-lsp's `initialized` handler — which is where indexing starts — and the server then answers
  nothing for the rest of its life. It went 0/41 → 29/41 on that one line. The hook is solidus'
  own (`Gemfile:91`) and solidus gitignores it.
- **`docs` is the expensive step, and only on the fallback path.** `solargraph gems` loads every
  installed gem's RBS in one environment, so one gem with a malformed signature takes the corpus
  with it: chatwoot has `snaky_hash-2.0.5`, which declares `SnakyHash::VERSION` in both
  `sig/snaky_hash.rbs` and `sig/snaky_hash/version.rbs` — tolerated by rbs 3, raised on by rbs 4.
  Upstream's bug; there is nothing to fix here. The fallback caches gem by gem and names what
  refuses. Measured on chatwoot, 2026-09-10: **4:04 for 381 gems, 380 cached, one refused.** A
  corpus whose whole-environment load succeeds pays a small fraction of that. Neither half of the
  step is fatal — both tools are competitors in the comparison harness, and a short solargraph
  cache does not stop ya-lsp answering anything.
- **ruby-lsp composes its own bundle** (`.ruby-lsp/Gemfile`) and adds `ruby-lsp-rails` itself once
  it sees Rails, so the addon is deliberately not in `LSP_GEMS`. `RAILS_ENV` must be in the
  server's environment or that addon raises and dies while the server keeps answering with the
  Rails half missing — `ruby-lsp-needs-the-app-to-boot`.
- **A healthy competitor is worth ~0 on accuracy, and that is itself the result.** Getting all six
  booting moved solidus 0/41 → 29/41 because its index never ran, and moved mastodon and discourse
  *not at all*. ya-lsp needs none of it on any corpus. The honest form of the claim is not that
  ruby-lsp is less accurate but that it has far more ways to be silently wrong.
