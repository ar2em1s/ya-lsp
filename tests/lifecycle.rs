//! End to end: spawn the real binary and speak LSP to it over stdio.
//!
//! The in-process tests in `analysis` cover graph behaviour. This covers what they cannot: that the
//! shipped executable frames messages correctly, negotiates capabilities and exits cleanly, which
//! is exactly where a language server tends to fail silently.

use std::{
    io::{BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::Duration,
};

use lsp_server::{Message, Notification, Request, RequestId, Response};

/// How long any single message may take to arrive. Generous (it only has to beat the 150 ms
/// analysis debounce) but finite, so a server that stops talking fails the test instead of hanging
/// the suite.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a test may wait for ya-lsp's own watcher to notice a change on disk.
///
/// A timeout, never a sleep: the loop it bounds stops as soon as the answer changes, which through
/// the shipped binary is a request or two. It is a minute because it must cover *arming*
/// (`server::watcher::watch` records what that costs and where it is pathological), not the
/// hundred-millisecond debounce behind it.
const WATCH_TIMEOUT: Duration = Duration::from_secs(60);

struct Server {
    child: Child,
    stdin: ChildStdin,
    /// Filled by a reader thread so every wait can be bounded.
    incoming: Receiver<Message>,
    next_id: i32,
}

impl Server {
    fn start(root: &std::path::Path) -> Self {
        Self::start_with_env(root, &[])
    }

    /// `start`, with extra environment variables: the only way to point the real binary at a
    /// synthetic gem home, since gem discovery reads the process environment.
    fn start_with_env(root: &std::path::Path, env: &[(&str, &std::path::Path)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ya-lsp"));
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .arg("--stdio")
            .current_dir(root)
            .env("YA_LSP_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ya-lsp");

        let stdin = child.stdin.take().expect("stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let (sender, incoming) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(message)) = Message::read(&mut stdout) {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin,
            incoming,
            next_id: 0,
        }
    }

    fn read(&mut self) -> Message {
        match self.incoming.recv_timeout(REPLY_TIMEOUT) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => panic!("server went quiet for {REPLY_TIMEOUT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("server closed the stream early"),
        }
    }

    fn send(&mut self, message: Message) {
        message.write(&mut self.stdin).expect("write");
        self.stdin.flush().expect("flush");
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> RequestId {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.send(Message::Request(Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        }));
        id
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.send(Message::Notification(Notification {
            method: method.to_owned(),
            params,
        }));
    }

    /// Read messages until the response to `id` arrives, skipping notifications.
    fn response(&mut self, id: &RequestId) -> Response {
        loop {
            match self.read() {
                Message::Response(response) if &response.id == id => return response,
                Message::Response(other) => panic!("unexpected response {:?}", other.id),
                Message::Notification(_) | Message::Request(_) => {}
            }
        }
    }

    /// Read messages until a request the *server* made with `method` arrives.
    fn server_request(&mut self, method: &str) -> Request {
        loop {
            if let Message::Request(request) = self.read()
                && request.method == method
            {
                return request;
            }
        }
    }

    /// Read messages until a notification of `method` arrives.
    fn notification(&mut self, method: &str) -> serde_json::Value {
        loop {
            if let Message::Notification(notification) = self.read()
                && notification.method == method
            {
                return notification.params;
            }
        }
    }
}

fn initialize_params(root: &std::path::Path) -> serde_json::Value {
    let uri = url::Url::from_file_path(root).unwrap().to_string();
    serde_json::json!({
        "processId": std::process::id(),
        "rootUri": uri,
        "capabilities": {
            "general": { "positionEncodings": ["utf-16", "utf-8"] },
            "textDocument": { "synchronization": { "dynamicRegistration": false } }
        },
        "workspaceFolders": [{ "uri": uri, "name": "fixture" }]
    })
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::write(
        dir.path().join("lib/person.rb"),
        "# Someone with a name.\nclass Person\n  def shout\n  end\nend\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("lib/main.rb"),
        "require \"person\"\n\nPerson.new\n",
    )
    .unwrap();
    // Ruby's own signatures are off for every fixture not about them. They come from whatever rbs
    // the machine has, or the vendored copy, and neither belongs in a test of something else:
    // hundreds of files of background work is noise the assertions would have to wait out.
    // `built_in_classes_*` turns them back on and tests them.
    std::fs::write(dir.path().join("ya-lsp.toml"), "[rbs]\nenabled = false\n").unwrap();
    dir
}

fn uri_of(root: &std::path::Path, relative: &str) -> String {
    url::Url::from_file_path(root.join(relative))
        .unwrap()
        .to_string()
}

/// Initialize params for a client that advertises everything we can take advantage of.
fn modern_client(root: &std::path::Path) -> serde_json::Value {
    let mut params = initialize_params(root);
    params["capabilities"]["textDocument"]["documentSymbol"] =
        serde_json::json!({ "hierarchicalDocumentSymbolSupport": true });
    params["capabilities"]["textDocument"]["definition"] =
        serde_json::json!({ "linkSupport": true });
    params["capabilities"]["window"] = serde_json::json!({ "workDoneProgress": true });
    params["capabilities"]["workspace"] =
        serde_json::json!({ "workspaceEdit": { "documentChanges": true } });
    params
}

/// The same client, plus the two capabilities a generated document needs to be readable.
///
/// A separate builder, not a field on `modern_client`, because the pair decides whether a code
/// action is offered at all: a fixture with them on by accident could not show that a client
/// without them is offered nothing.
fn reading_client(root: &std::path::Path) -> serde_json::Value {
    let mut params = modern_client(root);
    params["capabilities"]["window"]["showDocument"] = serde_json::json!({ "support": true });
    params["capabilities"]["workspace"]["textDocumentContent"] =
        serde_json::json!({ "dynamicRegistration": true });
    params
}

/// A project with one bundled gem installed in its own directory, laid out as RubyGems lays one
/// out. Returns the project and the gem home, which must stay alive.
fn bundled_fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = fixture();
    std::fs::write(
        project.path().join("Gemfile.lock"),
        "GEM\n  remote: https://rubygems.org/\n  specs:\n    shouty (1.2.3)\n",
    )
    .unwrap();

    let gem_home = tempfile::tempdir().expect("tempdir");
    let gem = gem_home.path().join("gems/shouty-1.2.3/lib");
    std::fs::create_dir_all(&gem).unwrap();
    std::fs::write(
        gem.join("shouty.rb"),
        "module Shouty\n  class Megaphone\n  end\nend\n",
    )
    .unwrap();
    std::fs::create_dir_all(gem_home.path().join("specifications")).unwrap();
    std::fs::write(
        gem_home.path().join("specifications/shouty-1.2.3.gemspec"),
        "Gem::Specification.new do |s|\n  s.require_paths = [\"lib\".freeze]\nend\n",
    )
    .unwrap();

    (project, gem_home)
}

/// Bring a server up to the point where it will answer requests.
fn started(root: &std::path::Path, params: serde_json::Value) -> Server {
    let mut server = Server::start(root);
    let id = server.request("initialize", params);
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));
    server
}

fn shut_down(mut server: Server) {
    let id = server.request("shutdown", serde_json::Value::Null);
    server.response(&id).response_result.expect("shutdown");
    server.notify("exit", serde_json::Value::Null);
    assert!(wait_for_exit(&mut server.child).success());
}

#[test]
fn full_lifecycle_over_stdio() {
    let root = fixture();
    let mut server = Server::start(root.path());

    let id = server.request("initialize", initialize_params(root.path()));
    let result = server
        .response(&id)
        .response_result
        .expect("initialize should succeed");

    // The client offered utf-8, so the server must take it: rubydex speaks byte offsets, and utf-8
    // makes every column conversion the identity.
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(result["capabilities"]["textDocumentSync"]["change"], 2); // INCREMENTAL
    assert_eq!(
        result["capabilities"]["textDocumentSync"]["openClose"],
        true
    );
    assert_eq!(result["capabilities"]["referencesProvider"], true);
    assert_eq!(result["capabilities"]["documentHighlightProvider"], true);
    assert_eq!(result["capabilities"]["selectionRangeProvider"], true);
    assert_eq!(result["capabilities"]["foldingRangeProvider"], true);
    // Announced with its one option, answering a question the client would otherwise ask once per
    // link: there is nothing to resolve.
    assert_eq!(
        result["capabilities"]["documentLinkProvider"]["resolveProvider"],
        false
    );
    // Both hierarchies, against the real binary: they are announced by two different mechanisms,
    // and a client needs each to offer its own command.
    assert_eq!(result["capabilities"]["callHierarchyProvider"], true);
    assert_eq!(
        result["capabilities"]["inlayHintProvider"]["resolveProvider"],
        true
    );
    assert_eq!(result["capabilities"]["workspaceSymbolProvider"], true);
    // The one capability `lsp-types` has no field for, added on the way to JSON. Asserted here as
    // well as in `capabilities::tests` because the flatten carrying it also carries every provider
    // above, and a flatten that stopped flattening would show as this line passing and the rest
    // failing.
    assert_eq!(result["capabilities"]["typeHierarchyProvider"], true);
    // `prepareProvider` is the half that matters: it lets the server decline a position before the
    // editor asks the user to type a new name.
    assert_eq!(
        result["capabilities"]["renameProvider"]["prepareProvider"],
        true
    );
    assert_eq!(
        result["capabilities"]["completionProvider"]["resolveProvider"],
        true
    );
    assert_eq!(
        result["capabilities"]["signatureHelpProvider"]["triggerCharacters"],
        serde_json::json!(["(", ","])
    );
    // The kinds are what matter here: a client filters on them before asking, so a
    // `codeActionProvider` listing none is never asked for a refactoring. The empty string is the
    // third kind, on purpose: the protocol's name for a kindless action, which is what the
    // read-only jump into a generated document is.
    assert_eq!(
        result["capabilities"]["codeActionProvider"]["codeActionKinds"],
        serde_json::json!(["refactor.extract", "refactor.rewrite", ""])
    );
    // The two halves of `workspace/textDocumentContent`: the scheme the server serves documents
    // under, and the command that opens one. Both are announced in the handshake and neither has an
    // `lsp-types` field (hence `capabilities::Advertised`), so this assertion is all that stands
    // between a typo and an unreachable feature.
    assert_eq!(
        result["capabilities"]["workspace"]["textDocumentContent"]["schemes"],
        serde_json::json!(["ya-lsp-generated"])
    );
    assert_eq!(
        result["capabilities"]["workspace"]["workspaceFolders"]["supported"],
        true
    );
    // The filter is everything a client asks about: without it, the server is asked before every
    // file operation in the project, and with a misspelled glob it is asked about none. Files only,
    // because a folder rename arrives as the folder, never as the Ruby inside it.
    assert_eq!(
        result["capabilities"]["workspace"]["fileOperations"]["willRename"]["filters"],
        serde_json::json!([{
            "scheme": "file",
            "pattern": { "glob": "**/*.rb", "matches": "file" }
        }])
    );
    // One command, whose name carries the workspace root: `vscode-languageclient` calls
    // `registerCommand` for every name here, which throws on a name already taken, and the
    // extension starts one client per workspace folder.
    let commands = result["capabilities"]["executeCommandProvider"]["commands"]
        .as_array()
        .expect("one command");
    assert_eq!(commands.len(), 1, "{commands:?}");
    let command = commands[0].as_str().expect("a name");
    assert!(command.starts_with("ya-lsp.showGenerated."), "{command}");
    assert_ne!(command, "ya-lsp.showGenerated");
    assert_eq!(result["serverInfo"]["name"], "ya-lsp");

    server.notify("initialized", serde_json::json!({}));

    let uri = url::Url::from_file_path(root.path().join("lib/person.rb"))
        .unwrap()
        .to_string();
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": uri,
                "languageId": "ruby",
                "version": 1,
                "text": "class Person\n  def whisper\n  end\nend\n"
            }
        }),
    );
    // An incremental change: replace `whisper` with `murmur` in place.
    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 1, "character": 6 },
                    "end": { "line": 1, "character": 13 }
                },
                "text": "murmur"
            }]
        }),
    );

    // The server must have applied that edit to its own copy of the buffer, and from out here the
    // only way to see that copy is to ask for the outline.
    let id = server.request(
        "textDocument/documentSymbol",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );
    let symbols = server
        .response(&id)
        .response_result
        .expect("documentSymbol");
    // This client never advertised `hierarchicalDocumentSymbolSupport`, so the answer must be the
    // flat pre-3.10 shape: `SymbolInformation`, with a `location` and a `containerName` instead of
    // nested `children`.
    assert!(
        symbols[0]["location"].is_object(),
        "a client without hierarchical support must get the flat shape: {symbols}"
    );
    assert_eq!(symbols[1]["name"], "murmur", "{symbols}");
    assert_eq!(symbols[1]["containerName"], "Person", "{symbols}");

    // A code action over the buffer the server holds, asked of the real process: the extractions
    // and accessors are pure functions over one string and are tested as such, but this is the only
    // place the request reaches the shipped binary, and it is the one request whose answer edits
    // the user's file.
    let id = server.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 1, "character": 2 }
            },
            "context": { "diagnostics": [] }
        }),
    );
    let actions = server.response(&id).response_result.expect("codeAction");
    // `def murmur` is a method with an empty body inside `class Person`: nothing to extract and no
    // instance variable to give an accessor, so the honest answer is `null`.
    assert_eq!(actions, serde_json::Value::Null, "{actions}");

    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": uri, "version": 3 },
            "contentChanges": [{
                "text": "class Person\n  def murmur\n    @volume = 1\n  end\nend\n"
            }]
        }),
    );
    let id = server.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 2, "character": 4 },
                "end": { "line": 2, "character": 4 }
            },
            "context": { "diagnostics": [] }
        }),
    );
    let actions = server.response(&id).response_result.expect("codeAction");
    assert_eq!(
        actions[0]["title"], "Declare attr_reader :volume",
        "{actions}"
    );
    assert_eq!(actions[0]["kind"], "refactor.rewrite", "{actions}");
    assert_eq!(
        actions[0]["edit"]["changes"][&uri][0]["newText"], "attr_reader :volume\n  ",
        "{actions}"
    );

    // An unimplemented method must still be answered, not dropped: an unanswered request wedges the
    // client forever.
    let id = server.request(
        "textDocument/formatting",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let error = server
        .response(&id)
        .response_result
        .expect_err("formatting is not implemented yet");
    assert_eq!(error.code, lsp_server::ErrorCode::MethodNotFound as i32);

    server.notify(
        "textDocument/didClose",
        serde_json::json!({
            "textDocument": { "uri": uri }
        }),
    );

    let id = server.request("shutdown", serde_json::Value::Null);
    server
        .response(&id)
        .response_result
        .expect("shutdown should succeed");
    server.notify("exit", serde_json::Value::Null);

    let status = wait_for_exit(&mut server.child);
    assert!(status.success(), "server exited with {status}");
}

