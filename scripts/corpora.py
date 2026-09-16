#!/usr/bin/env python3
"""Set up the six benchmark corpora, reproducibly, from `scripts/corpora.toml`.

Every measurement this project has ever published over a real repository was taken against
whatever those clones happened to contain that day. Nothing recorded which commit, which Ruby,
or how much of the bundle was installed — so an absolute count could not be compared against one
taken a week earlier, and a diff between two runs could not separate *ya-lsp changed* from *the
corpus changed*. That is the gap this script closes: the table is the pin, and `status --json` is
the manifest a measurement carries beside its numbers.

**Ruby is out of scope and stays out of scope.** This script never installs a Ruby. It resolves
the version the corpus declares against what asdf already has and, when it is missing, prints the
`asdf install ruby X` line and stops. `--allow-nearest` accepts another patch of the same
MAJOR.MINOR and says loudly in the manifest that it did.

**Nothing here is vendored and nothing here may be quoted.** Four of the six corpora are copyleft
and one has a proprietary subdirectory; the licence header in `corpora.toml` is the argument, and
the rule that follows from it is that no corpus source text is ever committed into this
repository.

Steps, each idempotent and each runnable alone:

    clone        fetch the pinned commit into the corpus directory (git only, no Ruby)
    ruby         write `.tool-versions` with the resolved Ruby
    gems         `bundle install`, plus the one corpus that needs a Gemfile hook first
    lsps         install ruby-lsp, solargraph and solargraph-rails into that Ruby
    solargraph   write `.solargraph.yml`
    docs         cache solargraph's gem documentation; warm ruby-lsp's composed bundle
    status       what is actually on disk, as a table or as the manifest
    setup        all of the above, in that order

**Ask git about `<dir>/.git` and never about `git -C <dir>`'s answer.** The corpora live under
`tmp/`, inside this repository, and git searches *upwards*: `git -C tmp/x rev-parse --git-dir` in
an empty `tmp/x` succeeds and answers about **ya-lsp**. The obvious spelling of "is this a
repository yet?" therefore skips the `init`, adds a remote to ya-lsp, fetches a corpus into
ya-lsp's object store and runs `checkout --detach` on the working tree being developed in.
`canary.md` records that this was written the wrong way once.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TABLE = ROOT / "scripts" / "corpora.toml"
ASDF_RUBY = Path.home() / ".asdf" / "installs" / "ruby"

# What setting a corpus up writes into a pinned clone. `status` measures dirtiness against this
# list rather than against nothing, because every one of these files is *supposed* to be there
# and a check that calls the corpus dirty for them is a check nobody reads.
WRITTEN = (
    ".tool-versions",
    ".solargraph.yml",
    "Gemfile-custom",
    ".ruby-lsp/",
    ".solargraph-bundle/",
    ".yardoc/",
    ".solargraph/",
)

# `.rubocop.yml` lists `rubocop-rails` under `plugins:` and the Gemfile never declares it, so
# RuboCop's FeatureLoader raises `cannot load such file -- rubocop-rails` *inside* ruby-lsp's
# `initialized` handler — which is where it starts indexing. The index therefore never runs and
# the server answers nothing for the rest of its life. solidus went 0/41 -> 29/41 on this one
# line. `Gemfile-custom` is solidus' own hook (`Gemfile:91`) and is gitignored by solidus.
GEMFILE_CUSTOM = {
    "solidus": """\
