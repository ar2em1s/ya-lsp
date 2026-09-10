---
paths:
  - "src/workspace/rbs.rs"
  - "src/analysis/signatures.rs"
  - "vendor/rbs/**"
  - "build.rs"
---

# Ruby's own core and stdlib (RBS)

- **rubydex indexes RBS natively, so none of this is about indexing.** `LanguageId::Rbs`, dispatched
  off the `.rbs` extension by `index_files`. `workspace::rbs` owns *which* copy of the signatures to
  use: `[rbs] path`, then the highest-versioned `rbs-*` gem on disk, then the copy `build.rs`
  embedded. Each rung has a test; the bottom one is the only reason built-ins survive a machine with
  no Ruby.
- **The vendored copy is extracted to `<cache>/rbs-<version>/`, not indexed from memory.** rubydex
  keys documents by `Url::from_file_path`, and go-to-definition has to answer with a URI the editor
  can open. The `.complete` marker is written last and holds the version, so a killed extraction is
  redone rather than half-trusted.
- **`cache_dir` picks that root in three rungs, and the last is a deliberate choice.**
  `$XDG_CACHE_HOME/ya-lsp` when set, then `%LOCALAPPDATA%/ya-lsp/cache` on Windows, then
  `~/.cache/ya-lsp` — *not* `~/Library/Caches` on macOS, because this is developer-tool state
  someone may want to `rm -rf` and every other language server puts it in `~/.cache`. With `HOME`
  unset there is no cache directory and therefore no built-in signatures, which is a `Problem` and
  not a panic.
- **Indexing Ruby's core made the server faster.** The steady resolve on a Rails app more than
  halved when the core signature files were added, because a bundle's references to
  `String`/`Hash`/`Kernel` had nothing to resolve against and pending work is what the resolver
  redoes. Adding names with nothing to resolve *against* them still costs a little: Ruby's own
  library is far more files for a fraction of the gain. Do not reason about graph size alone.
- **`ruby_lib_dirs` returns exactly one directory, requires a known Ruby version, and checks the
  whole path shape.** Taking every gem root's sibling would index macOS's Ruby 2.6 stdlib beside the
  project's 4.0. The ABI must match the resolved version, and nothing is returned if none does —
  *including when no version resolves at all*, because macOS ships a vestigial 2.6 that `gem_roots`
  finds on every machine, and falling back to "the first root" made `"hello".u` offer `unspace` from
  a 2.6 `bigdecimal` patch on a machine with no Ruby. RVM's root is `~/.rvm/gems/ruby-<version>`,
  whose parent is also named `gems`, so `lib/ruby/gems/<abi>` is checked in full.
- **A default gem's directory under `gems/` exists and is empty.** That is why they were silently
  dropped rather than reported unresolved: resolution succeeded and `load_paths_for` then found no
  `lib/`. The fix is Ruby's own library as a load path, held once on `Gems::ruby_lib` rather than
  attached to each of the forty gems inside it.
- **Every gem root reaches `gem_roots` through `Env`, the four absolute system paths included.**
  They used to be written into `gem_roots` itself, where nothing could steer them: a lockfile names
  an exact `name-version` directory a stranger's Ruby does not have, so they contributed nothing
  until Ruby's own library and rbs arrived with no such filter. `workspace::rbs`'s vendored-fallback
  tests then passed on a laptop with no Ruby and failed on CI, where a system Ruby's `rbs` gem
  answered `Discovered`. `Env::from_process` fills `system_roots`; `Env::default` leaves it empty,
  which is what makes a fixture hermetic. The analysis harness still writes a `ya-lsp.toml` turning
  `default_gems` and `rbs` off, now only to skip signature work no test there asks about.
- **Which classes are "core" is rbs's call and it moves.** `Set` and `Pathname` are both in `core/`
  as of rbs 4.x. A test needing a genuinely stdlib-only constant uses `OptionParser`.
- **Bumping `vendor/rbs` changes the answers ya-lsp gives.** Across one minor-version bump,
  declarations were both added and removed, most removals being methods re-homed onto an ancestor.
  Treat it as a behaviour change, not a dependency update. `vendor/rbs/README.md` has the
  refresh procedure and the licence.
- **ya-lsp does not index RBS `interface` declarations, and edits the text to make that true.**
  rubydex does not model them: `visit_interface_node`'s default walks into the members and rubydex
  overrides it nowhere, so an interface's methods are filed on whatever lexical scope encloses the
  block — `Object` for most — and the interface never enters the graph.
  `analysis::signatures::without_interfaces` blanks each block before indexing, replacing every byte
  but the newlines with a space so offsets and line numbers elsewhere are unchanged. Filtering the
  results instead would have to be repeated in completion, `workspace/symbol` *and* `locator` — the
  members are goto-definition targets too, and for `rand` the first target offered was
  `interface _Rand` in `core/array.rbs`.
- **The `%a{…}` annotations belong to the span even though the node says they do not.**
  `InterfaceNode::location()` starts at the keyword, so blanking that span alone leaves an annotation
  attached to nothing and rbs refuses the file with "cannot start a declaration". The first version
  did exactly that to `core/array.rbs`: rubydex indexed none of it, `[].` offered a handful of
  methods instead of the whole class, and every test passed. **The edited text is re-parsed and discarded if rbs will not read
  it** — a file this cannot improve keeps its interfaces. Do not remove that check to save a parse of
  forty files.
- **Three routes take an `.rbs` file into the graph, and the rule has to hold on all three.**
  `index_workspace` for the project's own (`index.include` covers `**/*.rbs` by default, so no
  configuration needed); `step_gem_indexing` for everything outside the workspace — `workspace::rbs`'s
  signature root, each gem's `sig/`, `.gem_rbs_collection/`; and `index_buffer` for a file the editor
  opened. The first version wired two of three, and `Object#slurp` from a project's own `sig/` came
  straight back — indexed one way, filtered the other, and *opening* the file would have changed the
  graph. Both indexing routes share `index_edited_signatures`, which keeps a new source of RBS from
  needing a fourth route: **a directory is a new place to look, not a new rule.**
- **`ruby-rbs` is a direct dependency at rubydex's own requirement, `"0.3"` and not `=0.3.0`.**
  `ruby-rbs-sys` links rbs's C library, so two copies collide at link time the way two `ruby-prism`
  copies do; matching the requirement rather than pinning tighter is what guarantees cargo resolves
  one.
- **rubydex indexes signatures as declarations only; ya-lsp reads their return types itself.** That is
  `analysis::types`, not this module: a second `Visit` over a second parse of the same files,
  harvesting `MethodDefinition` -> the class it declares into a table beside the graph. `types.md` has
  the policy; the cost belongs here. **Every `.rbs` path is read on the analysis thread**, not just
  the minority holding an `interface` — reading and parsing them is inside the noise of a background
  pass that already takes seconds. Indexing them from the string that read already holds would avoid
  the double read and is much worse: it would move megabytes of parsing off the worker threads onto
  this one.
- **The harvest skips `interface` blocks too, for the reason the text edit exists.** Their members
  never enter the graph, so an entry for one would be keyed by a declaration that does not exist — or
  worse, by the enclosing class's. That is a second copy of one rule, worth watching:
  `signatures::without_interfaces` enforces it on the text and `types::Harvest::visit_interface_node`
  on the walk, separate because the harvest reads the file *before* the edit.