#[test]
fn defaults_to_utf16_when_the_client_advertises_nothing() {
    let root = fixture();
    let mut server = Server::start(root.path());

    let uri = url::Url::from_file_path(root.path()).unwrap().to_string();
    let id = server.request(
        "initialize",
        serde_json::json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": {}
        }),
    );
    let result = server.response(&id).response_result.expect("initialize");

    // A client that omits `general.positionEncodings` predates the capability and must be served
    // UTF-16, as the spec mandates.
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-16");

    server.notify("initialized", serde_json::json!({}));
    let id = server.request("shutdown", serde_json::Value::Null);
    server.response(&id).response_result.expect("shutdown");
    server.notify("exit", serde_json::Value::Null);
    assert!(wait_for_exit(&mut server.child).success());
}

#[test]
fn a_broken_config_warns_instead_of_taking_the_server_down() {
    let root = fixture();
    std::fs::write(root.path().join("ya-lsp.toml"), "[index]\nexcludes = []\n").unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", initialize_params(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // The typo must surface as a window/showMessage rather than a dead server.
    let warning = server.notification("window/showMessage");
    assert_eq!(warning["type"], 2); // MessageType::WARNING
    assert!(
        warning["message"].as_str().unwrap().contains("excludes"),
        "{}",
        warning["message"]
    );

    let id = server.request("shutdown", serde_json::Value::Null);
    server.response(&id).response_result.expect("shutdown");
    server.notify("exit", serde_json::Value::Null);
    assert!(wait_for_exit(&mut server.child).success());
}

#[test]
fn the_log_file_holds_the_requests_the_output_channel_would_have_shown() {
    // Logging to a file, over a real process, in one assertion: a `[log] file` turns on a second
    // sink at its own level, incoming requests are written to it, and the file is where the setting
    // says, not where the binary happens to be.
    //
    // This is also the only test that runs `logging::install`, the one line installing a global
    // subscriber, which cannot run in-process without taking `testing.rs`'s capture away from every
    // other test in the binary.
    let root = fixture();
    std::fs::write(
        root.path().join("ya-lsp.toml"),
        "[rbs]\nenabled = false\n\n[log]\nlevel = \"warn\"\nfile = true\nfile_level = \"debug\"\n",
    )
    .unwrap();

    let mut server = started(root.path(), initialize_params(root.path()));
    let uri = uri_of(root.path(), "lib/person.rb");
    let id = server.request(
        "textDocument/documentSymbol",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );
    server.response(&id).response_result.expect("an outline");
    shut_down(server);

    let written = std::fs::read_to_string(root.path().join("tmp/ya-lsp.log"))
        .expect("[log] file = true writes tmp/ya-lsp.log under the workspace root");

    // The pair, at a level stderr was explicitly told not to carry, which is why the two sinks have
    // two filters instead of one writer tee'd into both.
    assert!(
        written.contains("request method=\"textDocument/documentSymbol\""),
        "{written}"
    );
    assert!(
        written.contains("answered method=\"textDocument/documentSymbol\""),
        "{written}"
    );
    // Every line says which process wrote it, because two windows on one project are two servers
    // sharing one file.
    let pid = format!("[{}] ", server_pid(&written));
    assert!(
        written.lines().all(|line| line.starts_with(&pid)),
        "{written}"
    );
    // And nothing in it is the user's source.
    assert!(!written.contains("class Person"), "{written}");
}

/// The pid every line of a log file is prefixed with, read back out of the first line.
fn server_pid(written: &str) -> u32 {
    written
        .lines()
        .next()
        .and_then(|line| line.strip_prefix('['))
        .and_then(|rest| rest.split(']').next())
        .and_then(|pid| pid.parse().ok())
        .expect("every line starts with the pid that wrote it")
}

#[test]
fn a_syntax_error_reaches_the_client_as_a_diagnostic() {
    // The end-to-end diagnostics claim: a file that does not parse lights up in the editor, with a
    // range the editor can place and a code the user can look up.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/broken.rb"),
        "class Broken\n  def bar\n",
    )
    .unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", initialize_params(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    let broken = url::Url::from_file_path(root.path().join("lib/broken.rb"))
        .unwrap()
        .to_string();
    let params = loop {
        let params = server.notification("textDocument/publishDiagnostics");
        // The clean file in the fixture never publishes, but do not rely on that.
        if params["uri"] == serde_json::Value::String(broken.clone()) {
            break params;
        }
    };

    let items = params["diagnostics"].as_array().expect("diagnostics array");
    let error = items
        .iter()
        .find(|item| item["code"] == "parse-error")
        .unwrap_or_else(|| panic!("{items:?}"));
    assert_eq!(error["severity"], 1); // DiagnosticSeverity::ERROR
    assert_eq!(error["source"], "ya-lsp");
    // Prism's own words, forwarded verbatim: ya-lsp owns the severity and the code and not one word
    // of the text. `parse_errors_read_the_way_prism_wrote_them` pins the whole set; here the wire
    // is being checked, so one sentence is enough, but a sentence, not a length. "Non-empty" as the
    // whole contract would test the mechanism and not the content, like an unranked completion
    // list.
    assert_eq!(
        error["message"],
        "expected an `end` to close the `class` statement"
    );
    assert!(error["range"]["start"]["line"].is_number());

    // Fixing the file must clear it, and clearing means an explicit empty array: silence would
    // leave the squiggle on screen forever.
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": broken,
                "languageId": "ruby",
                "version": 1,
                "text": "class Broken\n  def bar\n  end\nend\n"
            }
        }),
    );

    let cleared = loop {
        let params = server.notification("textDocument/publishDiagnostics");
        if params["uri"] == serde_json::Value::String(broken.clone()) {
            break params;
        }
    };
    assert_eq!(
        cleared["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "{cleared}"
    );
    assert_eq!(
        cleared["version"], 1,
        "the client needs the version it sent"
    );

    let id = server.request("shutdown", serde_json::Value::Null);
    server.response(&id).response_result.expect("shutdown");
    server.notify("exit", serde_json::Value::Null);
    assert!(wait_for_exit(&mut server.child).success());
}

#[test]
fn navigation_over_stdio() {
    // The end-to-end navigation claim: an editor that opens a file can ask what a name is, where it
    // came from and what the file contains, and gets answers in the shapes it advertised.
    let root = fixture();
    let mut server = started(root.path(), modern_client(root.path()));

    let main = uri_of(root.path(), "lib/main.rb");
    let person = uri_of(root.path(), "lib/person.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": main,
                "languageId": "ruby",
                "version": 1,
                "text": "require \"person\"\n\nPerson.new\n"
            }
        }),
    );

    // `Person` on the last line.
    let id = server.request(
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": main },
            "position": { "line": 2, "character": 2 }
        }),
    );
    let targets = server.response(&id).response_result.expect("definition");
    assert_eq!(
        targets[0]["targetUri"],
        serde_json::json!(person),
        "{targets}"
    );
    // `linkSupport` was advertised, so the answer carries the origin span and points at the class
    // name, not the whole body.
    assert_eq!(targets[0]["originSelectionRange"]["start"]["character"], 0);
    assert_eq!(targets[0]["originSelectionRange"]["end"]["character"], 6);
    assert_eq!(targets[0]["targetSelectionRange"]["start"]["line"], 1);
    assert_eq!(targets[0]["targetSelectionRange"]["start"]["character"], 6);

    // The path inside `require "person"`, which the graph never indexes.
    let id = server.request(
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": main },
            "position": { "line": 0, "character": 11 }
        }),
    );
    let required = server.response(&id).response_result.expect("definition");
    assert_eq!(
        required[0]["targetUri"],
        serde_json::json!(person),
        "{required}"
    );
    assert_eq!(required[0]["targetRange"]["start"]["line"], 0, "{required}");

    // Hover on the same constant.
    let id = server.request(
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": main },
            "position": { "line": 2, "character": 2 }
        }),
    );
    let hover = server.response(&id).response_result.expect("hover");
    let markdown = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(markdown.contains("class Person"), "{markdown}");
    assert!(
        markdown.contains("Someone with a name."),
        "the comment above the class is its documentation: {markdown}"
    );

    // The outline of the file that was never opened, in the nested shape this client asked for.
    let id = server.request(
        "textDocument/documentSymbol",
        serde_json::json!({ "textDocument": { "uri": person } }),
    );
    let symbols = server
        .response(&id)
        .response_result
        .expect("documentSymbol");
    assert_eq!(symbols[0]["name"], "Person", "{symbols}");
    assert_eq!(symbols[0]["kind"], 5, "SymbolKind::CLASS: {symbols}");
    assert_eq!(symbols[0]["children"][0]["name"], "shout", "{symbols}");

    // A position with nothing under it must answer `null`, not an error: the editor shows errors to
    // the user.
    let id = server.request(
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": main },
            "position": { "line": 1, "character": 0 }
        }),
    );
    assert_eq!(
        server.response(&id).response_result.expect("hover"),
        serde_json::Value::Null
    );

    shut_down(server);
}