# Added by ya-lsp's corpus setup (scripts/corpora.py), not by solidus.
# `.rubocop.yml` names `rubocop-rails` under `plugins:` but the Gemfile does not declare it, so
# RuboCop raises inside ruby-lsp's `initialized` handler, which is where indexing starts. This is
# the gem the config already asks for.
gem "rubocop-rails", require: false
"""
}

# The gems every corpus needs for the comparison harness to have three servers to talk to.
# ruby-lsp composes its own bundle on first start (`.ruby-lsp/Gemfile`) and adds `ruby-lsp-rails`
# itself once it sees Rails, so that addon is deliberately not listed here.
LSP_GEMS = ("ruby-lsp", "solargraph", "solargraph-rails", "yard")

# The bundle solargraph is measured from, composed on top of the corpus' own Gemfile exactly the
# way ruby-lsp composes `.ruby-lsp/Gemfile`.
#
# **A gem the corpus already declares is not added again, at any version or none.** Bundler reads
# a bare `gem "solargraph"` as `solargraph (>= 0)` and refuses it beside forem's own
# `~> 0.45` — *"You cannot specify the same gem twice with different version requirements"* — at
# Gemfile *parse* time, before resolution starts. So there is no constraint to negotiate and no
# floor to ask for: where the corpus declares it, the corpus' version is the only option. That is
# the same degradation ruby-lsp makes for discourse, which declares `gem "ruby-lsp"` itself and
# gets no composed bundle at all.
SOLARGRAPH_BUNDLE = ".solargraph-bundle"
SOLARGRAPH_GEMS = ("solargraph", "solargraph-rails")


def solargraph_gemfile(corpus):
    """The composed Gemfile for this corpus, adding only what the corpus does not declare."""
    text = (corpus.dir / "Gemfile").read_text() if (corpus.dir / "Gemfile").exists() else ""
    lines = [
        "# Written by ya-lsp's corpus setup (scripts/corpora.py), not by the corpus.",
        '# See `.claude/rules/corpora.md`, "The strscan trap".',
        'eval_gemfile(File.expand_path("../Gemfile", __dir__))',
    ]
    for name in SOLARGRAPH_GEMS:
        # Quoted on both sides, so looking for `solargraph` does not match `solargraph-rails`.
        if re.search(r"gem\s+[\"']" + re.escape(name) + r"[\"']", text):
            lines.append(f"# {name}: declared by the corpus; declaring it again is a parse error.")
        else:
            lines.append(f'gem "{name}", require: false')
    return "\n".join(lines) + "\n"


SOLARGRAPH_YML = """\
---
# Written by ya-lsp's corpus setup (scripts/corpora.py), not by the corpus.
include:
- Rakefile
- Gemfile
- "*.gemspec"
- "./**/*.rb"
exclude:
- spec/**/*
- test/**/*
- vendor/**/*
- ".bundle/**/*"
require: []
domains: []
reporters:
- rubocop
- require_not_found
formatter:
  rubocop:
    cops: safe
    except: []
    only: []
    extra_args: []
type_checker:
  rules: {}
