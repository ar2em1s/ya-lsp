---
paths:
  - "src/workspace/gems.rs"
  - "src/workspace/bundler.rs"
  - "src/workspace/ruby_version.rs"
  - "src/workspace/config.rs"
  - "src/workspace/mod.rs"
---

# Gems and bundle discovery

- **Gem discovery reads `gems::Env`, never `std::env` directly.** That is the only reason the layout
  fixtures can run against a temp directory instead of whatever Ruby the machine has.
  `Workspace::load_with_env` is the seam.
- **The version-file walk's ceiling is `Env::home`, and `None` means "do not walk".**
  `ruby_version::resolve` reads `.ruby-version` then `.tool-versions` from the workspace root
  upwards, stopping *after* `$HOME` or at `/`. A fixture whose ceiling is not an ancestor of its
  workspace walks out of its own temp directory and reads whatever `/tmp` and `/` hold. Put a
  fixture's workspace *inside* its fixture home.
- **Nearest directory first, kind second.** `.ruby-version` outranks `.tool-versions` only *within*
  one directory; across the chain the nearer file wins whichever kind it is, and the whole chain
  outranks `RUBY VERSION` in the lockfile — that records what the last person to run bundler used,
  and the question is which interpreter will run this code. `gems::roots` and `gems::discover`
  resolve independently and must pass the same ceiling, or `rbs::discover` searches one Ruby's tree
  while the gem index searches another's.
- **The ABI directory is globbed, never computed.** Ruby 4.0.1 installs into `lib/ruby/gems/4.0.0/`.
  `abi_of` exists only to *prefer* a globbed directory.
- **Gem roots are deduplicated by canonical path but stored as spelled.** Storing the canonical form
  breaks a vendored bundle on any machine whose workspace root reaches disk through a symlink (every
  macOS temp directory): its files get a different URI spelling from everything else, forking a
  second document per file.
- **Diagnostics are filtered by URI prefix, and the workspace prefix alone is not enough.** A
  vendored bundle is inside the workspace root by construction, so `is_own_code` also excludes the
  gem roots, the RBS root and Ruby's own library. Without it, opening a Rails app publishes hundreds
  of unfixable squiggles.
- **Background gem chunks set `dirty` but must not arm `resolve_at`.** Arming it per chunk pushes the
  user's own diagnostics out for the whole index. Conversely, `serve` must check `dirty`, not
  `resolve_at`, or requests answer against an unresolved graph during the index.
- **`GEM_FILES_PER_STEP` is measured, not chosen.** Its cost is the resolve the *next request* runs
  over what the step added, and that cost rises sharply once the step grows past its current value.
  Re-measure (`benchmarking.md`) before changing it.
- **`require_paths` comes from `specifications/<full name>.gemspec`**, RubyGems' serialised gemspec,
  a plain array literal. Git and path sources have no such file — only the project's own
  arbitrary-Ruby `.gemspec` — so they fall back to `lib`. Absolute entries are native-extension stubs
  and are skipped, the same rule rubydex's Ruby-side `graph.rb` applies.
- **Unresolved gems are counted per name, not per spec.** A lockfile resolved for seven platforms
  lists `nokogiri` seven times, and six of those directories will never exist here.
- **`signature_paths` is a second list answering a second question, and must never merge into
  `load_paths`.** `load_paths` is what `require "..."` resolves against; `sig/` is on no load path,
  so a `sig/` leaking in would make go-to-definition on a `require` land on a signature rather than
  the code. The gem walk consumes both lists; require resolution consumes one.
- **RBS adoption in a real bundle is a few per cent, and the item is worth it because of what it
  costs.** Only a handful of a real bundle's gems ship `sig/` at all, and most of the methods that
  buys come from two gems no application chains through. No gem ships `.rbs` under a `lib/` require
  path. The walk is one `is_dir` per gem and the indexing is inside the noise of the background pass.
  Do not repeat "a growing number of gems ship RBS" as if it were measured — measure it.
