"""One server, one settle, replies matched by id.

Each comment below records something a sweep got wrong without the line it sits above.
"""

import json
import os
import queue
import subprocess
import threading
import time

from audit.answers import Shifts, locations
from audit.config import CEILING, QUIET, pins, server_options


def uri(path):
    return "file://" + str(path)


class Client:
    def __init__(self, server, repo, argv=None, env=None):
        """One server on one workspace. `argv` and `env` default to ya-lsp's own.

        **No caller here passes either, and they still earn their place.** Everything below the
        launch is LSP, not ya-lsp: the framing, the id matching, `note`'s progress bookkeeping and
        its answer to `window/workDoneProgress/create`, and `settle`'s three conditions. So the
        launch is the one thing that must vary to drive another server. Keeping it a parameter
        prevents a second copy of the reader, which is the part that drifts.
        """
        launch = dict(os.environ)
        launch["YA_LSP_LOG"] = "ya_lsp=info"
        launch.update(env or {})
        self.name = os.path.basename(server)
        self.p = subprocess.Popen(argv or [server, "--stdio"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  cwd=repo, env=launch)
        self.inbox = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=lambda: [None for _ in self.p.stderr], daemon=True).start()
        self.next_id = 1
        self.pending = {}
        # What `settle` reads: the `$/progress` tokens begun and not yet ended. That is the server
        # saying it is still working, which beats any inference from silence.
        #
        # Not "has it begun one *yet*": the gap between the workspace index's diagnostics and gem
        # indexing's stream is tens of milliseconds. Waiting for a stream would refuse a
        # `gems.enabled = false` workspace to guard that gap.
        self.open = set()
        # Every `window/showMessage` at warning severity or worse. A server saying only part of its
        # bundle is installed invalidates every absolute number taken from it.
        self.warnings = []

    def _read(self):
        while True:
            header = self.p.stdout.readline()
            if not header:
                self.inbox.put(None)
                return
            if header.lower().startswith(b"content-length:"):
                n = int(header.split(b":")[1])
                self.p.stdout.readline()
                self.inbox.put(json.loads(self.p.stdout.read(n)))

    def send(self, m):
        b = json.dumps(m).encode()
        self.p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
        self.p.stdin.flush()

    def post(self, key, method, params):
        """Put one request in flight without waiting for it."""
        self.next_id += 1
        self.pending[self.next_id] = key
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})

    def note(self, m):
        """Everything true of a message, whoever is reading it.

        One place, not two, because two readers drift. Answering `window/workDoneProgress/create` is
        not optional: it is a *request*, and a server left waiting on one is not indexing.
        """
        if m is None:
            raise RuntimeError(f"{self.name}: server closed")
        if m.get("method") == "window/workDoneProgress/create":
            self.send({"jsonrpc": "2.0", "id": m["id"], "result": None})
            return
        if m.get("method") == "$/progress":
            params = m.get("params") or {}
            token = json.dumps(params.get("token"))
            kind = (params.get("value") or {}).get("kind")
            if kind == "begin":
                self.open.add(token)
            elif kind == "end":
                self.open.discard(token)
            return
        # **`logMessage` at error severity counts as a warning too.** A server may report a failed
        # subsystem on either channel, and some use only the log. Without this, a server running
        # with half its knowledge off looks healthy.
        #
        # Only the first line is kept: a backtrace is not a warning, and the reader needs the
        # sentence naming what died.
        if m.get("method") in ("window/showMessage", "window/logMessage"):
            params = m.get("params") or {}
            severe = (1, 2) if m["method"] == "window/showMessage" else (1,)
            if params.get("type") in severe:
                said = str(params.get("message") or "").strip().split("\n")[0][:300]
                if said and said not in self.warnings:
                    self.warnings.append(said)

    def pump(self):
        """Answer whatever is waiting without blocking. True if anything was there."""
        spoke = False
        while True:
            try:
                m = self.inbox.get_nowait()
            except queue.Empty:
                return spoke
            spoke = True
            self.note(m)

    def drain(self, down_to=0):
        """Read replies until at most `down_to` are still in flight. Yields (key, result)."""
        while len(self.pending) > down_to:
            m = self.inbox.get(timeout=600)
            self.note(m)
            # A reply never carries a `method`, and the server numbers its own requests in its own
            # id space. Without this, a server request whose id matched one of ours would be popped
            # off `pending` and yielded as that hover's answer.
            if "method" in m:
                continue
            key = self.pending.pop(m.get("id"), None)
            if key is None:
                continue
            # **An error reply is not a quiet one.** `result` is absent on an error, so
            # `m.get("result")` is `None`, the same as a server with nothing to say. A corpus of
            # malformed posts would read as a clean population of *no answer here*.
            #
            # It goes beside the warnings, because that list is what a reader checks before
            # believing an absolute, and `score` prints it per corpus.
            if "error" in m:
                said = str((m.get("error") or {}).get("message") or "").strip()[:200]
                note = f"error reply to {key[1] if isinstance(key, tuple) else key}: {said}"
                if note not in self.warnings:
                    self.warnings.append(note)
            yield key, m.get("result")

    def ask(self, method, params):
        """One request, answered. The key must not be `None`: `drain` skips a reply whose key is, so
        `post(None, ...)` swallows every answer and returns `None` for all of them.
        """
        self.post("__ask__", method, params)
        for _, result in self.drain():
            return result

    def stop(self):
        try:
            self.send({"jsonrpc": "2.0", "id": 9999, "method": "shutdown", "params": None})
            self.send({"jsonrpc": "2.0", "method": "exit"})
        except (BrokenPipeError, OSError):
            pass
        try:
            self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.kill()