require_paths: []
plugins:
- solargraph-rails
max_files: 20000
"""


class Fail(Exception):
    """A step that could not be completed, carrying the sentence a reader needs."""


class Corpus:
    def __init__(self, name, entry):
        self.name = name
        self.entry = entry
        self.dir = ROOT / entry["dir"]
        self.sha = entry["sha"]
        self.repo = entry["repo"]
        self.declared_ruby = entry["ruby"]
        self.license = entry["license"]
        self.role = entry["role"]
        self._ruby = None

    def __repr__(self):
        return f"<Corpus {self.name}>"

    # -- git -------------------------------------------------------------------------------

    @property
    def is_clone(self):
        """`.git` exists *here*. A path test cannot walk up; `git -C` can and does."""
        return (self.dir / ".git").exists()

    def guard(self):
        if self.dir.exists() and self.dir.resolve() == ROOT.resolve():
            raise Fail(f"{self.name}: dir is the ya-lsp working tree; refusing")

    def head(self):
        if not self.is_clone:
            return None
        out = run(["git", "-C", str(self.dir), "rev-parse", "HEAD"], check=False)
        return out.stdout.strip() if out.returncode == 0 else None

    def dirty(self):
        """Paths that changed and are not ones setup writes. Empty means clean enough."""
        if not self.is_clone:
            return []
        out = run(["git", "-C", str(self.dir), "status", "--porcelain"], check=False)
        if out.returncode != 0:
            return []
        paths = []
        for line in out.stdout.splitlines():
            path = line[3:].strip().strip('"')
            if any(path == w or path.startswith(w) for w in WRITTEN):
                continue
            paths.append(path)
        return paths

    # -- ruby ------------------------------------------------------------------------------

    def resolve_ruby(self, allow_nearest):
        """The declared version if installed; otherwise a named failure, not a guess.

        `--allow-nearest` prefers a patch whose bundle *already resolves* over the highest one,
        because the highest is not usually the one the gems were installed under: mastodon
        declares 4.0.6, has 4.0.5 and 4.0.1 available, and only 4.0.1 satisfies its Gemfile.
        Picking by version number alone would report a bundle as broken that is not.
        """
        if self._ruby is not None:
            return self._ruby
        installed = installed_rubies()
        if self.declared_ruby in installed:
            self._ruby = (self.declared_ruby, None)
            return self._ruby
        if allow_nearest:
            minor = ".".join(self.declared_ruby.split(".")[:2])
            same = [v for v in installed if ".".join(v.split(".")[:2]) == minor]
            for candidate in reversed(same):
                if self.is_clone and bundle_ok(self.dir, candidate):
                    self._ruby = (
                        candidate,
                        f"substituted {candidate} for {self.declared_ruby} (bundle resolves)",
                    )
                    return self._ruby
            if same:
                self._ruby = (
                    same[-1],
                    f"substituted {same[-1]} for {self.declared_ruby} (no patch resolves)",
                )
                return self._ruby
        raise Fail(
            f"{self.name}: Ruby {self.declared_ruby} is not installed.\n"
            f"      Installing a Ruby is out of scope for this script. Run:\n"
            f"        asdf install ruby {self.declared_ruby}\n"
            f"      or re-run with --allow-nearest to accept another patch of "
            f"{'.'.join(self.declared_ruby.split('.')[:2])}.x"
        )


def version_key(v):
    parts = []
    for piece in v.split("."):
        parts.append(int(piece) if piece.isdigit() else -1)
    return parts


def bundle_ok(directory, ruby):
    """Does this Ruby already resolve that Gemfile? Cheap, and the only honest test."""
    return run(["bundle", "check"], cwd=directory, ruby=ruby, check=False).returncode == 0


def installed_rubies():
    if not ASDF_RUBY.is_dir():
        return []
    return sorted((p.name for p in ASDF_RUBY.iterdir() if p.is_dir()), key=version_key)


def run(cmd, cwd=None, ruby=None, check=True, capture=True, echo=False, extra_env=None):
    env = dict(os.environ)
    if ruby:
        env["ASDF_RUBY_VERSION"] = ruby
        cmd = ["asdf", "exec", *cmd]
    env.update(extra_env or {})
    if echo:
        print(f"      $ {' '.join(cmd)}", flush=True)
    out = subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else None,
        env=env,
        capture_output=capture,
        text=True,
    )
    if check and out.returncode != 0:
        tail = ((out.stderr or "") + (out.stdout or "")).strip().splitlines()
        detail = "\n        ".join(tail[-6:]) if tail else f"exit {out.returncode}"
        raise Fail(f"{' '.join(cmd)} failed:\n        {detail}")
    return out


def load_table(only=None):
    with open(TABLE, "rb") as handle:
        table = tomllib.load(handle)
    corpora = [Corpus(name, entry) for name, entry in table.items()]
    if only:
        wanted = set(only)
        unknown = wanted - {c.name for c in corpora}
        if unknown:
            raise Fail(f"no such corpus: {', '.join(sorted(unknown))}")
        corpora = [c for c in corpora if c.name in wanted]
    return corpora


# ---------------------------------------------------------------------------------- steps


def step_clone(corpus, args):
    corpus.guard()
    if corpus.head() == corpus.sha:
        return f"at {corpus.sha[:12]}"
    corpus.dir.mkdir(parents=True, exist_ok=True)
    corpus.guard()
    if not corpus.is_clone:
        run(["git", "-C", str(corpus.dir), "init", "-q"])
    remotes = run(["git", "-C", str(corpus.dir), "remote"], check=False).stdout.split()
    if "origin" not in remotes:
        run(["git", "-C", str(corpus.dir), "remote", "add", "origin", corpus.repo])
    else:
        run(["git", "-C", str(corpus.dir), "remote", "set-url", "origin", corpus.repo])
    run(["git", "-C", str(corpus.dir), "fetch", "-q", "--depth", "1", "origin", corpus.sha])
    run(["git", "-C", str(corpus.dir), "checkout", "-q", "--detach", "FETCH_HEAD"])
    return f"fetched {corpus.sha[:12]}"


def step_ruby(corpus, args):
    ruby, note = corpus.resolve_ruby(args.allow_nearest)
    path = corpus.dir / ".tool-versions"
    wanted = f"ruby {ruby}\n"
    existing = path.read_text() if path.exists() else None
    if existing == wanted:
        return f"ruby {ruby}" + (f" ({note})" if note else "")
    path.write_text(wanted)
    return f"wrote ruby {ruby}" + (f" ({note})" if note else "")


def step_gems(corpus, args):
    ruby, _ = corpus.resolve_ruby(args.allow_nearest)
    custom = GEMFILE_CUSTOM.get(corpus.name)
    if custom:
        # Written before `bundle check`, because the hook changes what "satisfied" means.
        path = corpus.dir / "Gemfile-custom"
        existing = path.read_text() if path.exists() else None
        if existing != custom:
            path.write_text(custom)
    check = run(["bundle", "check"], cwd=corpus.dir, ruby=ruby, check=False)
    if check.returncode == 0:
        return "bundle satisfied"
    run(["bundle", "install"], cwd=corpus.dir, ruby=ruby, echo=args.verbose)
    return "bundle installed"


def step_lsps(corpus, args):
    ruby, _ = corpus.resolve_ruby(args.allow_nearest)
    listed = run(["gem", "list", "--local"], ruby=ruby).stdout
    have = {line.split(" ", 1)[0] for line in listed.splitlines() if line.strip()}
    missing = [gem for gem in LSP_GEMS if gem not in have]
    if not missing:
        return f"ruby {ruby}: all present"
    run(["gem", "install", "--no-document", *missing], ruby=ruby, echo=args.verbose)
    return f"ruby {ruby}: installed {', '.join(missing)}"


def solargraph_env(corpus):
    """What makes a command run out of the composed bundle. Relative, so `cwd` must be the corpus."""
    return {"BUNDLE_GEMFILE": f"{SOLARGRAPH_BUNDLE}/Gemfile"}


def solargraph_version(corpus):
    """The version the composed bundle resolved. Per corpus, and never assume it is the newest."""
    lock = corpus.dir / SOLARGRAPH_BUNDLE / "Gemfile.lock"
    if not lock.exists():
        return None
    for line in lock.read_text().splitlines():
        if line.startswith("    solargraph ("):
            return line.strip()[len("solargraph (") : -1]
    return None


def step_solargraph(corpus, args):
    """The config, then the bundle solargraph is actually measured from.

    Measured on mastodon, 2026-09-10, same Ruby and same corpus commit either side:
    a globally installed solargraph produced **513 request errors and 7 of 338 hovers**; out of
    this composed bundle, **0 errors and 427 of 811** — level with definition's 49%, which is what
    a healthy run looks like. It also ran definition 2.7x faster (p50 1350ms -> 497ms) and so
    reached 2.4x the plan in the same budget. `corpora.md` has the mechanism.
    """
    ruby, _ = corpus.resolve_ruby(args.allow_nearest)
    notes = []

    path = corpus.dir / ".solargraph.yml"
    existing = path.read_text() if path.exists() else None
    if existing == SOLARGRAPH_YML:
        notes.append("config current")
    else:
        tracked = run(
            ["git", "-C", str(corpus.dir), "ls-files", "--error-unmatch", ".solargraph.yml"],
            check=False,
        ).returncode == 0
        path.write_text(SOLARGRAPH_YML)
        notes.append("config written" + (" (over a tracked file)" if tracked else ""))

    bundle = corpus.dir / SOLARGRAPH_BUNDLE
    bundle.mkdir(exist_ok=True)
    # Self-ignoring, the way ruby-lsp's `.ruby-lsp/` is, so a pinned clone stays clean without
    # every corpus needing an entry in its own `.gitignore`.
    (bundle / ".gitignore").write_text("*\n")
    gemfile = bundle / "Gemfile"
    wanted = solargraph_gemfile(corpus)
    if (gemfile.read_text() if gemfile.exists() else None) != wanted:
        gemfile.write_text(wanted)
    if not (bundle / "Gemfile.lock").exists():
        # Seed from the corpus' own lock so bundler re-resolves rather than resolving from
        # nothing, which on a 344-gem application is the difference between seconds and minutes.
        corpus_lock = corpus.dir / "Gemfile.lock"
        if corpus_lock.exists():
            (bundle / "Gemfile.lock").write_text(corpus_lock.read_text())

    check = run(
        ["bundle", "check"], cwd=corpus.dir, ruby=ruby, check=False,
        extra_env=solargraph_env(corpus),
    )
    if check.returncode != 0:
        run(
            ["bundle", "install"], cwd=corpus.dir, ruby=ruby, echo=args.verbose,
            extra_env=solargraph_env(corpus),
        )
        notes.append(f"bundle installed, solargraph {solargraph_version(corpus)}")
    else:
        notes.append(f"bundle satisfied, solargraph {solargraph_version(corpus)}")
    return "; ".join(notes)


def step_docs(corpus, args):
    """Cache solargraph's documentation, then warm ruby-lsp's composed bundle.

    `solargraph gems` with no arguments loads *every* installed gem's RBS in one environment, so a
    single gem shipping a malformed signature takes the whole corpus down with it. chatwoot has
    one: `snaky_hash-2.0.5` declares `SnakyHash::VERSION` in both `sig/snaky_hash.rbs` and
    `sig/snaky_hash/version.rbs`, which rbs 3 tolerated and rbs 4 raises on. That is upstream's
    bug and there is nothing to fix here, so the fallback caches gem by gem instead and names the
    ones that refuse. Slower, and only on the path where the fast way already failed.

    Neither half is fatal. Both tools are *competitors* in the comparison harness; a corpus whose
    solargraph cache is short still answers every question ya-lsp is asked, and a setup step that
    fails the corpus over it would stop work for the wrong reason.
    """
    ruby, _ = corpus.resolve_ruby(args.allow_nearest)
    # Through the composed bundle, because solargraph's cache is keyed by its own version as well
    # as the gem's: caching under the global 0.60.4 and then serving under the bundle's 0.59.2
    # builds a cache the running server ignores.
    sg = ["bundle", "exec", "solargraph"]
    env = solargraph_env(corpus)
    whole = run([*sg, "gems"], cwd=corpus.dir, ruby=ruby, check=False, echo=args.verbose,
                extra_env=env)
    if whole.returncode == 0:
        cached = "solargraph cached"
    else:
        blamed = blame_gem(whole)
        names = locked_gems(corpus.dir)
        if not names:
            cached = f"solargraph FAILED ({blamed or 'no Gemfile.lock to fall back to'})"
        else:
            failed = []
            for index, name in enumerate(names, 1):
                if args.verbose:
                    print(f"      {index}/{len(names)} {name}", end="\r", flush=True)
                one = run([*sg, "gems", name], cwd=corpus.dir, ruby=ruby, check=False,
                          extra_env=env)
                if one.returncode != 0:
                    failed.append(name)
            if args.verbose:
                print(" " * 60, end="\r")
            cached = f"solargraph cached {len(names) - len(failed)}/{len(names)}"
            if failed:
                shown = ", ".join(failed[:5]) + ("..." if len(failed) > 5 else "")
                cached += f" (refused: {shown})"
            elif blamed:
                cached += f" (only the whole-environment load fails, on {blamed})"
    warm = run(
        ["ruby-lsp", "--time-index"], cwd=corpus.dir, ruby=ruby, check=False, echo=args.verbose
    )
    warmed = "ruby-lsp warmed" if warm.returncode == 0 else "ruby-lsp warm FAILED"
    return f"{cached}, {warmed}"


def blame_gem(out):
    """Which gem's signature broke the environment load, read out of the traceback."""
    text = (out.stderr or "") + (out.stdout or "")
    match = re.search(r"/gems/([A-Za-z0-9_.-]+)/sig/", text)
    return match.group(1) if match else None


