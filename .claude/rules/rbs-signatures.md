---
paths:
  - "src/workspace/rbs.rs"
  - "src/analysis/signatures.rs"
  - "vendor/rbs/**"
  - "build.rs"
---

# Ruby's own core and stdlib (RBS)

- **rubydex indexes RBS natively; the milestone was never about indexing.** `LanguageId::Rbs`,
  dispatched off the `.rbs` extension by `index_files`. What `workspace::rbs` owns is *which*
  copy of the signatures to use: `[rbs] path`, then the highest-versioned `rbs-*` gem on disk,
  then the copy `build.rs` embedded. Each rung has a test, and the bottom one is the only reason
  built-ins survive a machine with no Ruby.
- **The vendored copy is extracted to `<cache>/rbs-<version>/`, not indexed from memory.**
  rubydex keys documents by `Url::from_file_path` and go-to-definition has to answer with a URI
  the editor can open. The `.complete` marker is written last and holds the version, so a killed
  extraction is redone rather than half-trusted.
- **`cache_dir` picks that root in three rungs, and the last one is a deliberate choice.**
  `$XDG_CACHE_HOME/ya-lsp` when the variable is set, then `%LOCALAPPDATA%/ya-lsp/cache` on
  Windows, then `~/.cache/ya-lsp` — *not* `~/Library/Caches` on macOS, because this is
  developer-tool state someone may well want to `rm -rf` and every other language server on the
  machine puts it in `~/.cache`. With `HOME` unset too there is no cache directory and therefore
  no built-in signatures at all, which is a `Problem` and not a panic.
- **Indexing Ruby's core made the server faster, and the reason matters.** The steady resolve on a
  Rails app went 15.9 ms → 6.4 ms when 89 core signature files were added, because a bundle's
  references to `String`/`Hash`/`Kernel` had nothing to resolve against and pending work is what
  the resolver redoes. Adding names with nothing to resolve *against* them still costs: Ruby's own
  library is 727 files and +1 ms. Do not reason about graph size alone.
- **`ruby_lib_dirs` returns exactly one directory, requires a known Ruby version, and checks the
  whole path shape.** Taking every gem root's sibling would index macOS's Ruby 2.6 stdlib beside
  the project's 4.0. The ABI must match the resolved version and nothing is returned if none does
  — *including when no version resolves at all*, because macOS ships a vestigial 2.6 that
  `gem_roots` finds on every machine, and falling back to "the first root" made `"hello".u` offer
  `unspace` from a 2.6 `bigdecimal` patch on a machine with no Ruby. RVM's root is
  `~/.rvm/gems/ruby-<version>` — parent also named `gems` — so `lib/ruby/gems/<abi>` is
  checked in full.
- **A default gem's directory under `gems/` exists and is empty.** That is why they were silently
  dropped rather than reported unresolved: resolution succeeded and `load_paths_for` then found no
  `lib/`. The fix is Ruby's own library as a load path, held once on `Gems::ruby_lib` rather than
  attached to each of the forty gems inside it.
- **Every gem root reaches `gem_roots` through `Env`, the four absolute system paths included.**
  They used to be written into `gem_roots` itself, where nothing could steer them: a lockfile
  names an exact `name-version` directory a stranger's Ruby does not have, so they contributed
  nothing until Ruby's own library and rbs arrived with no such filter. `workspace::rbs`'s
  vendored-fallback tests then passed on a laptop with no Ruby and failed on CI, where a system
  Ruby's `rbs` gem answered `Discovered`. `Env::from_process` fills `system_roots` in;
  `Env::default` leaves it empty, which is what makes a fixture hermetic. The analysis harness
  still writes a `ya-lsp.toml` turning `default_gems` and `rbs` off, now only to skip ~800 files
  of signature work no test there asks about.
- **Which classes are "core" is rbs's call and it moves.** `Set` and `Pathname` are both in
  `core/` as of rbs 4.x. A test that needs a genuinely stdlib-only constant uses `OptionParser`.
- **Bumping `vendor/rbs` changes the answers ya-lsp gives.** Measured across rbs 3.10 → 4.1.3:
  76 declarations added, 82 removed out of ~3,640, and most removals were methods re-homed onto
  an ancestor. Treat it as a behaviour change, not a dependency update; `vendor/rbs/README.md`
  has the refresh procedure and the licence.
- **ya-lsp does not index RBS `interface` declarations, and edits the text to make that true.**
  rubydex does not model them: `visit_interface_node`'s default walks into the members and rubydex
  overrides it nowhere, so an interface's methods are filed on whatever lexical scope encloses the
  block — `Object` for most of them — and the interface itself never enters the graph.
  `analysis::signatures::without_interfaces` blanks each block before indexing, replacing every
  byte but the newlines with a space so offsets and line numbers in the rest of the file are
  exactly what they were. The alternative, filtering the results, would have to be repeated in
  completion, `workspace/symbol` *and* `locator` — the members are goto-definition targets too,
  and for `rand` the first target offered was `interface _Rand` in `core/array.rbs`.
- **The `%a{…}` annotations belong to the span even though the node says they do not.**
  `InterfaceNode::location()` starts at the keyword, so blanking that span alone leaves an
  annotation attached to nothing and rbs refuses the file with "cannot start a declaration". The
  first version of this did exactly that to `core/array.rbs`: rubydex indexed none of it, `[].`
  offered 7 methods instead of 197, and every test passed. **The edited text is re-parsed and
  discarded if rbs will not read it** — a file this cannot improve keeps its interfaces, which is
  where it started. Do not remove that check to save a parse of forty files.
- **There are three routes an `.rbs` file takes into the graph and the rule has to hold on all
  of them.** `workspace::rbs`'s signature root goes through `step_gem_indexing`; a project's own
  `sig/` goes through `index_workspace`, reached only when `index.include` is widened past its
  `**/*.rb` default; and a buffer goes through `index_buffer`. The first version wired the first
  and the third, and `Object#slurp` from a project's own `sig/` came straight back — indexed one
  way, filtered the other, and *opening* the file would have changed the graph. Both indexing
  routes now share `index_edited_signatures`. A gem's own `sig/` is not a fourth route:
  `gems::ruby_files` takes `.rb` and nothing else, and `rbs_collection` is not read at all.
- **`ruby-rbs` is a direct dependency at rubydex's own requirement, `"0.3"` and not `=0.3.0`.**
  `ruby-rbs-sys` links rbs's C library, so two copies collide at link time the way two `ruby-prism`
  copies do; matching the requirement rather than pinning tighter is what guarantees cargo resolves
  one.
- **Nothing reads RBS's types.** Signatures are indexed as declarations only. `(?symbol) -> String`
  is sitting in the graph and using it is return-type inference, which is out of scope.
