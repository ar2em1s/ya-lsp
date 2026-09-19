---
name: ya-lsp-setup
description: Install, update or diagnose the ya-lsp Ruby language server for this Claude Code plugin. Use it when the LSP tool answers nothing for a Ruby file, when goToDefinition into a gem comes back empty, when ya-lsp is not on PATH, or to check whether the installed binary is the current release. Running it a second time is how the binary is updated.
---

# ya-lsp setup

`ya-lsp` is a Ruby language server written in Rust. The plugin runs a bare `ya-lsp` found on
`PATH` — there is no server-path setting — so setup is: get the binary onto `PATH`, then ask the
server a question and see what it answers.

**This is not a bootstrap.** You are reading it from inside the plugin, so the plugin is already
installed. Plugin first, binary second.

**It is re-runnable, and that is the update path.** A second run compares the installed binary
against the latest release and offers to replace it. Nothing else announces a new version.

**Two rules that hold for every step below.** Never download and never write a file without
asking first. And never diagnose by reading the server's own output: Claude Code drops
`window/showMessage` and `window/logMessage` entirely, and the log file would go inside the
user's project. Ask the server questions with the `LSP` tool instead — that is what step 3 is.

## Step 1 — which binary, and is it current

```bash
command -v ya-lsp && ya-lsp --version
curl -sI -o /dev/null -w '%{url_effective}\n' -L https://github.com/ar2em1s/ya-lsp/releases/latest
```

The first prints `ya-lsp <version>`, or nothing at all. The second redirects to
`.../releases/tag/v<version>` — that is the latest release, and it needs no `jq` and no `gh`.

| what is on `PATH` | what to do |
| --- | --- |
| nothing | step 2, install |
| a version older than the release | say both versions, offer to replace it, then step 2 |
| the latest | change nothing, say so, go to step 3 |

**If `command -v` found nothing, look for an installed one before offering to download.** The
common case is a binary that is there and unreachable: `~/.local/bin` is added to `PATH` by
`~/.zshrc`, an interactive shell reads that file and the shell Claude Code spawns does not, so
`ya-lsp` exists and `command -v` cannot see it.

```bash
ls -l "$HOME/.local/bin/ya-lsp" "$HOME/.cargo/bin/ya-lsp" 2>/dev/null
```

Found one? Then the fix is `PATH`, not a download: move the `export PATH=` line into `~/.zshenv`,
which every shell reads, or link the binary into a directory that is already on `PATH`
unconditionally. Say which you did and why, and skip step 2.

**If `command -v` found a symlink, say where it points before offering to replace it.** A
developer of ya-lsp keeps `~/.local/bin/ya-lsp` pointed at their own `target/release/ya-lsp`;
overwriting that swaps their build for a release and the next `cargo build` does not come back.
`ls -l "$(command -v ya-lsp)"` is the check.

**If the version looks wrong for what is installed, `which -a ya-lsp`.** A second copy earlier on
`PATH` answers every request while the one the user believes in sits further down and looks fine.
The server that is really running is the one `lsof` names for the `ya-lsp --stdio` process, not
the one `--version` prints.

## Step 2 — install it, after asking

**Ask first.** On a first install and on an update alike.

Pick the archive from `uname -sm`:

| `uname -sm` | archive |
| --- | --- |
| `Darwin arm64` | `ya-lsp-aarch64-apple-darwin.tar.gz` |
| `Linux x86_64` | `ya-lsp-x86_64-unknown-linux-gnu.tar.gz` |
| `Linux aarch64` | `ya-lsp-aarch64-unknown-linux-gnu.tar.gz` |
| Windows, x64 | `ya-lsp-x86_64-pc-windows-msvc.zip` |
| Windows, arm64 | `ya-lsp-aarch64-pc-windows-msvc.zip` |
| `Darwin x86_64` | **no archive** — see below |

