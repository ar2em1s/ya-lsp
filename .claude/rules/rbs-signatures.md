---
paths:
  - "src/workspace/rbs.rs"
  - "src/analysis/signatures.rs"
  - "vendor/rbs/**"
  - "build.rs"
---

# Ruby's core and stdlib (RBS)

rubydex indexes `.rbs` itself. This area decides **which** signatures to use and **what to strip**
before indexing.

## Which copy, in rung order

1. `[rbs] path`
2. the highest-versioned `rbs-*` gem on disk
3. the copy `build.rs` embeds, extracted to `<cache>/rbs-<version>/`

Every rung has a test. The third is the only reason built-in types work on a machine with no Ruby.

- **Extract the embedded copy; never index it from memory.** Go-to-definition must answer with a URI
  the editor can open. The `.complete` marker is written last, so a killed extraction is redone.
- **`cache_dir` checks three places, in order:** `$XDG_CACHE_HOME/ya-lsp`, then
  `%LOCALAPPDATA%/ya-lsp/cache` on Windows, then `~/.cache/ya-lsp`, including on macOS. With `HOME`
  unset there is no cache, which is a `Problem`, not a panic.

## Must

1. **Strip RBS `interface` blocks before indexing** (`signatures::without_interfaces`). rubydex
   files their methods on the enclosing scope, usually `Object`. Blank every byte except newlines,
   so offsets and line numbers stay the same.
2. **Blank the `%a{…}` annotations with the interface**, even though `InterfaceNode::location()`
   starts at the keyword. A leftover annotation makes rbs reject the whole file.
3. **Never remove the re-parse check.** The edited text is parsed again, and thrown away if rbs
   rejects it. Without that check, a broken `core/array.rbs` passed every test.
4. **Every route into the graph applies the rule**: `index_workspace`, `step_bundle` (gem `sig/`,
   `.gem_rbs_collection/`, the rbs root) and `index_buffer`. Both indexing routes go through
   `index_edited_signatures`, so a new source of RBS is a new directory, not a new route.
5. **Match rubydex's `ruby-rbs` requirement (`"0.3"`); never pin it tighter.** Two copies of
   `ruby-rbs-sys` collide at link time.

## Ruby's library directory

- **`ruby_lib_dirs` returns one directory, needs a known Ruby version, and checks the whole path.**
  With no version it returns nothing. Otherwise macOS's leftover Ruby 2.6 gets indexed.
  - RVM's root `~/.rvm/gems/ruby-<v>` has a parent named `gems` too, so `lib/ruby/gems/<abi>` is
    checked in full.
- **Default gems have an empty directory under `gems/`.** Ruby's library is held once as a load
  path, on `Gems::ruby_lib`.
- **Every gem root reaches `gem_roots` through `Env`.** `Env::from_process` fills `system_roots`,
  and `Env::default` leaves it empty, which keeps fixtures hermetic.

## Keep in mind

- **Bumping `vendor/rbs` changes answers.** Declarations are added and removed. Treat a bump as a
  behaviour change. The procedure is in `vendor/rbs/README.md`.
- **rbs decides what counts as core, and that moves.** `Set` and `Pathname` are in `core/` as of
  rbs 4.x. For a stdlib-only constant in a test, use `OptionParser`.
- **Return types are harvested separately**, by `analysis::types` on the analysis thread
  (`types.md`). It skips `interface` blocks too (`Harvest::visit_interface_node`), because it reads
  the file before it is edited. That makes two copies of one rule, so keep them in step.
- **Don't judge cost by graph size.** Adding core signatures made resolution faster, because
  references to `String` finally had something to resolve against.