# Each step, and whether it reaches for a Ruby. **The third column is what CI depends on.** The
# canary job installs no Ruby at all — deliberately, for a project whose headline is that it
# needs none — and it clones through this script, so `clone` has to run on a machine with no
# asdf on PATH. `RUBYLESS` is derived from this column rather than written out again, so a step
# added here cannot forget to say which kind it is.
STEPS = [
    ("clone", step_clone, False),           # git fetch and checkout; nothing else
    ("ruby", step_ruby, True),
    ("gems", step_gems, True),
    ("lsps", step_lsps, True),
    ("solargraph", step_solargraph, True),
    ("docs", step_docs, True),
]
# Which commands run without asdf. `setup` and `status` are in neither list and stay guarded:
# `setup` runs every step, and `status` resolves each corpus' Ruby to report on it.
RUBYLESS = frozenset(name for name, _, needs_ruby in STEPS if not needs_ruby)


# --------------------------------------------------------------------------------- status


def observe(corpus, args):
    """What is on disk right now. Never raises: an unset corpus is a row, not a crash."""
    row = {
        "name": corpus.name,
        "dir": str(corpus.dir.relative_to(ROOT)),
        "role": corpus.role,
        "license": corpus.license,
        "pinned_sha": corpus.sha,
        "declared_ruby": corpus.declared_ruby,
    }
    head = corpus.head()
    row["head"] = head
    row["at_pin"] = head == corpus.sha
    try:
        ruby, note = corpus.resolve_ruby(args.allow_nearest)
        row["ruby"] = ruby
        row["ruby_note"] = note
    except Fail:
        row["ruby"] = None
        row["ruby_note"] = f"not installed: asdf install ruby {corpus.declared_ruby}"
    tool_versions = corpus.dir / ".tool-versions"
    row["tool_versions"] = tool_versions.read_text().strip() if tool_versions.exists() else None
    if row["ruby"] and corpus.is_clone:
        check = run(["bundle", "check"], cwd=corpus.dir, ruby=row["ruby"], check=False)
        row["bundle"] = "satisfied" if check.returncode == 0 else "incomplete"
        row["gems_locked"] = count_locked(corpus.dir)
    else:
        row["bundle"] = "unknown"
        row["gems_locked"] = None
    row["strscan"] = strscan_state(row["ruby"], corpus.dir) if row["ruby"] else None
    row["solargraph_version"] = solargraph_version(corpus)
    row["solargraph_bundle"] = (corpus.dir / SOLARGRAPH_BUNDLE / "Gemfile.lock").exists()
    row["solargraph_yml"] = (corpus.dir / ".solargraph.yml").exists()
    row["ruby_lsp"] = ruby_lsp_state(corpus.dir)
    row["dirty"] = corpus.dirty()
    return row


