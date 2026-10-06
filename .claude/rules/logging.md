---
paths:
  - "src/logging.rs"
  - "src/main.rs"
  - "src/analysis/requests.rs"
  - "src/analysis/mod.rs"
  - "src/workspace/config.rs"
---

# The log

**The log is an interface.** When a user asks why they have no completions, stderr is the only
answer. Which Ruby, how many gems, which `ya-lsp.toml` was read, whether a request arrived: these
lines are product, and `captured_logs` asserts them.

## What to write

- **Write** methods, counts, durations, positions, paths and identifiers.
- **Never write** a line of the user's source, a rendered hover card or a completion list. Renamed
  identifiers and search queries are fine.
- **Never write to stdout.** Every layer names its writer explicitly, because `fmt::layer()`
  defaults to stdout.

## Every request writes two lines

Both are written in `Analysis::serve`, the one door every method goes through, or in
`Analysis::supersede`, the door a held hint request leaves by without being computed:

1. **`request`:** method, id, document, position, and whether the graph was behind the buffer.
2. **`answered`:** method, id, outcome, `settled`, `retried`, `drained`, `elapsed`.
   - `retried` means the deferred request settled and asked again.
   - `drained` means a cold server indexed the queued bundle first.
   - Keep the outcomes `nothing` (`null`) and `empty` (an empty list) distinct. They are fixed in
     different modules.
   - A cancelled request uses the same shape, and so does `superseded` (a held hint request a newer
     one for the same range replaced).

**A held `inlayHint` writes one line more**, `held until the edit settles`, when the run loop sets it
aside (`Analysis::hold`). Its pair is written when it is answered, so without that line the wait
before the pair is invisible.

`every_method_says_it_arrived_and_says_what_it_answered` reads the method list out of `dispatch`
itself. Never copy the list into the test.

## Levels

1. **`info` is the normal working log.** `DEFAULT_LOG_FILTER` is `info`. The watcher's one `info`
   line includes how long it took to arm.
2. **Per-request and per-pass detail is `debug`.**
3. **`[log] level` and `[log] file_level` take a bare level or a full filter.** A bare word is scoped
   to `ya_lsp=` (`directive`), so `debug` doesn't turn on rubydex's logs.
4. **`YA_LSP_LOG` outranks `[log] level`.** This is the one exception to `config.rs` precedence. It
   is read once and held on `Reload`, so a config reload does not undo it.
5. **A filter that cannot be parsed is never silent.** Fall back to the default and name the key.

## Two sinks

- **Two layers, each with its own `EnvFilter`**, so the file can sit at `debug` while stderr stays
  at `info`. Never tee one layer into both.
- **Register both layers at startup and never replace them.** A layer swapped in later has no
  `FilterId` and panics. Reload only each layer's filter and the file target (`Switch`). The file
  layer starts with its filter `off`.
- **The reason for reloading:** `install` runs before `initialize`, and the root that
  `tmp/ya-lsp.log` is relative to only exists after it.
- **The `Reload` handle is passed along, not made global:** `main` → `server::run_stdio` → `serve`
  → `analysis::spawn`. `Reload::default()` is detached, not a no-op: tests still open the file.

## What the file sink must never do

1. **Truncate.** Use `O_APPEND`, and create the directory if needed.
2. **Split an event across writes.** Do one `write` per event, with the pid as a prefix, so two
   servers sharing one file don't interleave.
3. **Get indexed.** Exclude it by path, because the path can be changed.
4. **Be watched.** A server writing to a file it watches creates a loop.
5. **Dirty a corpus.** The default `tmp/ya-lsp.log` is gitignored by all six corpora. That is why
   the default is not `.ya-lsp.log`.

## Coverage

- **A `tracing::` line is covered only when a test reads it** (`Capture` answers
  `Interest::sometimes`). Never raise the level just to cover lines.
- **In a floored module, a new log line needs a `captured_logs` test.** Keep the macro on one line,
  with its arguments computed into locals first.
- **Generators don't log; `analysis/synthesize.rs` does.** `workspace/rails/` and `generated.rs`
  stay pure text in, text out.
- **`logging.rs` is floored at 100.** Installing the global subscriber lives in `main.rs`, which
  `tests/lifecycle.rs` reaches by running the binary.