#[test]
fn project_wide_search_over_stdio() {
    // The end-to-end search claim: an editor can ask the project a question that names no file.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/team.rb"),
        "require \"person\"\n\nclass Team\n  def lead\n    Person.new.shout\n  end\nend\n",
    )
    .unwrap();
    let mut server = started(root.path(), modern_client(root.path()));

    let person = uri_of(root.path(), "lib/person.rb");
    let team = uri_of(root.path(), "lib/team.rb");
    let main = uri_of(root.path(), "lib/main.rb");

    // Every use of `Person`, asked from the class definition, including the definition itself as VS
    // Code asks.
    let id = server.request(
        "textDocument/references",
        serde_json::json!({
            "textDocument": { "uri": person },
            "position": { "line": 1, "character": 8 },
            "context": { "includeDeclaration": true }
        }),
    );
    let found = server.response(&id).response_result.expect("references");
    let mut files: Vec<&str> = found
        .as_array()
        .expect("an array of locations")
        .iter()
        .map(|location| location["uri"].as_str().unwrap_or_default())
        .collect();
    files.sort_unstable();
    files.dedup();
    assert_eq!(
        files,
        vec![main.as_str(), person.as_str(), team.as_str()],
        "{found}"
    );

    // A method, which is name-based: `shout` is called once and defined once.
    let id = server.request(
        "textDocument/references",
        serde_json::json!({
            "textDocument": { "uri": team },
            "position": { "line": 4, "character": 16 },
            "context": { "includeDeclaration": false }
        }),
    );
    let found = server.response(&id).response_result.expect("references");
    assert_eq!(found[0]["uri"], serde_json::json!(team), "{found}");
    assert_eq!(found[0]["range"]["start"]["line"], 4, "{found}");

    // `workspace/symbol` names no document at all.
    let id = server.request("workspace/symbol", serde_json::json!({ "query": "shout" }));
    let symbols = server.response(&id).response_result.expect("symbol");
    assert_eq!(symbols[0]["name"], "shout", "{symbols}");
    assert_eq!(symbols[0]["containerName"], "Person", "{symbols}");
    assert_eq!(symbols[0]["kind"], 6, "SymbolKind::METHOD: {symbols}");
    assert_eq!(
        symbols[0]["location"]["uri"],
        serde_json::json!(person),
        "{symbols}"
    );
    // The name span, not the whole `def ... end`: a client reveals this range selected.
    assert_eq!(
        symbols[0]["location"]["range"]["start"]["line"], 2,
        "{symbols}"
    );
    assert_eq!(
        symbols[0]["location"]["range"]["start"]["character"], 6,
        "{symbols}"
    );

    // Nothing matching is `null`, not `[]`.
    let id = server.request(
        "workspace/symbol",
        serde_json::json!({ "query": "no_such_symbol_anywhere" }),
    );
    assert_eq!(
        server.response(&id).response_result.expect("symbol"),
        serde_json::Value::Null
    );

    shut_down(server);
}

#[test]
fn completion_over_stdio() {
    // The end-to-end completion claim: an editor asking what can be typed at a position gets an
    // answer from the project, not from words in the open buffer.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/office.rb"),
        "module HR\n  MAX_STAFF = 50\n\n  class Person\n    def self.build(name:)\n    end\n\n    \
         def shout\n    end\n  end\nend\n",
    )
    .unwrap();
    let mut server = started(root.path(), modern_client(root.path()));

    // A buffer the editor holds and the disk has never seen: the situation completion always runs
    // in.
    let scratch = uri_of(root.path(), "lib/scratch.rb");
    let typed = "HR::Person.bui\n";
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": scratch,
                "languageId": "ruby",
                "version": 1,
                "text": typed
            }
        }),
    );

    // `HR::Person.` is a singleton receiver: `build` is offered and `shout` is not.
    let id = server.request(
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "position": { "line": 0, "character": 14 }
        }),
    );
    let found = server.response(&id).response_result.expect("completion");
    // One row, so the cap dropped nothing, and the client is told it may narrow this list itself
    // instead of asking again for every character. Tested over the wire, because that flag is the
    // one thing here an editor acts on without asking anything else.
    assert_eq!(found["isIncomplete"], false, "{found}");
    let labels: Vec<&str> = found["items"]
        .as_array()
        .expect("an item list")
        .iter()
        .map(|item| item["label"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(labels, vec!["build"], "{found}");

    let item = &found["items"][0];
    assert_eq!(item["kind"], 2, "CompletionItemKind::METHOD: {found}");
    assert_eq!(item["detail"], "HR::Person.build", "{found}");
    // The half-typed `bui` is replaced, not appended to.
    assert_eq!(
        item["textEdit"]["range"],
        serde_json::json!({
            "start": { "line": 0, "character": 11 },
            "end": { "line": 0, "character": 14 }
        }),
        "{found}"
    );

    // The documentation arrives only when asked for.
    assert!(item["documentation"].is_null(), "{found}");
    let id = server.request("completionItem/resolve", item.clone());
    let resolved = server.response(&id).response_result.expect("resolve");
    assert!(
        resolved["documentation"]["value"]
            .as_str()
            .unwrap_or_default()
            .contains("build(name:)"),
        "{resolved}"
    );

    // And the argument list knows what the method takes.
    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": scratch, "version": 2 },
            "contentChanges": [{ "text": "HR::Person.build(\n" }]
        }),
    );
    let id = server.request(
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "position": { "line": 0, "character": 17 }
        }),
    );
    let found = server.response(&id).response_result.expect("completion");
    assert_eq!(found["items"][0]["label"], "name:", "{found}");

    // A comment is not a place Ruby can be written, and saying so lets the editor fall back to its
    // own word list instead of showing an empty popup.
    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": scratch, "version": 3 },
            "contentChanges": [{ "text": "# HR::Person\n" }]
        }),
    );
    let id = server.request(
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "position": { "line": 0, "character": 12 }
        }),
    );
    assert_eq!(
        server.response(&id).response_result.expect("completion"),
        serde_json::Value::Null
    );

    shut_down(server);
}

#[test]
fn signature_help_over_stdio() {
    // The end-to-end signature-help claim: an editor asking what a half-written call takes gets the
    // method's real parameters, with the one being typed marked, over the wire, from a buffer the
    // disk has never seen, with offsets in the encoding the client negotiated, not bytes.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/office.rb"),
        "module HR\n  class Person\n    # Build one.\n    def self.build(name, title = nil, \
         *tags, remote: false)\n    end\n  end\nend\n",
    )
    .unwrap();
    let mut server = started(root.path(), modern_client(root.path()));

    let scratch = uri_of(root.path(), "lib/scratch.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": scratch,
                "languageId": "ruby",
                "version": 1,
                "text": "HR::Person.build(\n"
            }
        }),
    );

    let ask = |server: &mut Server, character: u32| {
        let id = server.request(
            "textDocument/signatureHelp",
            serde_json::json!({
                "textDocument": { "uri": scratch },
                "position": { "line": 0, "character": character }
            }),
        );
        server
            .response(&id)
            .response_result
            .expect("signature help")
    };

    let help = ask(&mut server, 17);
    assert_eq!(
        help["signatures"][0]["label"], "HR::Person.build(name, title = ..., *tags, remote: ...)",
        "{help}"
    );
    assert_eq!(help["activeSignature"], 0, "{help}");
    assert_eq!(help["activeParameter"], 0, "{help}");
    assert_eq!(
        help["signatures"][0]["parameters"][0]["label"],
        serde_json::json!([17, 21]),
        "the span covers `name` in the label: {help}"
    );
    assert!(
        help["signatures"][0]["documentation"]["value"]
            .as_str()
            .unwrap_or_default()
            .contains("Build one."),
        "{help}"
    );

    // Two arguments in, and the answer follows the cursor, not the request.
    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": scratch, "version": 2 },
            "contentChanges": [{ "text": "HR::Person.build(\"ada\", \"lead\", \n" }]
        }),
    );
    let help = ask(&mut server, 32);
    assert_eq!(help["activeParameter"], 2, "`*tags`: {help}");

    // A keyword is found by its name, not by where it was written.
    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": scratch, "version": 3 },
            "contentChanges": [{ "text": "HR::Person.build(\"ada\", remote: \n" }]
        }),
    );
    assert_eq!(ask(&mut server, 32)["activeParameter"], 3, "`remote:`");

    // And outside a call there is nothing to say, which closes the popup.
    server.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": scratch, "version": 4 },
            "contentChanges": [{ "text": "HR::Person\n" }]
        }),
    );
    assert_eq!(ask(&mut server, 10), serde_json::Value::Null);

    shut_down(server);
}

#[test]
fn document_highlight_over_stdio() {
    // The end-to-end highlight claim, which needs a real server: the answer's two halves come from
    // different places (a Prism walk of the buffer for the local, the graph for the method), and an
    // editor cannot tell, because both arrive as ranges in its negotiated encoding over a buffer
    // the disk has never seen.
    let root = fixture();
    let mut server = started(root.path(), modern_client(root.path()));

    let scratch = uri_of(root.path(), "lib/scratch.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": scratch,
                "languageId": "ruby",
                "version": 1,
                // `total` is a local in one method and a different local in the other; `sum` is a
                // method with a call. The comment is the word match this replaces.
                "text": "class Till\n  # total is a word here\n  def sum(total)\n    total + 1\n  end\n\n  def other\n    total = 2\n    sum(total)\n  end\nend\n"
            }
        }),
    );

    let ask = |server: &mut Server, line: u32, character: u32| {
        let id = server.request(
            "textDocument/documentHighlight",
            serde_json::json!({
                "textDocument": { "uri": scratch },
                "position": { "line": line, "character": character }
            }),
        );
        server.response(&id).response_result.expect("a response")
    };

    // The parameter on line 2 and its use on line 3, and nothing on lines 7 and 8, where the other
    // method spells the same letters.
    let found = ask(&mut server, 2, 12);
    assert_eq!(found.as_array().map(Vec::len), Some(2), "{found}");
    assert_eq!(
        found[0]["range"]["start"],
        serde_json::json!({ "line": 2, "character": 10 })
    );
    assert_eq!(found[0]["kind"], 3, "the parameter is a write: {found}");
    assert_eq!(
        found[1]["range"]["start"],
        serde_json::json!({ "line": 3, "character": 4 })
    );
    assert_eq!(found[1]["kind"], 2, "and its use is a read: {found}");

    // The other scope's `total`, which is the assignment and the argument beside it.
    let found = ask(&mut server, 7, 6);
    assert_eq!(found.as_array().map(Vec::len), Some(2), "{found}");
    assert_eq!(
        found[0]["range"]["start"],
        serde_json::json!({ "line": 7, "character": 4 })
    );
    assert_eq!(
        found[1]["range"]["start"],
        serde_json::json!({ "line": 8, "character": 8 })
    );

    // The graph's half: the method's own name, and the call to it.
    let found = ask(&mut server, 8, 5);
    assert_eq!(found.as_array().map(Vec::len), Some(2), "{found}");
    assert_eq!(
        found[0]["range"]["start"],
        serde_json::json!({ "line": 2, "character": 6 })
    );
    assert_eq!(found[0]["kind"], 3, "the definition is a write: {found}");
    assert_eq!(
        found[1]["range"]["start"],
        serde_json::json!({ "line": 8, "character": 4 })
    );

    // And `null` in the comment, which hands word matching back to the client where ya-lsp cannot
    // speak.
    assert_eq!(ask(&mut server, 1, 6), serde_json::Value::Null);

    shut_down(server);
}