def unsettled(client, last):
    """Which of `settle`'s three conditions the ceiling cut short.

    A sentence, because the caller turns it into an exit code, and the causes differ: "still
    indexing" means raise the ceiling, "never spoke" means the server is broken.
    """
    if last is None:
        return "never spoke"
    if client.open:
        return "still indexing"
    return "still talking"


def settle(client, quiet=QUIET, ceiling=CEILING):
    """Wait until the server has finished its cold start. Returns `(seconds, why)`.

    Three conditions:

    1. **The server has said something.** A cold server is silent while it walks the workspace, and
       on a large one that walk is the longest thing it does. A quiet period counted from
       `initialized` reads the silence *before* the work as the silence *after* it, and the sweep
       then asks a server with no bundle in it.
    2. **No `$/progress` stream is open.** Gem indexing says when it begins and ends, and a
       statement beats an inference from silence. A server that opens no stream falls through to the
       other two conditions.
    3. **Then quiet:** `scripts/canary.py`'s rule and its 3 s, for the diagnostics that follow the
       resolve gem indexing triggers on its way out.
    """
    started = time.time()
    last = None
    while time.time() - started < ceiling:
        if client.pump():
            last = time.time()
        if last is not None and not client.open and time.time() - last >= quiet:
            return last - started, "quiet"
        time.sleep(0.02)
    return ceiling, unsettled(client, last)


def start(server, corpus, argv=None, env=None, options=None):
    """A settled server on one corpus. Returns `(client, seconds, why)`.

    - `argv`, `env` and `options` default to ya-lsp's own. `Client.__init__` says why the launch is
      a parameter.
    - `options` is separate because `server.toml` is *this* server's configuration. Another server
      would get settings in a vocabulary it lacks.
    - Pass `{}` for the empty `initializationOptions` an editor sends before a user sets anything.
    """
    client = Client(server, str(corpus.dir), argv=argv, env=env)
    # Stamped on the client, not returned, so the signature stays what `commands` calls. Nothing
    # outside can recover this number: `settle`'s seconds start when settle begins, and its quiet
    # period is wall clock on top, so only here can the round trip an editor blocks on be told apart
    # from the cold start after it.
    began = time.time()
    ready = client.ask("initialize", {
        "processId": os.getpid(),
        "rootUri": uri(corpus.dir),
        "workspaceFolders": [{"uri": uri(corpus.dir), "name": corpus.name}],
        # `server.toml`, sent the way an editor sends settings. Not a `ya-lsp.toml` in the corpus:
        # an extra file makes the clone dirty, and `check_clean` refuses to measure a dirty clone.
        "initializationOptions": server_options() if options is None else options,
        "capabilities": {
            "window": {"workDoneProgress": True},
            # **Code points, negotiated, not assumed.**
            #
            # - LSP's default character is a UTF-16 code unit. One accented string earlier on a line
            #   would shift every cursor after it, and the answers would be about a different word.
            # - The sampler counts **code points**: `shapes.find` runs its patterns over a Python
            #   `str`, and `ruby.at_offset` counts elements of one. So every recorded `column`, and
            #   every `offset` in an `audit.site`, is a code-point count.
            # - UTF-32's code unit is the code point, so asking for it makes the wire agree with the
            #   whole package by construction. A `column` also stays valid for slicing a decoded
            #   line, which `lane2.footnotes` does.
            # - **Any other encoding fails silently.** A cursor lands inside the previous word, the
            #   server answers that question correctly, and lane 2 reads a right answer to the wrong
            #   question as a defect.
            "general": {"positionEncodings": ["utf-32"]},
            "textDocument": {
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                # **On, and it changes no answer.** `goto_response` in `requests.rs` builds the
                # `Location` array from the same links' `targetSelectionRange`, so the targets are
                # identical either way. The link shape adds `originSelectionRange`, the span
                # `definition` decided the cursor was on. Check 3 compares `hover`'s range against
                # it, so it cannot run without this.
                "definition": {"linkSupport": True},
                "documentHighlight": {},
                "publishDiagnostics": {},
            },
        },
    })
    agreed = ((ready or {}).get("capabilities") or {}).get("positionEncoding")
    if agreed != "utf-32":
        # Fail, never convert. Converting means re-deriving every column in the server's encoding
        # here, a second copy of `position.rs` in Python. Asking for the encoding this package
        # already counts in leaves nothing to convert.
        raise pins.Fail(f"{corpus.name}: server chose {agreed!r} positions, not utf-32")
    client.initialize_s = time.time() - began
    client.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    seconds, why = settle(client)
    return client, seconds, why