**Where it goes**: the directory the old `ya-lsp` was found in, if there was one — an update
replaces in place rather than installing a second copy for `PATH` order to choose between.
Otherwise a directory already on `PATH` that the user can write, which is `~/.local/bin` where it
exists. There is no other place the plugin looks.

```bash
dir="$HOME/.local/bin"          # or the directory the old binary was in
name="ya-lsp-aarch64-apple-darwin.tar.gz"   # from the table
base="https://github.com/ar2em1s/ya-lsp/releases/latest/download"

cd "$(mktemp -d)"
curl -sSL -o "$name" "$base/$name"
curl -sSL -o SHA256SUMS "$base/SHA256SUMS"
grep " $name\$" SHA256SUMS | shasum -a 256 -c -    # or: sha256sum -c -
tar xzf "$name" --strip-components=1               # the archive holds one directory
mkdir -p "$dir" && mv ya-lsp "$dir/ya-lsp"
"$dir/ya-lsp" --version
```

The checksum line is not optional, and the `grep` before it is what makes it pass: `SHA256SUMS`
covers every asset of the release — the other archives and the VSIXs — and `-c` fails on each one
that was not downloaded. On Windows the archive is a `.zip`: `Expand-Archive`, and
`Get-FileHash -Algorithm SHA256` against the same line.

**An Intel Mac has no archive.** The release matrix builds five targets natively, because the
server compiles C and a cross build would need a full target toolchain; `x86_64-apple-darwin` is
not one of them. So check for cargo before saying anything:

```bash
cargo --version
```

With cargo: `cargo install --git https://github.com/ar2em1s/ya-lsp --tag v<version>`, which puts
it in `~/.cargo/bin` — one package, one binary, so it needs no crate name. Without cargo, do not
print a command that will fail: say that building it needs a Rust toolchain (`rustup`, a few
hundred megabytes, and several minutes of compiling), and let the user decide.

**After installing, the server may already be running as a failed one.** If the binary was missing
when the session started, that server is in the `error` state; Claude Code starts a stopped or
errored server again on its next use, so ask one `LSP` question first. Only if that still answers
nothing is `/reload-plugins` needed. (A developer who has just rebuilt the binary in place wants
neither: `pkill -f 'ya-lsp --stdio'`, then ask again.)

## Step 3 — ask the server three questions

Use the `LSP` tool. It is a **deferred tool** — load it with `ToolSearch` for `select:LSP` before
the first call. Every operation takes `filePath`, `line` and `character`, `workspaceSymbol`
included, because the position is how the client picks the server; pass a real file and a real
position there too.

Pick the file from the extensions the plugin routes — `.rb`, `.rake`, `.gemspec`, `.ru`, `.erb`,
and the Rails view handlers that are plain Ruby, `.jbuilder`, `.builder` and `.ruby` — and not
one git ignores, because Claude Code drops every answer in a git-ignored file. `Gemfile`
and `Rakefile` have no extension and are unreachable from here whatever the server does.

1. **`documentSymbol`** on a Ruby file in the project. It answers, or the server is not running.
   Empty here and nothing else in this step will mean anything.
2. **`workspaceSymbol`** for a class the project declares. It answers, or the workspace was not
   indexed — which is the session root being somewhere other than the project.
3. **`goToDefinition` on a constant the project *names* and does not *declare*.** This is the one
   that says whether the bundle was found. To pick the constant:
   - grep the project's own Ruby for `class X < Y` and `include Y`;
   - take the first `Y` that `workspaceSymbol` does not answer from a file inside the project —
     `ActiveRecord::Base`, `Sidekiq::Job`, `Rack::Test::Methods` are the usual ones;
   - `goToDefinition` on it, and check the answer's path is under a gem directory.

   A project that names no foreign constant at all: **skip this step and say it was skipped.**
   Do not report a pass for a check that did not run.

An empty answer at step 3 is the only symptom of a missing bundle an agent can see — the server's
own sentence about it never arrives here. Step 4 is what to do about it.