#[test]
fn type_hierarchy_over_stdio() {
    // Three requests and one round trip, which is why this needs a real server: the item the client
    // expands is the item the server sent, `data` and all, and nothing in-process can check that
    // the field survives serialisation both ways. The capability is checked too, because
    // `lsp-types` has no field for it and it is added on the way to JSON.
    let root = fixture();
    let mut server = started(root.path(), modern_client(root.path()));

    let scratch = uri_of(root.path(), "lib/scratch.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": scratch,
                "languageId": "ruby",
                "version": 1,
                "text": "module Greet\nend\n\nclass Base\nend\n\nclass Middle < Base\n  include Greet\nend\n\nclass Leaf < Middle\nend\n"
            }
        }),
    );

    let ask = |server: &mut Server, method: &str, params: serde_json::Value| {
        let id = server.request(method, params);
        server.response(&id).response_result.expect("a response")
    };

    // The cursor on the `Leaf` in `class Leaf`.
    let prepared = ask(
        &mut server,
        "textDocument/prepareTypeHierarchy",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "position": { "line": 10, "character": 6 }
        }),
    );
    assert_eq!(prepared.as_array().map(Vec::len), Some(1), "{prepared}");
    assert_eq!(prepared[0]["name"], "Leaf");
    assert_eq!(prepared[0]["kind"], 5, "SymbolKind::Class: {prepared}");
    assert!(prepared[0]["data"].is_string(), "{prepared}");

    // Expanded upwards with the item exactly as it arrived, as an editor sends it.
    let supertypes = ask(
        &mut server,
        "typeHierarchy/supertypes",
        serde_json::json!({ "item": prepared[0] }),
    );
    assert_eq!(
        supertypes
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| item["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["Middle", "Greet", "Base"],
        "{supertypes}"
    );

    // And downwards from the top of the chain, two generations at once.
    let base = ask(
        &mut server,
        "textDocument/prepareTypeHierarchy",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "position": { "line": 3, "character": 6 }
        }),
    );
    let subtypes = ask(
        &mut server,
        "typeHierarchy/subtypes",
        serde_json::json!({ "item": base[0] }),
    );
    assert_eq!(
        subtypes
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| item["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["Leaf", "Middle"],
        "{subtypes}"
    );

    // A method is not a type, and the editor is told so with `null`, not an empty tree.
    assert_eq!(
        ask(
            &mut server,
            "textDocument/prepareTypeHierarchy",
            serde_json::json!({
                "textDocument": { "uri": scratch },
                "position": { "line": 7, "character": 4 }
            }),
        ),
        serde_json::Value::Null
    );

    shut_down(server);
}

#[test]
fn rename_over_stdio() {
    // Through the shipped binary because this is the one request that *writes*: the edit must
    // survive serialisation and arrive as something an editor can apply, and its version comes from
    // the `didOpen`, not from anything in-process.
    let root = fixture();
    let mut server = started(root.path(), modern_client(root.path()));

    let scratch = uri_of(root.path(), "lib/scratch.rb");
    let source = "class Ledger
  def total(amount)
    amount * 2
  end
end

Ledger.new
";
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": scratch,
                "languageId": "ruby",
                "version": 7,
                "text": source
            }
        }),
    );

    let ask = |server: &mut Server, method: &str, params: serde_json::Value| {
        let id = server.request(method, params);
        server.response(&id).response_result.expect("a response")
    };

    // The cursor on the `Ledger` in `class Ledger`.
    let at_the_class = serde_json::json!({
        "textDocument": { "uri": scratch },
        "position": { "line": 0, "character": 6 }
    });
    assert_eq!(
        ask(
            &mut server,
            "textDocument/prepareRename",
            at_the_class.clone()
        ),
        serde_json::json!({
            "start": { "line": 0, "character": 6 },
            "end": { "line": 0, "character": 12 },
        })
    );

    let mut params = at_the_class.clone();
    params["newName"] = serde_json::json!("Journal");
    let edit = ask(&mut server, "textDocument/rename", params);
    let changes = &edit["documentChanges"][0];
    assert_eq!(changes["textDocument"]["uri"], scratch);
    // The version the buffer was opened at, which lets the client refuse an edit the user has typed
    // past.
    assert_eq!(changes["textDocument"]["version"], 7);
    assert_eq!(
        changes["edits"],
        serde_json::json!([
            {
                "range": {
                    "start": { "line": 0, "character": 6 },
                    "end": { "line": 0, "character": 12 }
                },
                "newText": "Journal"
            },
            {
                "range": {
                    "start": { "line": 6, "character": 0 },
                    "end": { "line": 6, "character": 6 }
                },
                "newText": "Journal"
            }
        ]),
        "{edit}"
    );

    // A method is refused, and the refusal reaches the user as a message, not only as a `null` the
    // editor turns into its own generic sentence. The message goes out before the response, so it
    // is read first: `response` steps over notifications and would drop it.
    let id = server.request(
        "textDocument/prepareRename",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "position": { "line": 1, "character": 7 }
        }),
    );
    let mut said = String::new();
    while !said.contains("renaming a method") {
        // Stepping over whatever startup said: this client takes no dynamic registrations, so it
        // was already told files cannot be watched.
        said = server.notification("window/showMessage")["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
    }
    assert_eq!(
        server.response(&id).response_result.expect("a response"),
        serde_json::Value::Null
    );

    shut_down(server);
}

#[test]
fn a_client_without_link_support_gets_plain_locations() {
    // LSP lets a server answer `definition` with `LocationLink`s only if the client said it
    // understands them. Sending the richer shape unasked is not graceful degradation: some clients
    // fail to parse it outright.
    let root = fixture();
    let mut server = started(root.path(), initialize_params(root.path()));

    let main = uri_of(root.path(), "lib/main.rb");
    let id = server.request(
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": main },
            "position": { "line": 2, "character": 2 }
        }),
    );
    let targets = server.response(&id).response_result.expect("definition");
    assert!(targets[0]["uri"].is_string(), "{targets}");
    assert!(targets[0]["targetUri"].is_null(), "{targets}");
    assert!(targets[0]["range"].is_object(), "{targets}");

    shut_down(server);
}

#[test]
fn implementations_are_advertised_and_answered_as_plain_locations() {
    // The asymmetry comes from a real client: Claude Code sends `definition.linkSupport: true` and
    // no `textDocument.implementation` capability, so the negotiated shape is `LocationLink[]` for
    // one goto and `Location[]` for the other. A server reading one flag for both sends an agent a
    // shape it never asked for, and `modern_client` is exactly that pair of capabilities.
    //
    // Over a real process because the capability is the other half: an answer nothing advertised is
    // one no client ever requests.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/loud.rb"),
        "class Loud < Person\n  def shout\n  end\nend\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("lib/caller.rb"),
        "person = Person.new\nperson.shout\n",
    )
    .unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", modern_client(root.path()));
    let advertised = server.response(&id).response_result.expect("initialize");
    assert_eq!(
        advertised["capabilities"]["implementationProvider"],
        serde_json::json!(true),
        "{advertised}"
    );
    server.notify("initialized", serde_json::json!({}));

    let caller = uri_of(root.path(), "lib/caller.rb");
    let id = server.request(
        "textDocument/implementation",
        serde_json::json!({
            "textDocument": { "uri": caller },
            "position": { "line": 1, "character": 8 }
        }),
    );
    let found = server
        .response(&id)
        .response_result
        .expect("implementation");

    // `Location[]`, not `LocationLink[]`, while `definition` at the same cursor is the other way
    // round: the whole point of the pair.
    assert!(found[0]["uri"].is_string(), "{found}");
    assert!(found[0]["targetUri"].is_null(), "{found}");
    let files: Vec<String> = found
        .as_array()
        .expect("locations")
        .iter()
        .map(|location| {
            location["uri"]
                .as_str()
                .unwrap_or_default()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(files, ["person.rb", "loud.rb"], "{found}");

    let id = server.request(
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": caller },
            "position": { "line": 1, "character": 8 }
        }),
    );
    let defined = server.response(&id).response_result.expect("definition");
    assert!(defined[0]["targetUri"].is_string(), "{defined}");

    shut_down(server);
}

#[test]
fn a_type_definition_is_advertised_and_answers_where_a_definition_does_not() {
    // The pair that shows what this request is for. At `person`, the *definition* jump answers
    // nothing (a local is a line the reader can already see), and the type jump answers
    // `class Person`, the one thing about that local the line does not say.
    //
    // Over a real process because the capability is the other half of the feature: an answer
    // nothing advertised is one no client requests. `modern_client` declares `linkSupport` for
    // `definition` only, so the shape here is `Location[]`: the third goto reading its own flag,
    // not a neighbour's.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/caller.rb"),
        "person = Person.new\nperson.shout\n",
    )
    .unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", modern_client(root.path()));
    let advertised = server.response(&id).response_result.expect("initialize");
    assert_eq!(
        advertised["capabilities"]["typeDefinitionProvider"],
        serde_json::json!(true),
        "{advertised}"
    );
    server.notify("initialized", serde_json::json!({}));

    let caller = uri_of(root.path(), "lib/caller.rb");
    let at = serde_json::json!({
        "textDocument": { "uri": caller },
        "position": { "line": 1, "character": 2 }
    });
    let id = server.request("textDocument/typeDefinition", at.clone());
    let found = server
        .response(&id)
        .response_result
        .expect("typeDefinition");

    assert!(found[0]["uri"].is_string(), "{found}");
    assert!(found[0]["targetUri"].is_null(), "{found}");
    assert!(
        found[0]["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("person.rb"),
        "{found}"
    );

    let id = server.request("textDocument/definition", at);
    let defined = server.response(&id).response_result.expect("definition");
    assert!(defined.is_null(), "{defined}");

    shut_down(server);
}