def open_document(client, corpus, path):
    """`didOpen` one file. The `languageId` is what makes a template a template."""
    full = corpus.dir / path
    text = full.read_text(encoding="utf-8", errors="replace")
    client.send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
        "textDocument": {"uri": uri(full), "languageId": "erb" if path.endswith(".erb")
                         else "ruby", "version": 1, "text": text}}})
    return text


class Post:
    """How one method is asked: what it needs beyond a position, where it is legal, and how much of
    the draw it takes.

    `hover`, `definition` and `documentHighlight` need none of this, so they stay bare strings: they
    take `{textDocument, position}`, are legal at any cursor, and are asked at every drawn position.
    `references` needs all three fields, and each is a way it can quietly answer nothing:

    - **`params`.** `ReferenceParams.context` is **required**. Without it the server's
      `parse_params` logs *malformed request params* and returns `None`, which reaches this client
      as `result: null`, the same as a cursor with no references. Every position would read as
      *nothing here*: a clean population of zeroes that no lane-2 counter can see. The check reading
      this reply also refuses to be believed when its answered count is zero.
    - **`shapes`.** The cursor shapes a request may be *concluded* from. `references` has no scope
      walk (`highlight.rs` has), so at a local or a block parameter, highlight lights a variable and
      `references` has no target. A check that forgets this fires on every local.
    - **`stride`.** Every *n*th eligible position, counted over eligible positions, not the draw, so
      the subset is the same every run and spread evenly through the qualifying shapes. 1 asks all
      of them. It exists because `references` is the widest answer the server gives and the budget
      is wall clock; `audit cost` measures whether it is needed.
    """

    __slots__ = ("method", "params", "shapes", "stride")

    def __init__(self, method, params=None, shapes=None, stride=1):
        self.method = method
        self.params = params or {}
        self.shapes = frozenset(shapes) if shapes else None
        self.stride = max(1, int(stride))

    def legal(self, shape):
        return self.shapes is None or shape in self.shapes

    def payload(self, target, line, column):
        return dict(self.params, textDocument={"uri": target},
                    position={"line": line, "character": column})


