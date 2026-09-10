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

- **Every sentence a user reads lives in `src/messages.rs`, one `pub fn` per message** — not beside
  the code that raises it. Spread over five files, seventeen sites answered the same four questions
  independently (how to spell a setting, where to break the clause, whether to name a remedy,
  whether to end with a period) and no two agreed on all four; two were the *same string written
  twice*, so a fix to either left the other. The unit of work is the sentence.

- **The boundary is a message *about the workspace*, not every string that leaves the process.**
  Anything reaching `window/showMessage` — a bad setting, an unresolved bundle, a truncated answer —
  belongs here. An LSP *error response* does not: `"ya-lsp does not handle {method} yet"` and
  `"request cancelled by the client"` answer one request, are addressed to the client rather than a
  person, and most editors only log them.

- **One exception, deliberate: a rename refusal is about a single request.** The rule above would
  put the seven `rename_*` sentences outside this module; they are in it anyway. An error response
  is the wrong carrier (most editors only log those) and a bare `null` makes the editor say only
  that nothing can be renamed. The user pressed a key asking for that rename, so they are owed which
  of the four reasons applies and what to do next. What earns the exception is the deliberate
  keystroke — not that per-request messages are fine generally. Anything answered `null` because the
  cursor is on nothing says nothing.

- **Plain text, in both places it lands.** It goes out as `window/showMessage` and as a
  `tracing::warn!` line, and no client renders markdown in a notification: a backtick is a backtick
  on screen. Name a command as `bundle install` and a setting as `gems.paths`.

- **A setting is its dotted TOML path** — `gems.max_files`, `index.include`, `diagnostics.rules`.
  Never `[gems].max_files` or `[gems] max_files`. The dotted form is valid TOML, so it is pasteable.
  `messages::tests` checks every `<section>.<key>` a message names against the real schema: naming a
  key that does not exist is worse than saying nothing, because the user goes and adds it.

- **One colon.** It joins what happened to what follows from it — or, when another library's error is
  the explanation, introduces that error, which is then the last thing in the message and passed
  through exactly as that library wrote it. Not `;`, not ` — `; all three were in use and all three
  read the same.

- **A remedy whenever ya-lsp knows one**, as its own sentence, starting with a verb. "Run bundle
  install" is the difference between a warning and a nag. Where a warning is unactionable but
  correct, the remedy is the setting that silences it.

- **A full stop at the end**, unless the message ends in a foreign error.

- **The user's words, not ours.** "gem intelligence is incomplete" says less than "navigation into
  gems will mostly not work", and only one is a phrase a user could have written. No `rubydex`, no
  `declaration`, no `graph`, no `DocUri`. Say what stops working, not what the server stopped doing.

- **`messages::tests::every_message()` enumerates the whole set, both directions checked** — every
  `pub fn` has an entry and every entry names a real one. A new message with no entry fails the
  suite rather than shipping ungoverned. Coverage cannot do this job: `workspace/config.rs` is held
  at 100% of lines and branches, so all seven of its messages provably execute under test, and not
  one of those tests reads a word of any of them.

- **Silence is a message too, and the expensive one.** `ruby_lib_dirs` refusing to guess a Ruby is
  correct — guessing once put Apple's vestigial 2.6 stdlib into the graph and answered `"hello".u`
  with `unspace` — but refusing in silence cost **every one of Ruby's library files**, with
  `require "json"` answering `null` and nothing connecting that to a missing `.ruby-version`. When a
  guard declines to do something, ask what the user loses and whether anything says so.

- **A `tracing::debug!` beside a silent degradation must name what was lost, not what was looked
  for.** `gems.rs`'s "no Gemfile.lock … only Ruby's own library to index" was the only trace of that
  cliff, and it named the bundle — which is not what went missing — and *claimed* the library was
  indexed when the line above had just decided it would not be.

- **Prism writes the diagnostics and ya-lsp does not edit them.** `analysis::mod` forwards
  `diagnostic.message()` verbatim; ya-lsp owns the severity and the `code` and not one word of the
  text. Rewriting a parser's diagnostics is a real cost and a real risk of saying something false.
  Pinned rather than assumed: `parse_errors_read_the_way_prism_wrote_them` asserts the actual text,
  so an upstream rewrite is a visible test failure.
