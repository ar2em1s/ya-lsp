---
paths:
  - "src/logging.rs"
  - "src/main.rs"
  - "src/analysis/requests.rs"
  - "src/analysis/mod.rs"
  - "src/workspace/config.rs"
---

# The log

## The log is an interface

`src/testing.rs` states it and this file is what holds the rest of the crate to it: **when a user
asks why they have no completions, stderr is the only thing that answers.** Which Ruby was
picked, how many gems resolved, which `ya-lsp.toml` was read, whether the request arrived at all.
Those lines are as much a product as a hover card, and `captured_logs` is what asserts them.

The defect that made this a rule rather than a preference: **21 of the 24 request methods used to
answer in complete silence.** No line anywhere said that a request had arrived, which method it
was, whether it settled first, how long it took or whether it answered anything — so *"hover does
nothing in this file"* and *"the client never sent a hover"* produced identical logs. The second
is what a document selector one folder too narrow really does, and it shipped.
`workspace/gems.rs`'s `ruby_lib` comment is the older example of the same shape: `require "json"`
answered `null`, `JSON.parse` hovered as nothing, and the only trace anywhere was a `DEBUG` line
about the bundle — which is not what went missing.

## What may be written, and what may never be

- **Methods, counts, durations, positions, paths and identifiers.** All of it.
- **Never a line of the user's source, a rendered hover card, or a completion list.** `rename`
  logging two identifiers and `search` logging the query are the standing precedent and they
  stay; a card is a different thing. Once there is a copy on disk the difference is the
  difference between a diagnostic and a transcript of somebody's private repository.
- Never stdout. `core-invariants.md` already forbids it and the file sink is a second way to
  break it: the sink writes to a `File` it opened by path, and `fmt::layer()`'s *default* writer
  is stdout, so every layer built here names its writer explicitly.

## The two lines every request writes

`Analysis::serve` is the one door all 24 methods go through, so the pair is written there rather
than 24 times. `request` carries method, id, document, position and whether the graph was already
behind the buffer; `answered` carries method, id, outcome, `settled`, `retried` and `elapsed`.

- **`nothing` and `empty` are different outcomes and must stay different.** `null` is a cursor
  the server could not make anything of; an empty `items` is a list that matched nothing. They
  are repaired in different modules, and one word for both sends every report to the wrong one.
- **A cancelled request is answered in the same shape**, not in a sentence of its own, so a
  reader following one id gets the same two lines whatever happened to it.
- `every_method_says_it_arrived_and_says_what_it_answered` reads the method list **out of
  `dispatch` itself**. A list written out in the test would be a second copy, and the method it
  went stale on is exactly the one whose silence nobody would notice.

## Level

- `info` is what a user reads when the server is merely working: which Ruby, how many files, how
  long. `DEFAULT_LOG_FILTER` is `info` and the VS Code manifest documents it in those words.
- **Per-request and per-pass detail is `debug`**, which the manifest already describes as *the
  work behind each request*. Nothing in the log item moved the default.
- `[log] level` and `[log] file_level` take a **bare level or a full filter**. A bare word is
  scoped to `ya_lsp=` rather than passed through, because `debug` on its own means every crate
  linked in — rubydex and lsp-server included — which is not what anyone picking `debug` out of a
  drop-down meant. The extension always sent `ya_lsp=${level}`; `directive` is where that moved
  to when the setting stopped being an environment variable.
- **`YA_LSP_LOG` outranks `[log] level`**, which is the one inversion of `config.rs`'s
  precedence. The variable is what somebody debugging from a terminal types, and a project file
  that silently overrode it would be the opposite of a debugging aid. It is read once, at the
  edge, and **held on the `Reload`** — a reload that forgot it would quietly undo it on the first
  `ya-lsp.toml` change.
- A directive nobody can read is **never silent**: it falls back to the default and says which
  key was wrong. A log at a level the user did not choose is not a symptom anyone traces back to
  the setting that decides it.