def ask_all(client, corpus, drawn, methods=("textDocument/hover", "textDocument/definition"),
            in_flight=32, opened=None):
    """Ask `methods` at every drawn position. Returns {(index, method): result}.

    A method is a bare string or a [`Post`]. A string is `Post(method)` with every field defaulted.

    **A method the stride or shape skipped has no key in the result.** Every downstream check must
    keep *not asked here* apart from *asked and answered nothing*, the way `lane2.context.Row` keeps
    `referenced` apart from `references`.

    **Pipelined and matched by id, with a cap on requests in flight.** Posting a whole corpus at
    once gains nothing (the server answers in order anyway) and holds every reply in memory before
    the first is read.

    **`opened` is the set of paths already `didOpen`ed on this client.** A second caller on the same
    server **must** pass the first one's, because a second `didOpen` for an open document is
    illegal, and lane 1's Rails key reaches the same model files the sample does. Pass nothing if
    the caller owns the server alone.
    """
    answers = {}
    opened = set() if opened is None else opened
    posts = tuple(m if isinstance(m, Post) else Post(m) for m in methods)
    # **Every sampled document is opened before the first request.** Opening each as the loop
    # reached it made an answer depend on where its cursor sat in the draw: a reference in an
    # unopened document is not in the graph if the project's `.gitignore` names that file (lobsters
    # gitignores some tracked templates). The same cursor then answered differently early and late
    # in the pass, and check 8 read the difference as missed call sites. One document set for the
    # whole pass is what makes two replies comparable.
    for _, _, path, _, _, _, _ in drawn:
        if path not in opened:
            open_document(client, corpus, path)
            opened.add(path)
    # Eligible positions seen per method, which is what `stride` counts, not the draw index.
    # Striding the index would take every nth *position*, and a method's shapes are not evenly
    # spread through a draw stratified by directory.
    seen = {post.method: 0 for post in posts}
    for index, (_, shape, path, line, column, _, _) in enumerate(drawn):
        for post in posts:
            if not post.legal(shape):
                continue
            take = seen[post.method] % post.stride == 0
            seen[post.method] += 1
            if not take:
                continue
            client.post((index, post.method), post.method,
                        post.payload(uri(corpus.dir / path), line, column))
        for key, result in client.drain(down_to=in_flight):
            answers[key] = result
    for key, result in client.drain():
        answers[key] = result
    return answers


def ask_rebased(client, corpus, drawn, eager, in_flight=32):
    """Re-ask every position over a graph that is really behind the buffer.

    **The edit is one newline at the very start of a document.** Every sampled construct stays
    exactly as it was, and every sampled offset moves by exactly one line per edit. `didChange`
    records the edit and indexes nothing, so the answer must come from the last settled graph,
    through `position::Rebase`. The crate's contract is that *a deferred answer is never less than
    an eager one*; this measures that sentence.

    **Each edit goes right before the request that needs it, in the same stream. That is the whole
    protocol.**
    - Editing every document once up front measures almost nothing. The first empty deferred answer
      makes the server settle and re-ask, and every later answer comes from a re-indexed graph with
      nothing to translate.
    - A longer `YA_LSP_RESOLVE_DEBOUNCE_MS` does not help: the settle that spoils it is the retry,
      not the timer.
    - An edit just before its request in the queue is applied just before it, whatever settled in
      between, so the retry cannot get ahead of it. It costs nothing extra.

    **Which documents:** the cursor's own, plus every *sampled* document the eager answer pointed
    into. Those are the maps this check compares: a place can only be reported lost from a document
    the eager answer named. Editing every document before every position would multiply the run's
    cost for nothing.

    Returns `(answers, shifts)`. `shifts` is an [`answers.Shifts`]: the edits each document had
    taken *at each position*, which `answers.targets` subtracts to compare the two passes.
    """
    sampled = {uri(corpus.dir / path) for _, _, path, _, _, _, _ in drawn}
    shifts, answers, versions = Shifts(), {}, {}

    def insert_a_line(target, index):
        # Versions must rise and `didOpen` used 1, so the edit count is the version.
        versions[target] = versions.get(target, 0) + 1
        shifts.record(target, index)
        client.send({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
            "textDocument": {"uri": target, "version": versions[target] + 1},
            "contentChanges": [{
                "range": {"start": {"line": 0, "character": 0},
                          "end": {"line": 0, "character": 0}},
                "rangeLength": 0, "text": "\n"}]}})

    for index, (_, _, path, line, column, _, _) in enumerate(drawn):
        here = uri(corpus.dir / path)
        pointed = {target for target, _ in
                   locations(eager.get((index, "textDocument/definition")))}
        for target in sorted({here} | (pointed & sampled)):
            insert_a_line(target, index)
        for method in ("textDocument/hover", "textDocument/definition"):
            client.post((index, method), method, {
                "textDocument": {"uri": here},
                "position": {"line": line + shifts.at(here, index), "character": column},
            })
        for key, result in client.drain(down_to=in_flight):
            answers[key] = result
    for key, result in client.drain():
        answers[key] = result
    return answers, shifts