#[test]
fn a_declaration_no_file_declares_is_read_by_asking_this_server_for_the_document_it_wrote() {
    // Showing a generated document, over a real process. `Point = Struct.new(:x, :y)` declares `x`
    // and `y` without writing either down, so the only statement of what they are is RBS this
    // server generated, indexed under a scheme with no file behind it. That stops it becoming a
    // `Location`, and also stops anyone reading it. The three steps here are the door: a code
    // action names the document, the command asks the client to show it, and the client asks this
    // server for its content.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/point.rb"),
        "Point = Struct.new(:x, :y)\n",
    )
    .unwrap();
    std::fs::write(root.path().join("lib/uses.rb"), "Point.new(1, 2).x\n").unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", reading_client(root.path()));
    let advertised = server.response(&id).response_result.expect("initialize");
    let command = advertised["capabilities"]["executeCommandProvider"]["commands"][0]
        .as_str()
        .expect("one command")
        .to_owned();
    server.notify("initialized", serde_json::json!({}));

    let uses = uri_of(root.path(), "lib/uses.rb");
    let at = serde_json::json!({ "line": 0, "character": 16 });
    let id = server.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": { "uri": uses },
            "range": { "start": at, "end": at },
            "context": { "diagnostics": [] },
        }),
    );
    let actions = server.response(&id).response_result.expect("code actions");
    assert_eq!(
        actions[0]["title"], "Show the RBS ya-lsp generated for Point, from point.rb",
        "{actions}"
    );
    // No kind and no edit: the one action in the menu that changes nothing on disk.
    assert!(actions[0]["kind"].is_null(), "{actions}");
    assert!(actions[0]["edit"].is_null(), "{actions}");
    // The same name that was advertised, which the client registered with the editor.
    assert_eq!(actions[0]["command"]["command"], serde_json::json!(command));
    let named = actions[0]["command"]["arguments"][0]
        .as_str()
        .expect("the document")
        .to_owned();
    assert!(named.starts_with("ya-lsp-generated:"), "{named}");

    // Running it asks the client to open that document, the command's only job; the response
    // carries nothing, because the work travels the other way.
    let id = server.request(
        "workspace/executeCommand",
        serde_json::json!({ "command": command, "arguments": [named] }),
    );
    let shown = server.server_request("window/showDocument");
    assert_eq!(shown.params["uri"], serde_json::json!(named), "{shown:?}");
    assert_eq!(shown.params["takeFocus"], true);
    assert_eq!(
        server.response(&id).response_result.expect("the command"),
        serde_json::Value::Null
    );

    // And what the client puts in the window, spelled as VS Code would spell it back (every
    // component decoded on the way in and re-escaped on the way out), so both colons arrive as
    // `%3A` and it is still the same document.
    let spelled = named
        .replacen("ya-lsp-generated:", "\u{0}", 1)
        .replace(':', "%3A");
    let spelled = spelled.replacen('\u{0}', "ya-lsp-generated:", 1);
    assert_ne!(spelled, named);
    let id = server.request(
        "workspace/textDocumentContent",
        serde_json::json!({ "uri": spelled }),
    );
    let content = server.response(&id).response_result.expect("the content");
    let rbs = content["text"].as_str().expect("text");
    assert!(rbs.contains("class Point"), "{rbs}");
    assert!(rbs.contains("def x:"), "{rbs}");

    shut_down(server);
}

#[test]
fn moving_a_model_in_the_file_tree_renames_the_class_before_the_move_happens() {
    // Renaming a class with its file, over a real process. The editor asks *before* moving the file
    // and applies the answer alongside the move, so `app/models/purchase.rb` arrives already
    // declaring `Purchase`. A file that kept `Order` would raise `NameError` on the next boot, at a
    // point nothing connects back to the drag in the file tree.
    let root = fixture();
    // `rails.enabled` is `auto`, and this is what it detects: an engine has no
    // `config/application.rb` and a fresh clone has no `Gemfile.lock`, so detection reads both, and
    // one is enough.
    std::fs::create_dir_all(root.path().join("config")).unwrap();
    std::fs::create_dir_all(root.path().join("app/models")).unwrap();
    std::fs::write(
        root.path().join("config/application.rb"),
        "module Shop\nend\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("app/models/order.rb"),
        "class Order\n  def total\n  end\nend\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("app/models/basket.rb"),
        "class Basket\n  def item\n    Order.new\n  end\nend\n",
    )
    .unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", modern_client(root.path()));
    let advertised = server.response(&id).response_result.expect("initialize");
    assert_eq!(
        advertised["capabilities"]["workspace"]["fileOperations"]["willRename"]["filters"][0]["pattern"]
            ["glob"],
        "**/*.rb"
    );
    server.notify("initialized", serde_json::json!({}));

    let id = server.request(
        "workspace/willRenameFiles",
        serde_json::json!({
            "files": [{
                "oldUri": uri_of(root.path(), "app/models/order.rb"),
                "newUri": uri_of(root.path(), "app/models/purchase.rb"),
            }],
        }),
    );
    let edit = server.response(&id).response_result.expect("an edit");
    // Both files: the class itself and the one call of it. An edit changing only the declaring file
    // would break the project just as surely as no edit.
    let changes = edit["documentChanges"].as_array().expect("changes");
    let mut renamed: Vec<String> = changes
        .iter()
        .map(|change| {
            let uri = change["textDocument"]["uri"].as_str().unwrap_or_default();
            let name = uri.rsplit('/').next().unwrap_or(uri);
            let edits = change["edits"].as_array().expect("edits");
            format!("{name} {} -> {}", edits.len(), edits[0]["newText"])
        })
        .collect();
    renamed.sort();
    assert_eq!(
        renamed,
        ["basket.rb 1 -> \"Purchase\"", "order.rb 1 -> \"Purchase\""],
        "{edit}"
    );

    // And a move that changes no class is answered `null`, not an empty edit: a client that gets
    // one still puts a pointless undo step in front of the user.
    let id = server.request(
        "workspace/willRenameFiles",
        serde_json::json!({
            "files": [{
                "oldUri": uri_of(root.path(), "lib/main.rb"),
                "newUri": uri_of(root.path(), "lib/start.rb"),
            }],
        }),
    );
    assert_eq!(
        server.response(&id).response_result.expect("no edit"),
        serde_json::Value::Null
    );

    shut_down(server);
}

#[test]
fn two_servers_in_one_window_do_not_register_one_command_name() {
    // The failure this guards against is not subtle, and not this feature's:
    // `vscode-languageclient` turns `executeCommandProvider` into
    // `vscode.commands.registerCommand`, which throws on a name already taken, and the extension
    // starts one client per workspace folder. A shared name would cost a multi-root workspace every
    // feature in its second folder.
    let first = fixture();
    let second = fixture();
    let mut names = Vec::new();
    for root in [&first, &second] {
        let mut server = Server::start(root.path());
        let id = server.request("initialize", reading_client(root.path()));
        let advertised = server.response(&id).response_result.expect("initialize");
        names.push(
            advertised["capabilities"]["executeCommandProvider"]["commands"][0]
                .as_str()
                .expect("one command")
                .to_owned(),
        );
        server.notify("initialized", serde_json::json!({}));
        shut_down(server);
    }
    assert_ne!(names[0], names[1], "{names:?}");
}

#[test]
fn a_client_that_cannot_read_a_generated_document_is_never_shown_the_door_to_one() {
    // Neovim's shape: it answers `window/showDocument`, but its LSP client implements nothing that
    // would fill the buffer that opens, so it would get an empty window named after a URI. The
    // action is withheld instead, and it appears, with no change on this side, once the client can
    // read one.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/point.rb"),
        "Point = Struct.new(:x, :y)\n",
    )
    .unwrap();
    std::fs::write(root.path().join("lib/uses.rb"), "Point.new(1, 2).x\n").unwrap();

    let mut client = modern_client(root.path());
    client["capabilities"]["window"]["showDocument"] = serde_json::json!({ "support": true });
    let mut server = started(root.path(), client);
    let uses = uri_of(root.path(), "lib/uses.rb");
    let at = serde_json::json!({ "line": 0, "character": 16 });
    let id = server.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": { "uri": uses },
            "range": { "start": at, "end": at },
            "context": { "diagnostics": [] },
        }),
    );
    let actions = server.response(&id).response_result.expect("code actions");
    assert_eq!(actions, serde_json::Value::Null, "{actions}");

    shut_down(server);
}

