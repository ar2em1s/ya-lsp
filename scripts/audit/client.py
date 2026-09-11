"""One server, one settle, replies matched by id.

The settle rule here is the one earlier sweeps arrived at the hard way; every comment below
records something a run got wrong before the line above it existed.
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

        **No caller in this package passes either, and they are still not dead weight.**
        Everything below the launch is a property of LSP rather than of ya-lsp — the framing, the
        id matching, `note`'s progress bookkeeping and its answer to
        `window/workDoneProgress/create`, `settle`'s three conditions — so the one thing that has
        to vary for this code to drive a differently spelled or differently configured server is
        the launch itself. Keeping that a parameter is what stops the alternative: a second copy of
        the reader, which is the part that drifts.
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
        # What `settle` reads: the `$/progress` tokens begun and not yet ended, which is the
        # server's own statement that it is still working and beats any inference from silence.
        # Deliberately not "has it begun one *yet*" — the gap between the diagnostics the
        # workspace index publishes and the stream gem indexing opens measures 60 ms, so waiting
        # for a stream would refuse a `gems.enabled = false` workspace to guard a hazard sixty
        # milliseconds wide.
        self.open = set()
        # Every `window/showMessage` at warning severity or worse. It used to be discarded: a
        # server that says out loud that only 104 of 328 gems are installed invalidates every
        # absolute number taken from it, and that sentence used to scroll past unread.
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
        """Everything that is true of a message whoever is reading it.

        One place rather than two, because two readers drift: answering
        `window/workDoneProgress/create` is not optional — it is a *request*, and a server left
        waiting on one is a server that is not indexing.
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
        # **`logMessage` at error severity counts as a warning, not only `showMessage`.** A server
        # is free to report a failed subsystem either way, and some report it only on the log
        # channel — where it used to be discarded, so a server running with half its knowledge
        # switched off was indistinguishable from a healthy one. The first line is kept rather than
        # the whole message because a backtrace is not a warning; what a reader needs is the
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
            # A reply never carries a `method`, and the server numbers its own requests in its
            # own id space: without this line a server request whose id collided with one of
            # ours would be popped off `pending` and yielded as that hover's answer.
            if "method" in m:
                continue
            key = self.pending.pop(m.get("id"), None)
            if key is not None:
                yield key, m.get("result")

    def ask(self, method, params):
        """One request, answered. The key must not be `None`: `drain` skips a reply whose key
        is, so `post(None, ...)` swallows every answer and returns `None` for all of them."""
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

    A sentence rather than a shrug, because the caller turns it into an exit code: "still
    indexing" means raise the ceiling and "never spoke" means something is wrong with the
    server, and those are not the same problem.
    """
    if last is None:
        return "never spoke"
    if client.open:
        return "still indexing"
    return "still talking"


def settle(client, quiet=QUIET, ceiling=CEILING):
    """Wait until the server has finished its cold start. Returns `(seconds, why)`.

    Three conditions, and only the last of them is obvious.

    1. **The server has said something.** A cold server is silent while it walks the workspace,
       and on a large one that walk is the longest thing it does — so a quiet period that starts
       counting at `initialized` reads the silence *before* the work as the silence *after* it.
       That is what once made four shards print `settled after 0.0s` and begin asking questions
       of a server with no bundle in it: 1,484 positions read as regressions.
    2. **No `$/progress` stream is open.** Gem indexing says when it begins and ends, and a
       statement beats an inference from silence. A server that opens no stream at all falls
       through to the other two conditions rather than waiting for what will not come.
    3. **Then quiet.** `scripts/canary.py`'s rule and its 3 s, for the diagnostics that follow
       the resolve gem indexing arms on its way out.
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

    `argv`, `env` and `options` default to ya-lsp's own; see `Client.__init__` for why the launch
    is a parameter at all. `options` is separate from the other two because `server.toml` is *this
    server's* configuration — a caller starting another one would otherwise be sending it settings
    in a vocabulary it does not have, while the first server got a configured run and the second a
    default one. Pass `{}` for the empty `initializationOptions` an editor sends before a user has
    set anything.
    """
    client = Client(server, str(corpus.dir), argv=argv, env=env)
    # Stamped rather than returned, so the signature stays what `commands` calls. It is the one
    # number a caller cannot recover afterwards: `settle`'s seconds run from when settle began,
    # and its quiet period is wall clock on top of both, so nothing outside here can separate the
    # round trip an editor blocks on from the cold start that follows it.
    began = time.time()
    ready = client.ask("initialize", {
        "processId": os.getpid(),
        "rootUri": uri(corpus.dir),
        "workspaceFolders": [{"uri": uri(corpus.dir), "name": corpus.name}],
        # `server.toml`, the way an editor sends its settings. Not a `ya-lsp.toml` written into
        # the corpus: a clone with an extra file in it is a dirty clone, and `check_clean`
        # refuses to measure one.
        "initializationOptions": server_options() if options is None else options,
        "capabilities": {
            "window": {"workDoneProgress": True},
            # **Code points, negotiated rather than assumed, and it was `utf-8` here until
            # 2026-09-14 on a belief about this harness that was not true.** LSP's default
            # character is a UTF-16 code unit, so one accented string earlier on the same line
            # shifts every cursor after it and the answers come back about a different word —
            # that much was right, and it is the reason this field is filled in at all. What was
            # wrong is which encoding fixes it. The sampler does not scan bytes: `shapes.find`
            # runs its patterns over a Python `str` and `ruby.at_offset` counts elements of one,
            # so every `column` it records and every `offset` in an `audit.site` is a count of
            # **code points**. UTF-32's code unit is the code point, so asking for it makes the
            # wire agree with the whole package by construction, and leaves a `column` something
            # a check may still slice a decoded line with — which `lane2.footnotes` does.
            #
            # **The failure this replaces was live and had been found by a check rather than by
            # reasoning.** Under `utf-8` a cursor after a multi-byte character was posed that
            # many bytes to the *left*, landing inside the previous word, where the server
            # answers a different question perfectly correctly — invisible in the way that
            # matters, because lane 2 then reads a right answer to the wrong question as a
            # defect. Measured over the whole draw: 2 of 5,523 positions, both on mastodon, and
            # one of them was raised as a `receiver-contradicted` finding that is not one.
            "general": {"positionEncodings": ["utf-32"]},
            "textDocument": {
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                # **On, and it changes no answer.** `mod.rs` builds the `Location` array by
                # taking `targetSelectionRange` off the same links, so the targets are identical
                # either way — what the link shape adds is `originSelectionRange`, the span
                # `definition` decided the cursor was on. Without it there is nothing to compare
                # `hover`'s own range against and check 3 cannot be written at all.
                "definition": {"linkSupport": True},
                "documentHighlight": {},
                "publishDiagnostics": {},
            },
        },
    })
    agreed = ((ready or {}).get("capabilities") or {}).get("positionEncoding")
    if agreed != "utf-32":
        # Fail rather than convert. Converting would mean re-deriving every column in the
        # server's encoding here, which is a second copy of `position.rs` living in a Python
        # script — and the whole point of asking for the one encoding this package already
        # counts in is that there is then nothing to convert.
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


def ask_all(client, corpus, drawn, methods=("textDocument/hover", "textDocument/definition"),
            in_flight=32, opened=None):
    """Ask `methods` at every drawn position. Returns {(index, method): result}.

    Pipelined and matched by id, with a cap on how many are in flight: a whole corpus posted at
    once is a queue the server answers in order anyway, and it costs the memory of every reply
    arriving before the first is read.

    `opened` is the set of paths already `didOpen`ed on this client, and a second caller on the
    same server **must** pass the first one's: a `didOpen` for a document the client has already
    opened is not a legal message, and lane 1's Rails key reaches the same model files the sample
    does. Passing nothing keeps the old behaviour for a caller that owns the server alone.
    """
    answers = {}
    opened = set() if opened is None else opened
    for index, (_, _, path, line, column, _, _) in enumerate(drawn):
        if path not in opened:
            open_document(client, corpus, path)
            opened.add(path)
        for method in methods:
            client.post((index, method), method, {
                "textDocument": {"uri": uri(corpus.dir / path)},
                "position": {"line": line, "character": column},
            })
        for key, result in client.drain(down_to=in_flight):
            answers[key] = result
    for key, result in client.drain():
        answers[key] = result
    return answers


def ask_rebased(client, corpus, drawn, eager, in_flight=32):
    """Re-ask every position over a graph that is really behind the buffer.

    The edit is **one newline at the very start of a document**, so every sampled construct is
    left exactly as it was and every sampled offset moves by exactly one line per edit it has
    taken. `didChange` records it and indexes nothing, so the answer now has to come from the
    last settled graph with `position::Rebase` translating onto it. The crate's stated contract
    is that *a deferred answer is never less than an eager one*, and this is that sentence as a
    measurement.

    **The edits go immediately before the requests that need them, in the same stream, and that
    is the whole of the protocol.** Editing every document once up front — which is what this
    did until 2026-09-15 — measures almost nothing: the first deferred answer that comes back
    empty makes the server settle and re-ask, the graph catches up, and every position after it
    is answered by a re-indexed graph with nothing left to translate. Measured over the six
    corpora, that protocol reached `Rebase` on **139 of 11,046 requests**, and on discourse on
    **1 of 1,792** — so its zeros were a property of the harness and said nothing about the
    server. Pushing `YA_LSP_RESOLVE_DEBOUNCE_MS` out does not help and was measured too: the
    contaminating settle is the retry, not the timer. What does work costs nothing extra: an
    edit that sits just before its request in the queue is applied just before it, whatever
    settled in between, so the retry cannot get ahead of it and the debounce is re-armed by
    every edit anyway.

    **Which documents.** The cursor's own, and every *sampled* document the eager answer pointed
    into — those are the maps this check compares, since a place can only be reported lost from
    a document the eager answer named. A document nobody can point at would cost an edit for
    nothing, and `full`-style editing of all of them before every position costs 8.6x the run.

    Returns `(answers, shifts)`, where `shifts` is an [`answers.Shifts`] — the count each
    document had taken *at each position*, which is what `answers.targets` has to subtract to
    compare the two passes at all.
    """
    sampled = {uri(corpus.dir / path) for _, _, path, _, _, _, _ in drawn}
    shifts, answers, versions = Shifts(), {}, {}

    def insert_a_line(target, index):
        # Versions have to rise and `didOpen` used 1, so the count of edits is the version.
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