- **`engine_paths` is a *third* list, for `signature_paths`' reason and with sharper teeth.** A Rails
  engine ships models, mailers, jobs and controllers under `app/` and declares
  `require_paths = ["lib"]` all the same — checked in the serialised gemspecs of `activestorage`,
  `actionmailbox`, `devise`, `solid_queue`, `turbo-rails`, `activeadmin`. So `app/` has to be walked
  and must **not** become a load path: a gem can ship `lib/thing.rb` *and* `app/thing.rb`, and an
  `app/` on the load path would silently change what `require "thing"` means. Walked after the
  signatures, before the load paths, inside `[gems] max_files`.
- **`ENGINE_DIRS` is two entries, and `config/` earns its place with one file and no constant.**
  Across every gem installed for one Ruby there are **barely a handful of `.rb` files** under any
  `config/` — `routes.rb` and `importmap.rb` — and **none defines a class or module**, so it contributes
  nothing to the constants gate 1 exists for. It is walked because an engine's `config/routes.rb` may
  name the *host application's* helpers. Both entries are on the engine list rather than the load
  path, so `require "thing"` cannot start meaning `config/thing.rb`.
- **Which helpers those are is `rails::Whose`, and the split is nearly even.** Of the gems shipping
  a routes file, **some draw into the application's set** — activestorage, actionmailbox,
  turbo-rails, solid_queue — and **the rest draw into their own**: blazer, pghero,
  mission_control-jobs, whose helpers are reached as `blazer.queries_path` after a `mount`. The
  discriminator is receiver **and** file location, not receiver alone, because an engine monorepo
  writes `Engine.routes.draw` in its own routes file for its own routes. Checked against
  `ActionDispatch::Routing::RouteSet` with a real set bound to `Rails.application.routes`: **every
  helper named, none invented, none missed** — and Rails agrees the own-set engines give the host
  nothing.
- **Demand for it is one call site in the whole corpus** (a `rails_direct_uploads_url` in a request
  spec). It is a correctness feature, not a coverage one: the routes reader was one parameter away,
  and the alternative was a reader that could not say why it declined.
- **What gate 1 buys, measured as a floor rather than an estimate.** Over four applications,
  constant references **gain a `textDocument/definition` and none loses one**, every one landing in a
  gem's `app/`; `ActiveStorage::Blob` is the largest single group. One position gains a *second*
  place rather than a first: `ActionView::Helpers::FormHelper` is reopened by actiontext's
  `app/helpers/`, which is true and was invisible. The corpora's bundles are only partly installed,
  so the real gain is larger; the engines themselves were installed on purpose.
- **The walk costs a couple of per cent.** `app/` and `config/` hold a small fraction of what
  `lib/` does across every gem installed for one Ruby, so adding them barely moves the indexing
  queue. `source_files` takes `.rb` and `.rbs` only, so an engine's `app/javascript` is excluded
  without a rule.
- **`.gem_rbs_collection/` is inside the workspace root, so it must be a foreign prefix.** Same shape
  as a vendored bundle and the same failure: without it, every squiggle in somebody else's curated
  signatures is published as the user's own. It is also *hidden*, so the workspace walk prunes it at
  the directory — it reaches the graph only as a signature path on the background pass. The path from
  `rbs_collection.yaml` is not honoured: that is YAML, and this crate has no parser for it.
- **One non-Ruby file this workspace reads, and one predicate for it.** `index.include` is a list of
  the shapes *Ruby* is written in, so `Workspace::indexes` answers no to a `db/structure.sql` however
  the user spells the list — which would make reading a SQL dump impossible rather than configurable.
  `Workspace::admits` is `indexes` minus that half: the same compiled globs and the same
  `ignore::WalkBuilder`, so `index.exclude`, `.gitignore` and the hidden-file rule still apply and a
  project excluding `db/` gets no dump read. Deliberately narrow — one caller, one file type — rather
  than a general "read anything" door. `core-invariants.md` holds it to the same strictly-wider
  property the other two share.