#[test]
fn a_declaration_is_advertised_and_answers_the_signature_where_the_definition_answers_the_source() {
    // The pair again, the other way round from the type jump: here both requests answer, with
    // **different files**. `definition` opens the `def` somebody wrote and `declaration` opens the
    // `.rbs` saying what it takes: the only question this server answers with a file the reader
    // would not otherwise open.
    //
    // Over a real process for the capability, as above. `modern_client` declares `linkSupport` for
    // `definition` alone, so the shape here is `Location[]`: the fourth goto reading the fourth
    // flag.
    let root = fixture();
    std::fs::create_dir_all(root.path().join("sig")).unwrap();
    std::fs::write(
        root.path().join("sig/person.rbs"),
        "class Person\n  def shout: () -> void\nend\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("lib/caller.rb"),
        "person = Person.new\nperson.shout\n",
    )
    .unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", modern_client(root.path()));
    let advertised = server.response(&id).response_result.expect("initialize");
    assert_eq!(
        advertised["capabilities"]["declarationProvider"],
        serde_json::json!(true),
        "{advertised}"
    );
    server.notify("initialized", serde_json::json!({}));

    let caller = uri_of(root.path(), "lib/caller.rb");
    let at = serde_json::json!({
        "textDocument": { "uri": caller },
        "position": { "line": 1, "character": 9 }
    });
    let id = server.request("textDocument/declaration", at.clone());
    let declared = server.response(&id).response_result.expect("declaration");
    assert!(
        declared[0]["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("person.rbs"),
        "{declared}"
    );
    assert!(declared[0]["targetUri"].is_null(), "{declared}");

    let id = server.request("textDocument/definition", at);
    let defined = server.response(&id).response_result.expect("definition");
    assert!(
        defined[0]["targetUri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("person.rb"),
        "{defined}"
    );

    shut_down(server);
}

/// Wait for the process, killing it rather than hanging the suite if it will not leave.
fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("server did not exit after `exit`");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// A gem the project depends on becomes navigable, and the editor is told it is happening.
///
/// The acceptance criterion reduced to something CI can run: no Ruby runs anywhere, the gem is
/// found from `Gemfile.lock` plus a `GEM_HOME`, and goto-definition crosses from the project into
/// it.
#[test]
fn gems_are_indexed_in_the_background_and_become_navigable() {
    let (root, gem_home) = bundled_fixture();
    let mut server = Server::start_with_env(root.path(), &[("GEM_HOME", gem_home.path())]);

    let id = server.request("initialize", modern_client(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // The server opens the progress stream with a request of its own. Answering it is the client's
    // job, and a server that could not cope with the answer arriving at any moment would wedge
    // here.
    let mut created: Option<RequestId> = None;
    let mut token = serde_json::Value::Null;
    let mut begun = false;
    let mut ended = false;
    while !ended {
        match server.read() {
            Message::Request(request) => {
                assert_eq!(request.method, "window/workDoneProgress/create");
                token = request.params["token"].clone();
                created = Some(request.id.clone());
                server.send(Message::Response(Response {
                    id: request.id,
                    response_result: Ok(serde_json::Value::Null),
                }));
            }
            Message::Notification(notification) if notification.method == "$/progress" => {
                assert_eq!(notification.params["token"], token, "one token per stream");
                match notification.params["value"]["kind"].as_str() {
                    Some("begin") => begun = true,
                    Some("end") => ended = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    assert!(
        created.is_some(),
        "progress must be created before it is reported"
    );
    assert!(begun, "a stream that ends without beginning is malformed");

    // Now the payoff: a constant that exists only inside the gem.
    let uri = uri_of(root.path(), "lib/main.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": "ruby", "version": 1,
            "text": "Shouty::Megaphone.new\n",
        }}),
    );

    let id = server.request(
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 10 },
        }),
    );
    let result = server.response(&id).response_result.expect("definition");
    let target = result[0]["targetUri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a gem target, got {result}"));
    assert!(
        target.ends_with("gems/shouty-1.2.3/lib/shouty.rb"),
        "{result}"
    );

    shut_down(server);
}

/// The shipped binary asks the client to claim the gem it found, and names the real directory.
///
/// **What only this covers.** The negotiation is unit-tested and the arbitration between clients is
/// tested in TypeScript, but only this joins them against the real executable: `initialize_params`
/// declares `synchronization.dynamicRegistration: false`, so every other test here is a client that
/// would never get a registration.
///
/// Four things are asserted that no in-process test can reach:
///
/// 1. The registration crosses the wire at all.
/// 2. It carries the gem home this process was pointed at by `GEM_HOME`, not a path computed in a
///    test.
/// 3. The watcher's registration is distinguishable from it by id, which an extension driving
///    several servers needs.
/// 4. A request inside the gem file the registration just named is answered over stdio.
///
/// The client takes only three of the requests on purpose. A real client need not take all of them,
/// and getting back exactly three plus synchronisation pins the per-capability filter:
/// `doRegisterCapability` rejects the *whole* array at the first method it has no feature for, so
/// sending one the client declined would cost every method after it.
#[test]
fn the_binary_asks_the_client_to_claim_the_gem_it_found() {
    let (root, gem_home) = bundled_fixture();
    let mut server = Server::start_with_env(root.path(), &[("GEM_HOME", gem_home.path())]);

    let dynamic = serde_json::json!({ "dynamicRegistration": true });
    let mut params = modern_client(root.path());
    params["capabilities"]["textDocument"]["synchronization"] = dynamic.clone();
    params["capabilities"]["textDocument"]["hover"] = dynamic.clone();
    params["capabilities"]["textDocument"]["definition"] =
        serde_json::json!({ "dynamicRegistration": true, "linkSupport": true });
    params["capabilities"]["textDocument"]["documentSymbol"] = serde_json::json!({
        "dynamicRegistration": true,
        "hierarchicalDocumentSymbolSupport": true,
    });
    // The watcher registers on this same channel, at `initialized` rather than once the bundle is
    // known, so both are in flight here.
    params["capabilities"]["workspace"]["didChangeWatchedFiles"] = dynamic;

    let id = server.request("initialize", params);
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // Read until the document registration has arrived *and* the gem index has finished, in either
    // order: the registration is sent when the roots are discovered, before the first gem file is
    // indexed, and the outline below needs the index.
    let mut watcher: Option<serde_json::Value> = None;
    let mut documents: Option<serde_json::Value> = None;
    let mut indexed = false;
    while !indexed || documents.is_none() {
        match server.read() {
            Message::Request(request) => {
                if request.method == "client/registerCapability" {
                    let registrations = request.params["registrations"].clone();
                    let first = registrations[0]["id"].as_str().unwrap_or_default();
                    if first.starts_with(ya_lsp::server::capabilities::DOCUMENTS_ID_PREFIX) {
                        documents = Some(registrations);
                    } else {
                        watcher = Some(registrations);
                    }
                }
                // Answered whatever it was: this one and `window/workDoneProgress/create` are both
                // server-initiated, and a server that wedged on the reply would hang here.
                server.send(Message::Response(Response {
                    id: request.id,
                    response_result: Ok(serde_json::Value::Null),
                }));
            }
            Message::Notification(notification)
                if notification.method == "$/progress"
                    && notification.params["value"]["kind"] == "end" =>
            {
                indexed = true;
            }
            _ => {}
        }
    }

    let watcher = watcher.expect("the watcher registers too, and is not one of these");
    assert_eq!(watcher[0]["method"], "workspace/didChangeWatchedFiles");
    assert!(
        watcher[0]["registerOptions"]
            .get("documentSelector")
            .is_none(),
        "it narrows no documents, which is why an extension forwards it untouched: {watcher}"
    );

    let documents = documents.expect("the gem the bundle resolved to has to be claimed");
    let methods: Vec<&str> = documents
        .as_array()
        .expect("a list of registrations")
        .iter()
        .map(|registration| registration["method"].as_str().expect("a method"))
        .collect();
    assert_eq!(
        methods,
        vec![
            "textDocument/hover",
            "textDocument/definition",
            "textDocument/documentSymbol",
            "textDocument/didChange",
            "textDocument/didClose",
            "textDocument/didSave",
            // Last, because registering it walks the already-open documents and sends a `didOpen`
            // for each one the new selector newly matches.
            "textDocument/didOpen",
        ],
        "only the three this client said it takes, and synchronisation after them"
    );

    let bases: Vec<&str> = documents[0]["registerOptions"]["documentSelector"]
        .as_array()
        .expect("every registration carries its own selector")
        .iter()
        .map(|filter| filter["pattern"]["baseUri"].as_str().expect("a URI string"))
        .collect();
    let workspace = url::Url::from_file_path(root.path()).unwrap().to_string();
    assert!(
        bases.iter().any(|base| base.ends_with("gems/shouty-1.2.3")),
        "the directory `GEM_HOME` pointed this process at, not one a test computed: {bases:?}"
    );
    assert!(
        bases.iter().all(|base| !base.starts_with(&workspace)),
        "the client already claimed the workspace, and twice means two providers: {bases:?}"
    );

    // The payoff, and the one step no in-process test reaches: a request inside the file the
    // registration just named, over stdio, answered by the shipped binary.
    const GEM_SOURCE: &str = "module Shouty\n  class Megaphone\n  end\nend\n";
    let gem_file =
        url::Url::from_file_path(gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .unwrap()
            .to_string();
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": gem_file, "languageId": "ruby", "version": 1, "text": GEM_SOURCE,
        }}),
    );
    let id = server.request(
        "textDocument/documentSymbol",
        serde_json::json!({ "textDocument": { "uri": gem_file } }),
    );
    let outline = server
        .response(&id)
        .response_result
        .expect("documentSymbol");
    assert_eq!(outline[0]["name"], "Shouty", "{outline}");
    assert_eq!(outline[0]["children"][0]["name"], "Megaphone", "{outline}");

    shut_down(server);
}

/// Ask `workspace/symbol` until the answer is the wanted one, or until [`WATCH_TIMEOUT`].
///
/// A poll, not a wait on a message, because the change produces no notification of its own:
/// `publishDiagnostics` fires only for a file with something to say, and a class appearing is not
/// that. The interval is small enough to be invisible where arming is instant and large enough not
/// to busy-loop where it is not.
fn symbol_settles(server: &mut Server, query: &str, found: bool) -> bool {
    symbol_settles_within(server, query, found, WATCH_TIMEOUT)
}

/// The same, on the caller's own deadline, for the one caller that means to give up early.
fn symbol_settles_within(
    server: &mut Server,
    query: &str,
    found: bool,
    patience: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + patience;
    while std::time::Instant::now() < deadline {
        let id = server.request("workspace/symbol", serde_json::json!({ "query": query }));
        let answer = server
            .response(&id)
            .response_result
            .expect("workspace/symbol");
        if answer.is_null() != found {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Write a probe until the server's own watcher reports one, then remove it again.
///
/// **An indexed workspace does not mean an armed watcher, and in between, a file written on disk is
/// lost for the life of the process.** `watcher::watch` returns as soon as its thread exists, and
/// arming happens on that thread, on purpose, so no client waits for a walk a network mount can
/// make arbitrarily slow (`concurrency.md` has the argument). So a test that writes once inside
/// that window then waits [`WATCH_TIMEOUT`] for an event that will never come, and whether it lands
/// inside depends on how fast the *index* finished, a property of the build, not of the server. A
/// single write made this test flaky under a faster dependency profile.
///
/// So: rewrite the probe instead of waiting on one, as `watcher::tests::armed` does one layer down.
/// The removal is waited on too, so nothing after this sees a class the fixture never had.
fn watcher_armed(server: &mut Server, root: &std::path::Path) {
    let probe = root.join("lib/probe_for_the_watcher.rb");
    let deadline = std::time::Instant::now() + WATCH_TIMEOUT;
    while std::time::Instant::now() < deadline {
        std::fs::write(&probe, "class ProbeForTheWatcher\nend\n").expect("the probe file");
        if symbol_settles_within(
            server,
            "ProbeForTheWatcher",
            true,
            Duration::from_millis(500),
        ) {
            std::fs::remove_file(&probe).expect("the probe file");
            assert!(
                symbol_settles(server, "ProbeForTheWatcher", false),
                "the probe was watched on the way in and not on the way out"
            );
            return;
        }
    }
    panic!("the server's own watcher never armed");
}

#[test]
fn a_client_with_no_watcher_of_its_own_still_follows_the_files_on_disk() {
    // **File watching, through the shipped binary.** `initialize_params` advertises no
    // `workspace.didChangeWatchedFiles`, so `capabilities::watched_files` answers `None`, nothing
    // is registered, and ya-lsp watches the project itself. That is the setup for Claude Code,
    // Helix, eglot and Neovim on Linux. Without it, the three changes below would be invisible for
    // the life of the process, and every symptom would look like the server being wrong rather than
    // never being told.
    //
    // No `didOpen` anywhere, on purpose: an agent edits through a shell, and a file it never opened
    // is the case a client's own watcher would have covered.
    let root = fixture();
    let mut server = started(root.path(), initialize_params(root.path()));
    assert!(
        symbol_settles(&mut server, "Person", true),
        "the workspace was not indexed at all"
    );
    watcher_armed(&mut server, root.path());

    let created = root.path().join("lib/comment.rb");
    std::fs::write(&created, "class Comment\nend\n").unwrap();
    assert!(
        symbol_settles(&mut server, "Comment", true),
        "a file created on disk never reached the index"
    );

    std::fs::write(&created, "class Comment\n  def body\n  end\nend\n").unwrap();
    assert!(
        symbol_settles(&mut server, "body", true),
        "a method added on disk never reached the index"
    );

    std::fs::remove_file(&created).unwrap();
    assert!(
        symbol_settles(&mut server, "Comment", false),
        "a file deleted on disk was still being answered about"
    );

    shut_down(server);
}

/// `--licenses` prints what the binary is obliged to carry, and exits cleanly.
///
/// The one test that runs the binary as a *command* rather than a server. It exists because the
/// obligation belongs to the artifact: a bare `ya-lsp` attached to a release or installed with
/// `cargo install` has no licence file beside it, and the BSD-2-Clause material embedded in it
/// must reach whoever holds it somehow.
#[test]
fn licenses_are_carried_by_the_binary_itself() {
    let output = Command::new(env!("CARGO_BIN_EXE_ya-lsp"))
        .arg("--licenses")
        .output()
        .expect("ran ya-lsp --licenses");

    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("utf-8");

    for phrase in [
        // Ours.
        "The MIT License (MIT)",
        // The three things BSD-2-Clause names: the notice, the conditions, the disclaimer.
        "Copyright (C) 2019 Soutaro Matsumoto",
        "Redistributions in binary form must reproduce the above copyright",
        "THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS",
        // The other half of rbs's dual licence, and where the original lives.
        "rbs is copyrighted free software",
        "https://github.com/ruby/rbs",
    ] {
        assert!(text.contains(phrase), "--licenses is missing: {phrase:?}");
    }

    // Nothing on stderr: this is output, not a diagnostic.
    assert!(
        output.stderr.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The command line, which no editor uses and every packager does.
///
/// `--version` is what a Homebrew formula or a CI step calls to check what it installed; `--help`
/// is what someone types after the binary did nothing they expected. Both write to stdout, the LSP
/// transport in every other mode, hence the assertion that the *other* stream stays empty.
#[test]
fn the_command_line_answers_version_and_help() {
    for flags in [["-V", "--version"], ["-h", "--help"]] {
        for flag in flags {
            let output = Command::new(env!("CARGO_BIN_EXE_ya-lsp"))
                .arg(flag)
                .output()
                .unwrap_or_else(|error| panic!("ran ya-lsp {flag}: {error}"));

            assert!(output.status.success(), "ya-lsp {flag}: {output:?}");
            let text = String::from_utf8(output.stdout).expect("utf-8");
            if flag.contains("version") || flag == "-V" {
                assert!(
                    text.starts_with("ya-lsp ") && text.trim().len() > "ya-lsp ".len(),
                    "ya-lsp {flag} printed {text:?}"
                );
            } else {
                for phrase in ["USAGE:", "--stdio", "--licenses", "YA_LSP_LOG"] {
                    assert!(text.contains(phrase), "ya-lsp {flag} is missing {phrase:?}");
                }
            }
            assert!(
                output.stderr.is_empty(),
                "ya-lsp {flag} wrote to stderr: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

/// An unrecognised argument fails loudly, on stderr, with the usage attached.
///
/// Exiting 0 would let a typo in an editor's configuration look like a server that starts and then
/// says nothing: the most confusing way for this binary to fail.
#[test]
fn an_unrecognised_argument_fails_with_the_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_ya-lsp"))
        .arg("--socket=1234")
        .output()
        .expect("ran ya-lsp");

    assert!(!output.status.success(), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "usage errors belong on stderr: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let text = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        text.contains("--socket=1234"),
        "the argument is not named: {text}"
    );
    assert!(text.contains("USAGE:"), "no usage attached: {text}");
}

/// A transport that breaks before the handshake is a failure, not a quiet success.
///
/// Closing stdin immediately is what an editor that crashed on startup looks like from here. The
/// exit code is all a supervisor can see, so it must be non-zero.
#[test]
fn a_transport_that_never_speaks_exits_non_zero() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ya-lsp"))
        .arg("--stdio")
        .env("YA_LSP_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ya-lsp");

    // Hang up before sending `initialize`.
    drop(child.stdin.take().expect("stdin"));

    let output = child.wait_with_output().expect("wait");
    assert!(!output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        text.contains("ya-lsp: "),
        "the failure should be reported on stderr: {text:?}"
    );
}

/// The first log line names the version, and does not wait for the handshake.
///
/// A pasted log is the only thing a bug report reliably carries, and every line in it is worthless
/// without knowing which build wrote it. Asserted on the path where `initialize` never arrives,
/// because that is the case the placement is for: logged after the handshake, logs from a client
/// that cannot complete one (the hardest reports to reproduce) would carry no version at all.
#[test]
fn startup_logs_the_version_before_the_handshake() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ya-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ya-lsp");

    // Hang up before sending `initialize`, as the test above does.
    drop(child.stdin.take().expect("stdin"));

    let output = child.wait_with_output().expect("wait");
    let text = String::from_utf8(output.stderr).expect("utf-8");
    let expected = format!("ya-lsp {} starting", env!("CARGO_PKG_VERSION"));
    assert!(
        text.contains(&expected),
        "no `{expected}` on stderr: {text:?}"
    );
}

/// Ruby's own core classes: indexed, navigable, and reachable from a literal.
///
/// All of Ruby's own signatures through the wire. The workspace has no gems and asks for core only;
/// which rung of the ladder answered is `workspace::rbs`'s business, and pinning it here would test
/// the machine, not the server.
#[test]
fn built_in_classes_are_indexed_and_complete() {
    let root = fixture();
    std::fs::write(
        root.path().join("ya-lsp.toml"),
        "[gems]\nenabled = false\n\n[rbs]\nstdlib = false\n",
    )
    .unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", modern_client(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));
    drain_progress(&mut server);

    // A buffer the disk has never seen: the situation completion always runs in.
    let uri = uri_of(root.path(), "lib/scratch.rb");
    let source = "greeting = \"hello\"\ngreeting.upc\nString.new\ngreeting.pu\n\
                  Person.new.sho\nOptionPars\n";
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": "ruby", "version": 1, "text": source,
        }}),
    );

    // A local assigned a string literal: `String`'s own instance methods.
    let labels = completion_labels(&mut server, &uri, 1, 12);
    assert!(
        labels.iter().any(|label| label == "upcase"),
        "expected String#upcase after a string local, got {labels:?}"
    );

    // This is what makes the line above mean anything. With core in the graph, the name-based
    // fallback would offer `upcase` too, since it offers every method name there is. Only a
    // receiver the server actually typed can *refuse* `push`, an `Array` method no `String` has
    // ever had.
    let labels = completion_labels(&mut server, &uri, 3, 11);
    assert!(
        !labels.iter().any(|label| label == "push"),
        "a typed String receiver must not offer Array#push, got {labels:?}"
    );

    // `Foo.new.` is the instance, not the class.
    let labels = completion_labels(&mut server, &uri, 4, 14);
    assert!(
        labels.iter().any(|label| label == "shout"),
        "expected Person#shout after Person.new, got {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label == "should_not_exist"),
        "{labels:?}"
    );

    // The class object is still the class: `String.` offers singleton methods.
    let labels = completion_labels(&mut server, &uri, 2, 7);
    assert!(
        labels.iter().any(|label| label == "new"),
        "expected String.new among singleton methods, got {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label == "upcase"),
        "an instance method is not callable on the class, got {labels:?}"
    );

    // The declaration behind it is a real file the editor can open.
    let id = server.request(
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 2, "character": 3 },
        }),
    );
    let result = server.response(&id).response_result.expect("definition");
    let target = result[0]["targetUri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a signature file, got {result}"));
    assert!(target.ends_with("core/string.rbs"), "{result}");

    // Hover carries what RBS knows.
    let id = server.request(
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 2, "character": 3 },
        }),
    );
    let result = server.response(&id).response_result.expect("hover");
    let markdown = result["contents"]["value"].as_str().expect("markdown");
    assert!(markdown.contains("String"), "{markdown}");

    // Core only. `OptionParser` is a stdlib signature, and this workspace asked for neither the
    // stdlib nor gems. Which classes count as "core" is rbs's call and it moves (`Set` and
    // `Pathname` are in `core/` in current rbs, so neither can test this), which is why
    // `the_stdlib_signatures_are_indexed_when_asked_for` asserts the positive side separately.
    let labels = completion_labels(&mut server, &uri, 5, 10);
    assert!(
        !labels.iter().any(|label| label == "OptionParser"),
        "stdlib = false must not index OptionParser, got {labels:?}"
    );

    shut_down(server);
}