def strscan_state(ruby, directory):
    """Two copies of `strscan` in one process is solargraph's silent hover killer.

    solargraph renders every hover card with kramdown, and kramdown scans with `StringScanner`.
    `strscan` is a *C extension*, so loading two builds of it puts two distinct `StringScanner`
    classes in the process and the type check fails against itself:

        [TypeError] wrong argument type StringScanner (expected StringScanner)

    Every hover raises; `definition` never touches kramdown and answers perfectly. Nothing on
    screen says why. Measured on lobsters: hover 6/45 against definition 17/45 with 83 request
    errors, and 23/45 with zero errors once the duplicate was gone.

    **The general "a default gem is pinned away from the Ruby's default" test is the wrong test**
    — it fires on five gems in lobsters, which is healthy, and on seventeen in chatwoot. Pure-Ruby
    default gems double-load harmlessly. What matters is a *C extension* loaded twice, and the
    cheap observable for that is a second `strscan` installed beside the default one.

    Two causes, and only one of them is ours to avoid. Installing solargraph into a contained
    `GEM_HOME` beside the project Ruby's own creates the duplicate — which is why `lsps` installs
    into the Ruby's own gem home and never sets `GEM_HOME`. The other cause is the project's:
    a lockfile that pins `strscan` away from the default. That one cannot be fixed from here.
    """
    out = run(["gem", "list", "strscan"], ruby=ruby, check=False)
    if out.returncode != 0:
        return None
    default, extra = None, []
    for line in out.stdout.splitlines():
        if not line.startswith("strscan "):
            continue
        for token in line[line.find("(") + 1 : line.rfind(")")].split(","):
            token = token.strip()
            if token.startswith("default:"):
                default = token.split(":", 1)[1].strip()
            elif token:
                extra.append(token)
    if default is None and not extra:
        return None
    if not extra:
        return {"default": default, "duplicates": [], "hover": "ok", "cause": None}
    # Which of the two causes is it? The lock is the discriminator, and it decides whether there
    # is anything to do: a pinned strscan is the corpus', a stray one is this machine's.
    pinned = None
    for line in (directory / "Gemfile.lock").read_text().splitlines() if (
        directory / "Gemfile.lock"
    ).exists() else []:
        if line.startswith("    strscan ("):
            pinned = line.strip()[len("strscan (") : -1]
            break
    cause = "lockfile" if pinned and pinned != default else "stray"
    return {
        "default": default,
        "duplicates": extra,
        "pinned": pinned,
        "cause": cause,
        "hover": "broken",
    }


