---
paths:
  - "src/workspace/gems.rs"
  - "src/workspace/bundler.rs"
  - "src/workspace/ruby_version.rs"
  - "src/workspace/config.rs"
  - "src/workspace/mod.rs"
---

# Gems and bundle discovery

- **Gem discovery reads `gems::Env`, never `std::env` directly.** That is the only reason the
  layout fixtures can run against a temp directory instead of whatever Ruby the machine has.
  `Workspace::load_with_env` is the seam.
- **The version-file walk's ceiling is `Env::home`, and `None` means "do not walk".**
  `ruby_version::resolve` reads `.ruby-version` then `.tool-versions` from the workspace root
  upwards, stopping *after* `$HOME` or at `/`, whichever it reaches first. The ceiling is an
  argument for the same reason gem discovery reads `Env` at all: a fixture whose ceiling is not
  an ancestor of its workspace walks out of its own temp directory and reads whatever `/tmp` and
  `/` happen to hold. Put a fixture's workspace *inside* its fixture home.
- **Nearest directory first, kind second.** `.ruby-version` outranks `.tool-versions` only
  *within* one directory; across the chain the nearer file wins whichever kind it is, and the
  whole chain outranks `RUBY VERSION` in the lockfile — that records what the last person to run
  bundler used, and the question is which interpreter will run this code. `gems::roots` and
  `gems::discover` resolve independently and must pass the same ceiling, or `rbs::discover`
  searches one Ruby's tree while the gem index searches another's.
- **The ABI directory is globbed, never computed.** Ruby 4.0.1 installs into
  `lib/ruby/gems/4.0.0/`. `abi_of` exists only to *prefer* a globbed directory.
- **Gem roots are deduplicated by their canonical path but stored as spelled.** Storing the
  canonical form breaks a vendored bundle on any machine whose workspace root reaches disk
  through a symlink (every macOS temp directory): its files get a different URI spelling from
  everything else, forking a second document per file.
- **Diagnostics are filtered by URI prefix, and the workspace prefix alone is not enough.** A
  vendored bundle is inside the workspace root by construction, so `is_own_code` also
  excludes the gem roots, the RBS root, and Ruby's own library. Without it, opening a Rails app
  publishes 208 unfixable squiggles.
- **Background gem chunks set `dirty` but must not arm `resolve_at`.** Arming it per chunk pushes
  the user's own diagnostics out for the whole index. Conversely, `serve` must check `dirty`, not
  `resolve_at`, or requests answer against an unresolved graph during the index.
- **`GEM_FILES_PER_STEP` is measured, not chosen.** Its cost is the resolve the *next request*
  runs over what the step added — p90 goes 94/117/127/333 ms at 50/100/200/400. Re-measure before
  changing it.
- **`require_paths` comes from `specifications/<full name>.gemspec`**, RubyGems' serialised
  gemspec, which is a plain array literal. Git and path sources have no such file — only the
  project's own arbitrary-Ruby `.gemspec` — so they fall back to `lib`. Absolute entries are
  native-extension stubs and are skipped, the same rule rubydex's Ruby-side `graph.rb` applies.
- **Unresolved gems are counted per name, not per spec.** A lockfile resolved for seven platforms
  lists `nokogiri` seven times and six of those directories will never exist here.