## Step 4 — when the gem jump comes back empty

**Ask these three in order and stop at the first `no`.** Printing six settings at somebody whose
problem is the first question is how a diagnosis becomes a wall of text.

1. Is there a `Gemfile.lock` at the session root? If not, the app is probably in a subdirectory.
2. Is `BUNDLE_GEMFILE` set, and does it point at a Gemfile that exists?
3. Does `gem env gemdir` name a directory that is not already being searched?

**Where the server looks, so you can tell which rung went missing.** `[gems] paths` first, then
`GEM_HOME` and `GEM_PATH`, then Bundler's own path and `vendor/bundle`, then the version managers
— `ASDF_DATA_DIR` or `~/.asdf`, `MISE_DATA_DIR` or `~/.local/share/mise`, `~/.rbenv/versions`,
`~/.rubies`, `~/.rvm/gems` — then system-wide installs, then `--user-install`. Each is filtered by
the Ruby version the project asks for, and a directory counts only if it holds a `gems/`.
`GEM_HOME` is one rung of seven, so setting it proves nothing: a project whose gems the version
manager also holds answers exactly the same with `GEM_HOME` pointed at an empty directory.
**A stale `ASDF_DATA_DIR` or `MISE_DATA_DIR` inherited from some other shell is a real cause** —
it replaces the default rather than adding to it, so the gems are suddenly nowhere.

Then name the one fix, from this table. Everything here is one line in `ya-lsp.toml` at the
workspace root, or one environment variable — there is no `initializationOptions` and no
Claude-Code-specific place to configure this server.

| the layout | what it looks like | the fix |
| --- | --- | --- |
| the app is in a subdirectory of the repo | project symbols fine, gem jumps empty | start the session in that subdirectory, **or** export `BUNDLE_GEMFILE=services/api/Gemfile` before `claude` — the lockfile follows the variable even though the root does not |
| several apps in one repo | one answers, the others do not | one session per app |
| a shared tree outside the root (`../shared/lib`) | nothing in it is indexed | `[index] load_paths` |
| gems somewhere the built-in search does not look — a container image, Nix, a custom prefix | every gem jump empty, with a lockfile present | `[gems] paths`, each a directory that holds a `gems/` — what `gem env gemdir` prints. `GEM_HOME`, `GEM_PATH` and `BUNDLE_PATH` are read from the environment too |
| a version-manager variable points somewhere empty | every gem jump empty, and `gem env gemdir` names a directory that does exist | unset the stale `ASDF_DATA_DIR` / `MISE_DATA_DIR` in the shell that launches `claude`, rather than writing `[gems] paths` — a path pinned to one Ruby breaks again at the next version bump |
| the wrong Ruby was detected | stdlib and default gems from another version | `[gems] ruby_version`, and `[rbs] path` for the signatures |
| a very large repo | indexing stops at a ceiling | `[index] include` / `exclude` / `max_files`, `[gems] max_files` |

```toml
# ya-lsp.toml, at the workspace root
[gems]
paths = ["/usr/local/bundle"]

[index]
load_paths = ["lib", "app", "../shared/lib"]
```

**Offer to write it and ask first.** Print the exact block, say which file it goes in, and write
nothing until the user says yes. `[index] include` and `[index] exclude` *replace* the defaults
rather than extending them, so a block that sets one of those has to spell the whole list — the
README prints both defaults in full.

**A bundle vendored into `vendor/bundle` is not the problem.** The server indexes it like any
other gem root, measured. `vendor/**/*` is on the default `index.exclude`, but that list governs
the project's own walk and gems arrive on a different pass.

## When it is done

Say which of the three probes answered, and for each empty one, the single next thing to try.
If anything was installed, say the version that is now on `PATH` and where it went.

Run this skill again whenever a Ruby answer looks wrong or missing — and once in a while
regardless, because a second run is how the server gets updated.