def ruby_lsp_state(directory):
    """How ruby-lsp gets into this corpus. Absence of a composed bundle is not absence of setup.

    ruby-lsp composes `.ruby-lsp/Gemfile` on top of the project's only when the project does not
    declare ruby-lsp itself. discourse does declare it, so it composes nothing and runs on the
    project's own resolved version — which is the exact pattern the solargraph composed bundle
    should copy, and the reason a bare `.ruby-lsp/Gemfile.lock` existence test reads a healthy
    corpus as an unconfigured one.
    """
    if (directory / ".ruby-lsp" / "Gemfile.lock").exists():
        return "composed"
    gemfile = directory / "Gemfile"
    if gemfile.exists():
        for line in gemfile.read_text().splitlines():
            stripped = line.strip()
            if stripped.startswith("gem ") and "ruby-lsp" in stripped:
                return "in bundle"
    return "absent"


def locked_gems(directory):
    """Every gem name the lock resolves, read as text. Bundler is not needed to answer this."""
    lock = directory / "Gemfile.lock"
    if not lock.exists():
        return []
    seen = set()
    for line in lock.read_text().splitlines():
        stripped = line.strip()
        if line.startswith("    ") and not line.startswith("      ") and " (" in stripped:
            seen.add(stripped.split(" (", 1)[0])
    return sorted(seen)