/// The same workspace with `[rbs] stdlib` left at its default.
#[test]
fn the_stdlib_signatures_are_indexed_when_asked_for() {
    let root = fixture();
    std::fs::write(root.path().join("ya-lsp.toml"), "[gems]\nenabled = false\n").unwrap();

    let mut server = Server::start(root.path());
    let id = server.request("initialize", modern_client(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));
    drain_progress(&mut server);

    let uri = uri_of(root.path(), "lib/scratch.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": "ruby", "version": 1, "text": "OptionPars\n",
        }}),
    );

    let labels = completion_labels(&mut server, &uri, 0, 10);
    assert!(
        labels.iter().any(|label| label == "OptionParser"),
        "expected a stdlib constant, got {labels:?}"
    );

    shut_down(server);
}

/// The same server, with the signatures turned off entirely.
#[test]
fn built_ins_can_be_turned_off_and_completion_still_answers() {
    let root = fixture();
    let mut server = Server::start(root.path());
    let id = server.request("initialize", modern_client(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    let uri = uri_of(root.path(), "lib/scratch.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": "ruby", "version": 1, "text": "\"hello\".sho\n",
        }}),
    );

    // No `String` in the graph, so the literal receiver has nothing to resolve against. It must
    // degrade to the name-based list (the project's own `Person#shout`), not to the silence a bare
    // `None` from `receiver_for` would produce.
    let labels = completion_labels(&mut server, &uri, 0, 11);
    assert!(
        labels.iter().any(|label| label == "shout"),
        "expected the name-based fallback, got {labels:?}"
    );

    shut_down(server);
}

