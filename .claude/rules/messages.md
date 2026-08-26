---
paths:
  - "src/messages.rs"
  - "src/workspace/config.rs"
  - "src/workspace/mod.rs"
  - "src/workspace/gems.rs"
  - "src/workspace/rbs.rs"
  - "src/analysis/mod.rs"
---

# What the server says

- **Every sentence a user reads lives in `src/messages.rs`, one `pub fn` per message.** Not
  beside the code that raises it. Before v0.2.0 they were spread over five files and seventeen
  sites answered the same four questions independently — how to spell a setting, where to break
  the clause, whether to name a remedy, whether to end with a period — and no two agreed on all
  four. Two were the *same string written twice*, so a fix to either left the other. The unit of
  work is the sentence, so the sentences live together.

- **The boundary is a message *about the workspace*, not every string that leaves the process.**
  Anything reaching `window/showMessage` — a bad setting, a bundle that did not resolve, a
  truncated answer — belongs here. An LSP *error response* does not: `"ya-lsp does not handle
  {method} yet"` and `"request cancelled by the client"` answer one request, are addressed to the
  client rather than to a person, and most editors only log them.

- **The message is plain text, in both places it lands.** It goes out as `window/showMessage`
  and as a `tracing::warn!` line, and no client renders markdown in a notification: a backtick is
  a backtick on the screen. Name a command as `bundle install` and a setting as `gems.paths`.

- **A setting is its dotted TOML path.** `gems.max_files`, `index.include`, `diagnostics.rules` —
  never `[gems].max_files`, never `[gems] max_files`. The dotted form is valid TOML too, so it is
  something a user can paste. `messages::tests` checks every `<section>.<key>` a message names
  against the real schema, because a message naming a key that does not exist is worse than
  saying nothing: the user goes and adds it.

- **One colon.** It joins what happened to what follows from it — *or*, when another library's
  error is the explanation, it introduces that error, which is then the last thing in the message
  and is passed through exactly as that library wrote it. Not `;`, not ` — `; all three were in
  use and all three read the same.

- **A remedy whenever ya-lsp knows one**, as its own sentence, starting with a verb. "Run bundle
  install" is the difference between a warning and a nag. Where a warning is unactionable but
  correct, the remedy is the setting that silences it.

- **A full stop at the end**, unless the message ends in a foreign error.

- **The user's words, not ours.** "gem intelligence is incomplete" says less than "navigation
  into gems will mostly not work", and only one of them is a phrase a user could have written.
  No `rubydex`, no `declaration`, no `graph`, no `DocUri`. Say what stops working, not what the
  server stopped doing.

- **`messages::tests::every_message()` enumerates the whole set, and both directions are
  checked** — every `pub fn` in the module has an entry, and every entry names a real one. A new
  message with no entry fails the suite rather than shipping ungoverned. Coverage cannot do this
  job and this is where item 8's limit shows: `workspace/config.rs` is held at 100% of lines and
  branches, so all seven of its messages provably execute under test, and before this rule not
  one test read a word of any of them.

- **Silence is a message too, and the expensive one.** `ruby_lib_dirs` refusing to guess a Ruby
  is correct — guessing once put Apple's vestigial 2.6 stdlib into the graph and answered
  `"hello".u` with `unspace` — but refusing in silence cost every one of Ruby's own 727 library
  files, with `require "json"` answering `null` and nothing anywhere connecting that to a missing
  `.ruby-version`. When a guard declines to do something, ask what the user loses by it and
  whether anything says so.

- **A `tracing::debug!` beside a silent degradation must name what was lost, not what was
  looked for.** `gems.rs`'s "no Gemfile.lock … only Ruby's own library to index" was the only
  trace of that cliff, and it named the bundle — which is not what went missing, and *claimed*
  the library was indexed when the line above had just decided it would not be.

- **Prism writes the diagnostics and ya-lsp does not edit them.** `analysis::mod` forwards
  `diagnostic.message()` verbatim; ya-lsp owns the severity and the `code` and not one word of
  the text. That is deliberate — rewriting a parser's diagnostics is a real cost and a real risk
  of saying something false — but it is pinned rather than assumed:
  `parse_errors_read_the_way_prism_wrote_them` asserts the actual text, so an upstream rewrite is
  a visible test failure and not a silent change in what users read.
