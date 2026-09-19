---
paths:
  - "src/workspace/gems.rs"
  - "src/workspace/bundler.rs"
  - "src/workspace/ruby_version.rs"
  - "src/workspace/config.rs"
  - "src/workspace/mod.rs"
---

# Gems, bundle discovery and the workspace walk

## Must: discovery

1. **Read `gems::Env`, never `std::env`.** `Workspace::load_with_env` is the seam that lets fixtures
   run against a temp directory.
2. **Stop the version-file walk at `Env::home`; `None` means don't walk.** Put a fixture's workspace
   *inside* its fixture home, or the walk escapes into `/tmp` and `/`.
3. **The nearest directory wins, then the file kind.** `.ruby-version` beats `.tool-versions` only
   within one directory. The whole chain beats the lockfile's `RUBY VERSION`. `gems::roots` and
   `gems::discover` must pass the same ceiling.
4. **Glob the ABI directory; never compute it.** Ruby 4.0.1 installs into `4.0.0/`. `abi_of` only
   ranks the globbed results.
5. **Dedupe gem roots by canonical path, but store them as spelled.** Storing the canonical form
   forks a second document per file behind a symlink (every macOS temp directory).

## Three path lists per gem, never merged

| List | Holds | Used for |
|---|---|---|
| `load_paths` | `require_paths`, from `specifications/<name>.gemspec` (git and path sources fall back to `lib`; absolute entries are skipped) | `require` resolution and indexing |
| `signature_paths` | `sig/` | indexing only. On a load path, a `require` would jump to RBS |
| `engine_paths` | `ENGINE_DIRS`: `app/` and `config/` | indexing only. On a load path, `require "thing"` could change meaning |

- **`config/` is walked for `routes.rb`.** `rails::Whose` decides whether an engine's routes draw
  into the host's helpers (activestorage) or into the engine's own (`blazer.queries_path`). It uses
  the receiver *and* the file location.
- **`source_files` takes `.rb` and `.rbs` only.**
- **Count unresolved gems per name, not per platform spec.**

## Background indexing

- **Gem chunks set `dirty` but never arm `resolve_at`.** Arming it per chunk delays the user's own
  diagnostics for the whole index. `serve` checks `dirty`.
- **`GEM_FILES_PER_STEP` is a measured value.** Re-measure against a real bundle before changing it.

## The workspace walk

1. **Read no ignore files at all.** `walker` switches off every `ignore` source explicitly;
   `index.exclude` does the whole job.
2. **Prune excluded trees at the directory** (`pruning_walker`, `filter_entry`).
3. **Every predicate respects pruning** (`prunes_an_ancestor`), so the watcher and the index cannot
   disagree about a file inside a pruned tree.
4. **`index.include` and `index.exclude` replace the defaults; they don't extend them.** That is
   deliberate: the README prints both defaults in full. There is no negation (`!keep.rb`).
5. **Exactly one non-Ruby file is read: `db/structure.sql`**, through `Workspace::admits` (`indexes`
   minus the Ruby-shape half, with the same excludes). Keep it that narrow (`core-invariants.md`).

## Your own code vs someone else's

- **`is_own_code` excludes gem roots, the RBS root and Ruby's library**, even when they sit inside
  the workspace, as a vendored bundle does. Otherwise a Rails app gets hundreds of squiggles it
  cannot fix.
- **`.gem_rbs_collection/` is a foreign prefix.** It is hidden, so it is reached only as a signature
  path. The path given in `rbs_collection.yaml` is not read, because there is no YAML parser.

## `[index] load_paths`

- **`resolve_load_path` canonicalizes a load path** (`..`, symlinks), then re-spells it under the
  root's own prefix when it is inside the root.
- **A load path inside the root belongs to the normal walk.** One outside the root goes to
  `Workspace::external_load_paths` → `Analysis::collect_external_load_paths`, which takes `.rb` and
  `.rbs` within `index.max_files`. `register_documents` asks the client to claim it.
- **Known limit:** opening a file through a different symlink spelling forks a second document.
  `DocUri` normalises encoding, not symlinks.
