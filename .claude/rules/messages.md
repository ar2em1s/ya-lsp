---
paths:
  - "src/messages.rs"
  - "src/logging.rs"
  - "src/workspace/config.rs"
  - "src/workspace/mod.rs"
  - "src/workspace/gems.rs"
  - "src/workspace/rbs.rs"
  - "src/analysis/mod.rs"
  - "src/analysis/requests.rs"
---

# What the server says

## Where a message lives

1. **Every sentence a user reads is one `pub fn` in `src/messages.rs`**, not written next to the
   code that raises it.
2. **In scope:** anything that reaches `window/showMessage`, such as a bad setting, an unresolved
   bundle or a truncated answer.
3. **Out of scope:** LSP error responses (`"ya-lsp does not handle {method} yet"`,
   `"request cancelled by the client"`). They go to the client, not to a person.
4. **One exception: rename refusals.** The user pressed a key asking for a rename, so they are owed
   the reason and what to do next. That covers the `rename_*` messages and the shared plan.
   - A file drag (`workspace/willRenameFiles`) is not that request. A move with no rule stays silent.
   - A cursor that is on nothing gets `null` and no message.
5. **Every new message goes into `messages::tests::every_message()`.** The test checks both
   directions, so a message with no entry fails the suite.

## How to write one

1. **Plain text only.** No client renders markdown in a notification. Write `bundle install`, not
   `` `bundle install` ``.
2. **Name a setting by its dotted TOML path**: `gems.max_files`, never `[gems].max_files`. The tests
   check every named key against the real schema.
3. **Use one colon**, joining what happened to what follows from it. Never `;` or ` — `. When a
   library's error explains the problem, put it last, word for word.
4. **Give the remedy whenever there is one**, as its own sentence starting with a verb ("Run bundle
   install."). If there is none, name the setting that silences the warning.
5. **Use the user's words.** Say what stops working ("navigation into gems will mostly not work").
   Never say `rubydex`, `declaration`, `graph` or `DocUri`. End with a full stop, unless the
   message ends in a foreign error.

## Silence is a message too

- **When a guard declines to do something, ask what the user loses and whether anything tells
  them.** `ruby_lib_dirs` refusing to guess a Ruby version is correct. But it drops all of Ruby's
  library, so it must say so.
- **A `tracing::debug!` next to a silent degradation names what was lost**, not what was looked
  for, and never claims the work was done.

## Parser diagnostics

- **Forward Prism's `diagnostic.message()` word for word.** ya-lsp owns the severity and the
  `code`, never the text. `parse_errors_read_the_way_prism_wrote_them` pins the text, so an
  upstream rewrite fails a test.
