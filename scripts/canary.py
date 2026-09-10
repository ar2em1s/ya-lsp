#!/usr/bin/env python3
"""Open a real Rails application the way an editor does, and check the shape of the answers.

Every performance number in this project's three plans came from a driver typed into a scratch
directory and run by hand against a Rails app that is not public. None of them is reproducible by
anyone who is not holding that repository, and none has ever run unattended. This is the first
driver that is a file rather than a throwaway, and the workspace it opens — `lobsters`, the
software running lobste.rs, pinned to one commit — is public and permissively licensed.

**This is a canary, not a benchmark.** It answers one question: does opening a real application
still work at all? The index ceiling is generous by an order of magnitude on purpose, because a
shared CI runner with a cold page cache is not the machine the reference number was measured on,
and a canary that flakes is worse than no canary. What it catches is the accidental quadratic, the
discovery rule that stops matching, the parser regression that turns working Ruby into red — each
of which moves these numbers by a factor, not by a percent.

**It does not cover gems.** Resolving lobsters' bundle needs `bundle install`, which needs Ruby
4.0.0 and a hand-built `sqlite3` — a large amount of CI for a project whose headline is that it
needs no Ruby at all. The run leaves the gem settings at their defaults, so on a machine with an
installed bundle the gem half runs and on CI it finds nothing; nothing asserted here depends on
which. The gem numbers stay manual (`.claude/rules/benchmarking.md`). Do not read a green canary
as covering them.

**Legal.** lobsters is BSD-3-Clause, (c) 2012-2019 Joshua Stein. CI clones it at test time and
ya-lsp never vendors it, so no artifact this project ships contains any of it and no notice is
owed — the rule is per artifact, not per repository. That is a property of how it is used and not
of the licence: the day somebody copies a file out of it into `tests/`, or caches a tarball in
this repository, the obligation attaches and `THIRD-PARTY-NOTICES.txt` is where it goes.

Usage (the numbers live in the `Makefile`, so a local run and the CI run cannot disagree):

    python3 scripts/canary.py --repo tmp/lobsters --server target/release/ya-lsp \
        --files 606 --parse-warnings 14 --max-index-ms 500
"""

import argparse
import collections
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time

# The own-code index line `analysis::Analysis::index_workspace` writes at INFO. It is the only
# place the file count and the cold index time are reported, and there is no LSP request that
# asks for either — `workspace/symbol` answers with symbols and a cap, not with files. So this
# driver reads the log, and pins its shape: a reword that breaks the pattern fails the canary
# with `LOG SHAPE` rather than silently measuring nothing.
INDEXED = re.compile(r"indexed (\d+) files in ([0-9.]+)(ns|us|µs|ms|s)\b")

# Rust's `{:.2?}` on a `Duration` picks the unit; all five are possible and only two are likely.
UNIT_MS = {"ns": 1e-6, "us": 1e-3, "µs": 1e-3, "ms": 1.0, "s": 1000.0}


class Server:
    """The binary, spoken to over stdio exactly as an editor speaks to it."""

    def __init__(self, binary, repo):
        env = dict(os.environ)
        # The one line this driver needs is INFO, and raising the level further would bury it in
        # a per-file debug stream on a 606-file workspace.
        env["YA_LSP_LOG"] = "ya_lsp=info"
        self.proc = subprocess.Popen(
            [binary, "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            env=env,
        )
        self.inbox = queue.Queue()
        self.log = []
        # Both pipes are drained on their own threads. A blocking read on either one deadlocks
        # the moment the server has nothing more to say on it, and stderr fills its pipe buffer
        # and stops the server dead if nobody is reading it.
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self):
        while True:
            header = self.proc.stdout.readline()
            if not header:
                self.inbox.put(None)
                return
            if header.lower().startswith(b"content-length:"):
                length = int(header.split(b":")[1])
                self.proc.stdout.readline()  # the blank line ending the headers
                self.inbox.put(json.loads(self.proc.stdout.read(length)))

    def _read_stderr(self):
        for line in self.proc.stderr:
            self.log.append(line.decode("utf-8", "replace").rstrip())

    def send(self, message):
        body = json.dumps(message).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.proc.stdin.flush()

    def shut_down(self):
        self.send({"jsonrpc": "2.0", "id": 9999, "method": "shutdown", "params": None})
        self.send({"jsonrpc": "2.0", "method": "exit"})
        try:
            return self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            return None


def open_workspace(server, repo, quiet_for, deadline):
    """Initialize, then read until the workspace settles. Returns the diagnostics as they stand.

    Diagnostics are *state*, not events: the server republishes a URI whenever its set changes
    and sends an empty list to clear one. Counting notifications would double every file the
    server touched twice, so the last publish for each URI wins.
    """
    uri = "file://" + repo
    server.send(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": os.getpid(),
                "rootUri": uri,
                "workspaceFolders": [{"uri": uri, "name": os.path.basename(repo)}],
                # Gem indexing is background work reported over `$/progress`, and the server only
                # opens a stream for a client that says it can receive one. Advertising it is
                # what makes the end of that work observable rather than guessed at.
                "capabilities": {
                    "window": {"workDoneProgress": True},
                    "textDocument": {"publishDiagnostics": {}},
                },
            },
        }
    )

    published = {}
    messages = []
    initialized = False
    last = time.monotonic()
    while True:
        if time.monotonic() > deadline:
            raise TimeoutError("the server never settled")
        try:
            message = server.inbox.get(timeout=0.25)
        except queue.Empty:
            if initialized and time.monotonic() - last > quiet_for:
                return published, messages
            continue
        if message is None:
            raise RuntimeError("the server closed its output before the workspace settled")
        last = time.monotonic()
        method = message.get("method")
        if message.get("id") == 1 and "result" in message:
            initialized = True
            server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        elif method == "window/workDoneProgress/create":
            # A request, not a notification: leaving it unanswered leaves the server waiting.
            server.send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method == "textDocument/publishDiagnostics":
            params = message["params"]
            published[params["uri"]] = params["diagnostics"]
        elif method == "window/showMessage":
            messages.append(message["params"]["message"])