def count_locked(directory):
    gems = locked_gems(directory)
    return len(gems) if gems else None


def print_status(rows):
    head = (f"{'corpus':<11}{'pin':<6}{'ruby':<9}{'bundle':<12}{'gems':>6}  "
            f"{'solargraph':<12}{'ruby-lsp':<10}dirty")
    print(head)
    print("-" * len(head))
    for row in rows:
        pin = "ok" if row["at_pin"] else ("MOVED" if row["head"] else "-")
        ruby = row["ruby"] or "MISSING"
        gems = "-" if row["gems_locked"] is None else str(row["gems_locked"])
        dirty = "clean" if not row["dirty"] else f"{len(row['dirty'])} file(s)"
        print(
            f"{row['name']:<11}{pin:<6}{ruby:<9}{row['bundle']:<12}{gems:>6}  "
            f"{row['solargraph_version'] or 'no bundle':<12}"
            f"{row['ruby_lsp']:<10}{dirty}"
        )
    # A duplicate strscan only reaches solargraph when solargraph runs *outside* the composed
    # bundle. With the bundle in place the duplicate is still installed and no longer loaded, so
    # this reports an exposure rather than a fault — and says which it is.
    exposed = [
        r for r in rows
        if (r.get("strscan") or {}).get("hover") == "broken" and not r["solargraph_bundle"]
    ]
    neutralised = [
        r for r in rows
        if (r.get("strscan") or {}).get("hover") == "broken" and r["solargraph_bundle"]
    ]
    if exposed:
        print()
        print("Two strscan builds and NO composed bundle — solargraph hover will raise, silently:")
        for row in exposed:
            state = row["strscan"]
            print(
                f"  {row['name']:<10} strscan {', '.join(state['duplicates'])}"
                f" beside default {state['default']}"
            )
        print("  Run `make corpora-solargraph` to compose one. Measured on mastodon: 513 request")
        print("  errors and 7/338 hovers without it, 0 and 427/811 with it.")
    if neutralised:
        print()
        print("Two strscan builds, neutralised by the composed bundle (informational):")
        for row in neutralised:
            state = row["strscan"]
            print(
                f"  {row['name']:<10} strscan {', '.join(state['duplicates'])} installed beside"
                f" default {state['default']}; solargraph loads one"
            )
        strays = [r for r in neutralised if r["strscan"]["cause"] == "stray"]
        for row in strays:
            dupes = " ".join(f"-v {v}" for v in row["strscan"]["duplicates"])
            print(
                f"    stray, nothing asks for it: ASDF_RUBY_VERSION={row['ruby']}"
                f" asdf exec gem uninstall strscan {dupes}   # {row['name']}"
            )
    missing = [r for r in rows if not r["ruby"]]
    if missing:
        print()
        print("Rubies this machine does not have (installing one is out of scope):")
        for row in missing:
            print(f"  asdf install ruby {row['declared_ruby']}   # {row['name']}")