## Two sinks, and the one shape that works

Two layers with two `EnvFilter`s, not one `fmt` layer tee'd into both with `MakeWriterExt::and`.
The tee gives the two sinks **one** filter, and the whole point of the file is that it can sit at
`debug` while stderr stays at `info`.

**Both layers are registered at startup and neither is ever replaced.** This is not a
preference — a per-layer filter is handed a `FilterId` when the layer is registered with the
subscriber, so a `reload` handle that swaps a *layer* in afterwards installs one that was never
registered and the first event to reach it panics with *a `Filtered` layer was used, but it had
no `FilterId`*. It was written that way first and the panic is what found it. What reloads is the
part that may: each layer's **filter**, and the file the second one writes to, behind `Switch`.
The file layer exists from the first line with its filter at `off`.

**Why anything reloads at all:** `install` runs in `main`, before the handshake, and the
workspace root that `tmp/ya-lsp.log` is relative to does not exist until `initialize`. The
alternative is what `config.ts` documented for two releases — *the filter is read once, before
the server has a client to be configured by* — which is why `ya-lsp.logLevel` used to restart the
process and no longer does.

**The handle travels; nothing global is added.** `main` → `server::run_stdio` → `serve` (the
first apply, at `initialize`) → `analysis::spawn` (every later one, because a `ya-lsp.toml`
change arrives on that thread). `Reload::default()` is *detached*: it decides everything and
reaches no subscriber, which is what the in-process harnesses hold. It is deliberately not a
no-op — a test that turns the file on still opens the file and still gets told when it could not.

## What the file sink must not do

1. **Never truncate.** `O_APPEND`, and the directory is created because the default path is
   inside `tmp/` and a fresh clone has no `tmp/`. Truncate-on-start throws away the session the
   user is trying to report.
2. **One `write` per event, with the pid in front of it.** Two windows on one project are two
   processes with one file between them; `O_APPEND` makes a single `write` atomic against another
   process' single `write`, so the prefix goes into the *same* call rather than a second one that
   another server can interleave between. The pid is what tells the two apart afterwards.
3. **Never become an indexed document.** `.log` matches no `index.include` glob and
   `respect_gitignore` prunes `tmp/` — but the path is user-settable, so the exclusion is by
   **path** and stated here rather than inherited from an extension.
4. **Never a `didChangeWatchedFiles` source.** A server writing a file it watches is a loop.
5. **It must not dirty a pinned corpus.** All six clones gitignore `tmp`, solidus included, whose
   `.gitignore` spells it bare — so `make corpora-status` and `make canary` stay honest at the
   default path. That is a property of **the default**: `.ya-lsp.log` at the root would have
   broken both, which is why the default is `tmp/ya-lsp.log` and off.

## Coverage

- **A `tracing::` line is covered exactly when some test asked to read it.** `testing.rs`'s
  `Capture` answers `Interest::sometimes` for that reason, and raising the level to cover a line
  is the gaming `coverage.md` forbids.
- **In a floored module a new log line needs a `captured_logs` test**, and the macro stays one
  line with the arguments computed into locals first — the value is then evaluated whether or not
  the level is on, which is where it belongs anyway.
- **A generator does not log; the pass that ran it does.** The naive place for "how many tables,
  how many declarations" is inside `workspace/rails/schema.rs` and `generated.rs`. Both are at
  100:100 and `rails/mod.rs` states the property that makes the directory reviewable — *pure text
  in, text out, no I/O and no graph* — which a log line inside it breaks for a count the **caller**
  already holds. Every number worth logging about a generator is known in `analysis/synthesize.rs`.
- `logging.rs` is on `COVERAGE_FLOORS` at 100:100 for `config.rs`'s reason. The one line that
  cannot be tested in process — installing the global subscriber — is in `main.rs` instead, where
  `tests/lifecycle.rs` reaches it by spawning the real binary.