def index_line(log):
    """The file count and the cold own-code index time, in milliseconds."""
    for line in log:
        found = INDEXED.search(line)
        if found:
            return int(found.group(1)), float(found.group(2)) * UNIT_MS[found.group(3)]
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="the workspace to open")
    parser.add_argument("--server", required=True, help="the ya-lsp binary to drive")
    parser.add_argument("--files", type=int, required=True, help="files the index must hold")
    parser.add_argument(
        "--parse-warnings", type=int, required=True, help="`parse-warning`s the app produces"
    )
    parser.add_argument(
        "--max-index-ms", type=float, required=True, help="ceiling on the cold own-code index"
    )
    parser.add_argument(
        "--quiet-for",
        type=float,
        default=3.0,
        help="seconds of silence that count as settled (default: 3)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=180.0,
        help="seconds before the run is called hung (default: 180)",
    )
    args = parser.parse_args()

    repo = os.path.abspath(args.repo)
    binary = os.path.abspath(args.server)
    if not os.path.isdir(repo):
        sys.exit(f"canary: no workspace at {repo}")
    if not os.access(binary, os.X_OK):
        sys.exit(f"canary: no runnable server at {binary}; run: make release")

    print(f"canary: {binary}")
    print(f"        {repo}\n")

    server = Server(binary, repo)
    started = time.monotonic()
    try:
        published, messages = open_workspace(
            server, repo, args.quiet_for, started + args.timeout
        )
    except (TimeoutError, RuntimeError) as failure:
        server.shut_down()
        for line in server.log[-20:]:
            print("  log:", line)
        sys.exit(f"canary: {failure}")
    settled_ms = (time.monotonic() - started) * 1000 - args.quiet_for * 1000
    exit_code = server.shut_down()

    counts = collections.Counter()
    files = collections.defaultdict(set)
    for uri, items in published.items():
        for item in items:
            counts[item.get("code")] += 1
            files[item.get("code")].add(uri)

    indexed = index_line(server.log)
    failures = []

    if indexed is None:
        failures.append(
            "LOG SHAPE: no `indexed N files in T` line on stderr. Either the workspace was "
            "never indexed, or `analysis::Analysis::index_workspace` reworded the line this "
            "canary reads its two numbers from — see scripts/canary.py's INDEXED."
        )
        count, index_ms = 0, 0.0
    else:
        count, index_ms = indexed

    print(f"  indexed          {count:6d} files      (expected {args.files})")
    print(f"  cold index       {index_ms:9.2f} ms     (ceiling {args.max_index_ms:.0f})")
    print(f"  settled after    {settled_ms:9.2f} ms")
    print(f"  files with problems {len(published):3d}")
    for code, number in sorted(counts.items(), key=lambda pair: -pair[1]):
        print(f"    {number:4d}  {code}  in {len(files[code])} files")
    for message in messages:
        print(f"  message: {message[:160]}")
    print()

    if indexed is not None and count != args.files:
        failures.append(
            f"indexed {count} files, expected {args.files}. Discovery changed: an exclude "
            f"rule, the gitignore walk, or which extensions count."
        )
    if indexed is not None and index_ms > args.max_index_ms:
        failures.append(
            f"the cold own-code index took {index_ms:.2f} ms, over the {args.max_index_ms:.0f} ms "
            f"ceiling. The ceiling is an order of magnitude above the reference measurement, so "
            f"this is a regression in kind rather than a slow runner."
        )
    if counts.get("parse-error", 0) != 0:
        failures.append(
            f"{counts['parse-error']} parse-error diagnostics, expected 0. Every one of these "
            f"{args.files} files parsed under the pinned ruby-prism; a file that no longer does "
            f"is a parser regression, not a defect in the application."
        )
    if counts.get("parse-warning", 0) != args.parse_warnings:
        failures.append(
            f"{counts.get('parse-warning', 0)} parse-warning diagnostics, expected "
            f"{args.parse_warnings}."
        )
    templates = sorted(uri for uri in published if uri.endswith((".erb", ".rhtml")))
    if templates:
        failures.append(
            f"{len(templates)} ERB templates published diagnostics, expected none: "
            + ", ".join(name.rsplit("/", 1)[-1] for name in templates[:5])
            + ". A template's parse errors are about text nobody wrote — a `yield` that is "
            "legal in the method the template compiles to, an `end` closing a block a helper "
            "opened — so `Analysis::collect_diagnostics` drops them. One arriving here means "
            "either that rule stopped firing or that the scanner in `analysis/erb.rs` "
            "regressed; `make coverage-missing F=erb` and the corpus in its module docs are "
            "where to look."
        )
    unexpected = sorted(
        code for code in counts if code not in ("parse-error", "parse-warning")
    )
    if unexpected:
        failures.append(
            "diagnostics this application has never produced: "
            + ", ".join(f"{code} ({counts[code]})" for code in unexpected)
            + ". A rule that starts firing on a real Rails app is a default worth re-deciding, "
            "not a number worth editing."
        )
    if exit_code not in (0, None):
        failures.append(f"the server exited {exit_code} after shutdown")
    elif exit_code is None:
        failures.append("the server did not exit after shutdown; it was killed")

    for failure in failures:
        print(f"canary: {failure}", file=sys.stderr)
    if failures:
        sys.exit(1)
    print("canary: ok")


if __name__ == "__main__":
    main()