/// Read `$/progress` to its end, answering the server's `create` request on the way.
fn drain_progress(server: &mut Server) {
    let mut token = serde_json::Value::Null;
    loop {
        match server.read() {
            Message::Request(request) => {
                assert_eq!(request.method, "window/workDoneProgress/create");
                token = request.params["token"].clone();
                server.send(Message::Response(Response {
                    id: request.id,
                    response_result: Ok(serde_json::Value::Null),
                }));
            }
            Message::Notification(notification) if notification.method == "$/progress" => {
                assert_eq!(notification.params["token"], token, "one token per stream");
                if notification.params["value"]["kind"] == "end" {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// The labels `textDocument/completion` offers at a position.
fn completion_labels(server: &mut Server, uri: &str, line: u32, character: u32) -> Vec<String> {
    let id = server.request(
        "textDocument/completion",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }),
    );
    let result = server.response(&id).response_result.expect("completion");
    result["items"]
        .as_array()
        .unwrap_or_else(|| panic!("expected a completion list, got {result}"))
        .iter()
        .map(|item| item["label"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A client that never advertised `window.workDoneProgress` must not be sent any.
#[test]
fn a_client_without_progress_support_is_sent_no_progress() {
    let (root, gem_home) = bundled_fixture();
    let mut server = Server::start_with_env(root.path(), &[("GEM_HOME", gem_home.path())]);

    // `initialize_params` advertises nothing under `window`.
    let id = server.request("initialize", initialize_params(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // Indexing still happens (the gem is navigable either way; the client just is not told when).
    // So the only honest way to wait is to keep asking, as a progress-less editor would as the user
    // works.
    let uri = uri_of(root.path(), "lib/main.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({ "textDocument": {
            "uri": uri, "languageId": "ruby", "version": 1,
            "text": "Shouty::Megaphone.new\n",
        }}),
    );

    let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
    let result = loop {
        let id = server.request(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 10 },
            }),
        );
        let result = server.response(&id).response_result.expect("definition");
        if !result.is_null() {
            break result;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the gem never became navigable"
        );
    };

    let mut sent_progress = false;
    while let Ok(message) = server.incoming.try_recv() {
        match message {
            Message::Notification(notification) => {
                sent_progress |= notification.method == "$/progress";
            }
            Message::Request(request) => {
                sent_progress |= request.method == "window/workDoneProgress/create";
            }
            Message::Response(_) => {}
        }
    }
    assert!(
        !sent_progress,
        "progress was sent to a client that did not ask"
    );
    // And this client, having advertised no `definition.linkSupport`, gets plain `Location`s.
    assert!(result[0]["uri"].is_string(), "{result}");

    shut_down(server);
}

#[test]
fn the_index_follows_files_written_deleted_and_rewritten_on_disk() {
    // The file watcher through the shipped binary. If it covered only `ya-lsp.toml`, a
    // `git checkout`, `git pull`, rebase or `rails g model` would change Ruby under a running
    // server with nothing re-indexing it, and a deleted file would keep its declarations until a
    // restart. Every step here happens with no `didOpen`, because that is the situation: the editor
    // never touched the file.
    let root = fixture();
    let mut server = Server::start(root.path());
    let mut params = initialize_params(root.path());
    params["capabilities"]["workspace"] =
        serde_json::json!({ "didChangeWatchedFiles": { "dynamicRegistration": true } });
    let id = server.request("initialize", params);
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // The registration covers the project's Ruby, not only its configuration.
    let registration = server.server_request("client/registerCapability");
    let watchers = registration.params["registrations"][0]["registerOptions"]["watchers"]
        .as_array()
        .expect("watchers")
        .iter()
        .map(|watcher| {
            watcher["globPattern"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    let spelled = |relative: &str| {
        root.path()
            .join(relative)
            .to_string_lossy()
            .replace('\\', "/")
    };
    assert_eq!(
        watchers,
        vec![
            spelled("ya-lsp.toml"),
            // The second constant, and the only non-Ruby file this server reads: watched so an
            // editor's save of `db/structure.sql` re-settles, never indexed, and spelled here
            // because this is the wire.
            spelled("db/*structure.sql"),
            spelled("**/*.rb"),
            spelled("**/*.erb"),
            // Rails' three other template handlers, plain Ruby and never blanked. On the wire
            // because each glob is a watcher the client registers: a handler the walk indexes but
            // nobody watches goes stale on the first `git checkout` that touches it.
            spelled("**/*.jbuilder"),
            spelled("**/*.builder"),
            spelled("**/*.ruby"),
            spelled("**/*.rbs"),
            spelled("**/*.rake"),
            spelled("**/*.gemspec"),
            spelled("**/*.ru"),
            spelled("**/Rakefile"),
            spelled("**/Gemfile"),
        ],
        "the config, and index.include verbatim — spelled out here rather than derived because \
         this is the wire, and a widened default that reaches an editor by accident is exactly \
         what an end-to-end pin is for"
    );
    server.send(Message::Response(Response::new_ok(
        registration.id,
        serde_json::Value::Null,
    )));

    let main = uri_of(root.path(), "lib/main.rb");
    let place_path = root.path().join("lib/place.rb");
    let place = uri_of(root.path(), "lib/place.rb");

    let definition_of_place = |server: &mut Server| {
        let id = server.request(
            "textDocument/definition",
            // `Place` on the fourth line of lib/main.rb, once it is written below.
            serde_json::json!({
                "textDocument": { "uri": main },
                "position": { "line": 3, "character": 0 }
            }),
        );
        server.response(&id).response_result.expect("definition")
    };

    // A file appears, and the file using it is rewritten: one `git pull` in miniature.
    std::fs::write(&place_path, "class Place\n  def name\n  end\nend\n").unwrap();
    std::fs::write(
        root.path().join("lib/main.rb"),
        "require \"person\"\n\nPerson.new\nPlace.new\n",
    )
    .unwrap();
    server.notify(
        "workspace/didChangeWatchedFiles",
        serde_json::json!({
            "changes": [
                { "uri": place, "type": 1 },
                { "uri": main, "type": 2 },
            ]
        }),
    );

    // Definition follows.
    let targets = definition_of_place(&mut server);
    assert_eq!(targets[0]["uri"], serde_json::json!(place), "{targets}");

    // The outline follows, for a document no editor ever opened.
    let id = server.request(
        "textDocument/documentSymbol",
        serde_json::json!({ "textDocument": { "uri": place } }),
    );
    let symbols = server
        .response(&id)
        .response_result
        .expect("documentSymbol");
    assert_eq!(symbols[0]["name"], "Place", "{symbols}");
    assert_eq!(symbols[1]["name"], "name", "{symbols}");

    // And references, the half that reads the *other* file the change touched.
    let id = server.request(
        "textDocument/references",
        serde_json::json!({
            "textDocument": { "uri": place },
            "position": { "line": 0, "character": 6 },
            "context": { "includeDeclaration": false }
        }),
    );
    let found = server.response(&id).response_result.expect("references");
    assert_eq!(found.as_array().map(Vec::len), Some(1), "{found}");
    assert_eq!(found[0]["uri"], serde_json::json!(main), "{found}");

    // The same file, rewritten: the declarations it used to hold must go with it.
    std::fs::write(&place_path, "class Place\n  def label\n  end\nend\n").unwrap();
    server.notify(
        "workspace/didChangeWatchedFiles",
        serde_json::json!({ "changes": [{ "uri": place, "type": 2 }] }),
    );
    let id = server.request(
        "textDocument/documentSymbol",
        serde_json::json!({ "textDocument": { "uri": place } }),
    );
    let rewritten = server
        .response(&id)
        .response_result
        .expect("documentSymbol");
    assert_eq!(rewritten[1]["name"], "label", "{rewritten}");
    assert!(
        !rewritten
            .as_array()
            .expect("a flat outline")
            .iter()
            .any(|symbol| symbol["name"] == "name"),
        "the method the rewrite removed is still in the index: {rewritten}"
    );

    // And gone: the one change nothing else in the protocol can stand in for. Without it, a deleted
    // file keeps every declaration it had for the life of the process.
    std::fs::remove_file(&place_path).unwrap();
    server.notify(
        "workspace/didChangeWatchedFiles",
        serde_json::json!({ "changes": [{ "uri": place, "type": 3 }] }),
    );
    let targets = definition_of_place(&mut server);
    assert!(
        targets.is_null(),
        "a deleted file must stop answering: {targets}"
    );

    shut_down(server);
}

#[test]
fn the_config_file_reloads_without_a_restart() {
    // Config reload through the shipped binary, not an in-process connection: an editor that takes
    // a dynamic registration is asked to watch `ya-lsp.toml`, and the change it reports really
    // changes an answer. Without the server sending `client/registerCapability`, this works in only
    // one editor: the one whose extension brings its own watcher.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/broken.rb"),
        "class Broken\n  def bar\n",
    )
    .unwrap();
    // Start with the rule off, so the file that cannot parse says nothing.
    std::fs::write(
        root.path().join("ya-lsp.toml"),
        "[diagnostics.rules]\nparse-error = \"off\"\n",
    )
    .unwrap();

    let mut server = Server::start(root.path());
    let mut params = initialize_params(root.path());
    params["capabilities"]["workspace"] =
        serde_json::json!({ "didChangeWatchedFiles": { "dynamicRegistration": true } });
    let id = server.request("initialize", params);
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // The registration comes first, and it names this workspace's file and no other.
    let registration = server.server_request("client/registerCapability");
    let watcher = &registration.params["registrations"][0];
    assert_eq!(watcher["method"], "workspace/didChangeWatchedFiles");
    let config = root.path().join("ya-lsp.toml");
    assert_eq!(
        watcher["registerOptions"]["watchers"][0]["globPattern"],
        serde_json::Value::String(config.to_string_lossy().replace('\\', "/"))
    );
    server.send(Message::Response(Response::new_ok(
        registration.id,
        serde_json::Value::Null,
    )));

    let broken = url::Url::from_file_path(root.path().join("lib/broken.rb"))
        .unwrap()
        .to_string();
    let silent = loop {
        let params = server.notification("textDocument/publishDiagnostics");
        if params["uri"] == serde_json::Value::String(broken.clone()) {
            break params;
        }
    };
    // `parse-error` only: Prism also reports the indentation as a `parse-warning`, and this test is
    // about the one rule the edited file turns on and off.
    assert!(
        !has_parse_error(&silent),
        "the rule was off, so nothing should have reported it: {silent:?}"
    );

    // Now the edit an editor would make, and the notification its watcher would send.
    std::fs::write(&config, "[diagnostics.rules]\nparse-error = \"error\"\n").unwrap();
    server.notify(
        "workspace/didChangeWatchedFiles",
        serde_json::json!({
            "changes": [{
                "uri": url::Url::from_file_path(&config).unwrap().to_string(),
                "type": 2
            }]
        }),
    );

    let reloaded = loop {
        let params = server.notification("textDocument/publishDiagnostics");
        if params["uri"] == serde_json::Value::String(broken.clone()) && has_parse_error(&params) {
            break params;
        }
    };
    let error = reloaded["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .find(|item| item["code"] == "parse-error")
        .expect("the rule the reload turned on");
    assert_eq!(error["severity"], 1); // DiagnosticSeverity::ERROR

    shut_down(server);
}

#[test]
fn the_editor_can_switch_one_rule_off_and_leave_the_others_loud() {
    // Per-rule severity through the shipped binary, via the layer the *editor* sends: `ya-lsp.toml`
    // is what `the_config_file_reloads_without_a_restart` drives, and this is the editor's view of
    // the same setting. The rule is `parse-warning` because a project may already lint for that
    // itself (a public Rails app turns off exactly that ground, `Lint/UselessAssignment`, in
    // `.standard.yml`) while a file that does not parse still deserves a squiggle. One file carries
    // both, so the assertion is that the warning goes and the errors beside it stay.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/unused.rb"),
        "class Unused\n  def call\n    total = 1\n  end\nend\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("lib/broken.rb"),
        "class Broken\n  def call\n    total = 1\n  end\n",
    )
    .unwrap();
    let unused = uri_of(root.path(), "lib/unused.rb");
    let broken = uri_of(root.path(), "lib/broken.rb");

    let mut server = started(root.path(), initialize_params(root.path()));
    let reported = published_codes(&mut server, &[&unused, &broken]);
    assert_eq!(reported[0], ["parse-warning"]);
    assert_eq!(reported[1], ["parse-error", "parse-error", "parse-warning"]);
    shut_down(server);

    // The same workspace, started with what the extension sends for `ya-lsp.diagnostics.rules`.
    // `lib/unused.rb` now has nothing to report and is never published, which is why the assertion
    // is on the file that still has errors.
    let mut params = initialize_params(root.path());
    params["initializationOptions"] =
        serde_json::json!({ "diagnostics": { "rules": { "parse-warning": "off" } } });
    let mut server = started(root.path(), params);
    assert_eq!(
        published_codes(&mut server, &[&broken])[0],
        ["parse-error", "parse-error"],
        "the rule was switched off, and only that rule"
    );
    shut_down(server);
}

/// The rule names published for each of `uris`, sorted, waiting until every one has been seen.
///
/// Sorted, not pinned in order: which end of a broken file rubydex reports first is not something
/// this test may have an opinion about.
fn published_codes(server: &mut Server, uris: &[&str]) -> Vec<Vec<String>> {
    let mut found: Vec<Option<Vec<String>>> = vec![None; uris.len()];
    while found.iter().any(Option::is_none) {
        let params = server.notification("textDocument/publishDiagnostics");
        let Some(index) = uris
            .iter()
            .position(|uri| params["uri"] == serde_json::json!(uri))
        else {
            continue;
        };
        let mut codes: Vec<String> = params["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .map(|item| {
                item["code"]
                    .as_str()
                    .expect("every diagnostic names the rule that raised it")
                    .to_owned()
            })
            .collect();
        codes.sort();
        found[index] = Some(codes);
    }
    found.into_iter().map(Option::unwrap).collect()
}

#[test]
fn selection_and_folding_ranges_over_stdio() {
    // The end-to-end ranges claim. Both are pure functions of one buffer, so a real server adds the
    // wire: a chain arrives as nested `parent` objects instead of a list, a fold as two line
    // numbers with no characters, and both are measured against a buffer the disk has never seen.
    let root = fixture();
    let mut server = started(root.path(), modern_client(root.path()));

    let scratch = uri_of(root.path(), "lib/scratch.rb");
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": scratch,
                "languageId": "ruby",
                "version": 1,
                "text": "# a note\n# and more\nclass Till\n  def sum(total)\n    puts \"got #{total}\"\n  end\nend\n"
            }
        }),
    );

    let id = server.request(
        "textDocument/foldingRange",
        serde_json::json!({ "textDocument": { "uri": scratch } }),
    );
    let found = server.response(&id).response_result.expect("a response");
    assert_eq!(
        found,
        serde_json::json!([
            { "startLine": 0, "endLine": 1, "kind": "comment" },
            { "startLine": 2, "endLine": 5 },
            { "startLine": 3, "endLine": 4 },
        ]),
        "{found}"
    );

    // The cursor inside `total` in the interpolation on line 4.
    let id = server.request(
        "textDocument/selectionRange",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "positions": [{ "line": 4, "character": 18 }]
        }),
    );
    let found = server.response(&id).response_result.expect("a response");
    // Innermost first: the name, then the interpolation, then what is inside the quotes.
    assert_eq!(
        found[0]["range"],
        serde_json::json!({
            "start": { "line": 4, "character": 16 },
            "end": { "line": 4, "character": 21 }
        }),
        "{found}"
    );
    assert_eq!(
        found[0]["parent"]["range"]["start"],
        serde_json::json!({ "line": 4, "character": 14 }),
        "{found}"
    );
    // And the last link is the whole buffer, which makes every position answerable.
    let mut outermost = &found[0];
    while outermost["parent"].is_object() {
        outermost = &outermost["parent"];
    }
    assert_eq!(
        outermost["range"]["start"],
        serde_json::json!({ "line": 0, "character": 0 }),
        "{found}"
    );

    shut_down(server);
}

/// Whether a `publishDiagnostics` payload reports the rule the reload test switches.
fn has_parse_error(published: &serde_json::Value) -> bool {
    published["diagnostics"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["code"] == "parse-error"))
}