# ----------------------------------------------------------------------------------- main


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "command",
        choices=[name for name, _, _ in STEPS] + ["status", "setup"],
        help="which step to run; `setup` runs them all in order",
    )
    parser.add_argument("--only", action="append", metavar="NAME", help="one corpus; repeatable")
    parser.add_argument(
        "--allow-nearest",
        action="store_true",
        help="accept another patch of the declared MAJOR.MINOR, and say so in the manifest",
    )
    parser.add_argument("--json", action="store_true", help="status as the manifest, on stdout")
    parser.add_argument("-v", "--verbose", action="store_true", help="echo the commands run")
    args = parser.parse_args()

    # Only the commands that drive a Ruby need asdf. Guarding the rest fails `clone`, which is a
    # git fetch — and `make canary-clone` is that command on a runner with no Ruby toolchain at
    # all. It was written unconditionally once and took CI's canary job down with it.
    if args.command not in RUBYLESS and not shutil.which("asdf"):
        print("corpora: asdf is not on PATH; this script drives Ruby through it", file=sys.stderr)
        return 2

    try:
        corpora = load_table(args.only)
    except Fail as failure:
        print(f"corpora: {failure}", file=sys.stderr)
        return 2

    if args.command == "status":
        rows = [observe(corpus, args) for corpus in corpora]
        if args.json:
            json.dump({"corpora": rows}, sys.stdout, indent=2)
            sys.stdout.write("\n")
        else:
            print_status(rows)
        return 0

    steps = STEPS if args.command == "setup" else [s for s in STEPS if s[0] == args.command]
    failures = 0
    for corpus in corpora:
        print(f"{corpus.name} ({corpus.license})")
        for name, step, _ in steps:
            try:
                print(f"  {name:<11} {step(corpus, args)}", flush=True)
            except Fail as failure:
                print(f"  {name:<11} FAILED: {failure}", flush=True)
                failures += 1
                break
    if failures:
        print(f"\ncorpora: {failures} corpus/corpora incomplete", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
