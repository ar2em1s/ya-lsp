//! End-to-end: spawn the real binary and speak LSP to it over stdio.
//!
//! The in-process tests in `analysis` cover graph behaviour. This covers what they cannot —
//! that the shipped executable frames messages correctly, negotiates capabilities, and exits
//! cleanly — which is exactly the layer where a language server tends to fail silently.

use std::{
    io::{BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::Duration,
};

use lsp_server::{Message, Notification, Request, RequestId, Response};

/// How long any single message may take to arrive. Generous — this only has to beat the
/// 150 ms analysis debounce — but finite, so a server that stops talking fails the test
/// instead of hanging the suite.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// `start`, with extra environment variables — the only way to point the real binary at a
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
    // Ruby's own signatures are off for every fixture that is not about them. They come from
    // whatever rbs the machine has, or from the vendored copy, and neither belongs in a test of
    // something else: 250 files of background work is noise these assertions would have to
    // wait out. `built_in_classes_*` turns them back on and is where they are tested.
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
    params
}

/// A project with one bundled gem installed in a directory of its own, laid out the way
/// RubyGems lays one out. Returns the project and the gem home, which has to stay alive.
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

    // The client offered utf-8, so the server must take it: rubydex speaks byte offsets, and
    // choosing utf-8 makes every column conversion the identity.
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(result["capabilities"]["textDocumentSync"]["change"], 2); // INCREMENTAL
    assert_eq!(
        result["capabilities"]["textDocumentSync"]["openClose"],
        true
    );
    assert_eq!(result["capabilities"]["referencesProvider"], true);
    assert_eq!(result["capabilities"]["workspaceSymbolProvider"], true);
    assert_eq!(
        result["capabilities"]["completionProvider"]["resolveProvider"],
        true
    );
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

    // The server has to have applied that edit to its own copy of the buffer, and the only way
    // to see its copy from out here is to ask for the outline.
    let id = server.request(
        "textDocument/documentSymbol",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );
    let symbols = server
        .response(&id)
        .response_result
        .expect("documentSymbol");
    // This client never advertised `hierarchicalDocumentSymbolSupport`, so the answer must be
    // the flat pre-3.10 shape: `SymbolInformation`, carrying a `location` and a
    // `containerName` rather than nested `children`.
    assert!(
        symbols[0]["location"].is_object(),
        "a client without hierarchical support must get the flat shape: {symbols}"
    );
    assert_eq!(symbols[1]["name"], "murmur", "{symbols}");
    assert_eq!(symbols[1]["containerName"], "Person", "{symbols}");

    // An unimplemented method must still be answered rather than dropped: an unanswered
    // request wedges the client forever.
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

    // A client that omits `general.positionEncodings` predates the capability and must be
    // served UTF-16, which is what the spec mandates.
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
fn a_syntax_error_reaches_the_client_as_a_diagnostic() {
    // The end-to-end claim of M1: a file that does not parse lights up in the editor, with a
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
        // The clean file in the fixture never publishes, but do not depend on that.
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
    assert!(error["message"].as_str().is_some_and(|m| !m.is_empty()));
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
    // The end-to-end claim of M2: an editor that opens a file can ask what a name is, where it
    // came from, and what the file contains — and get answers in the shapes it advertised.
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
    // `linkSupport` was advertised, so the answer carries the origin span and points at the
    // class name rather than at the whole body.
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

    // A position with nothing under it must answer `null`, not an error: an error is something
    // the editor shows the user.
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
    // The end-to-end claim of M4: an editor can ask the project a question that names no file.
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

    // Every use of `Person`, asked for from the class definition, with the definition itself
    // included the way VS Code asks for it.
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

    // A method, which is name-based — `shout` is called once and defined once.
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
    // The end-to-end claim of M5: an editor asking what can be typed at a position gets an
    // answer that came from the project, not from the words in the open buffer.
    let root = fixture();
    std::fs::write(
        root.path().join("lib/office.rb"),
        "module HR\n  MAX_STAFF = 50\n\n  class Person\n    def self.build(name:)\n    end\n\n    \
         def shout\n    end\n  end\nend\n",
    )
    .unwrap();
    let mut server = started(root.path(), modern_client(root.path()));

    // A buffer the editor holds and the disk has never seen, which is the situation completion
    // always runs in.
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
    assert_eq!(found["isIncomplete"], true, "{found}");
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

    // A comment is not a place Ruby can be written, and saying so lets the editor fall back to
    // its own word list instead of showing an empty popup.
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
fn a_client_without_link_support_gets_plain_locations() {
    // LSP lets a server answer `definition` with `LocationLink`s only if the client said it
    // understands them. Sending the richer shape unasked is not a graceful degradation — some
    // clients fail to parse it outright.
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

/// M3: a gem the project depends on becomes navigable, and the editor is told it is happening.
///
/// This is the milestone's acceptance criterion reduced to something CI can run: no Ruby is
/// executed anywhere, the gem is found from `Gemfile.lock` plus a `GEM_HOME`, and
/// goto-definition crosses from the project into it.
#[test]
fn gems_are_indexed_in_the_background_and_become_navigable() {
    let (root, gem_home) = bundled_fixture();
    let mut server = Server::start_with_env(root.path(), &[("GEM_HOME", gem_home.path())]);

    let id = server.request("initialize", modern_client(root.path()));
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // The server opens the progress stream with a request of its own. Answering it is the
    // client's job, and a server that cannot cope with the answer arriving at any moment would
    // wedge here.
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

/// `--licenses` prints what the binary is obliged to carry, and exits cleanly.
///
/// This is the one test that runs the binary as a *command* rather than as a server. It exists
/// because the obligation belongs to the artifact: a bare `ya-lsp` attached to a release or
/// installed with `cargo install` has no licence file beside it, and the BSD-2-Clause material
/// embedded in it has to reach the person holding it somehow.
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

/// Ruby's own core classes: indexed, navigable, and reachable from a literal.
///
/// This is the whole of M7 through the wire. The workspace has no gems and asks for core only —
/// which rung of the ladder answered is `workspace::rbs`'s business, and pinning it here would
/// make the test assert something about the machine rather than about the server.
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

    // A buffer the disk has never seen, which is the situation completion always runs in.
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

    // And this is what makes the line above mean anything. With core in the graph, the
    // name-based fallback would offer `upcase` too — it offers every method name there is. Only
    // a receiver the server actually typed can *refuse* `push`, which is an `Array` method and
    // no `String` has ever had one.
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

    // Core only. `OptionParser` is a stdlib signature and this workspace asked for neither the
    // stdlib nor its gems. Which classes are "core" is rbs's call and it moves — `Set` and
    // `Pathname` are both in `core/` as of rbs 4.x, so neither can be used to test this — which
    // is exactly why `the_stdlib_signatures_are_indexed_when_asked_for` asserts the positive
    // side separately.
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

/// The same server, with the signatures turned off entirely: what shipped before M7.
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
    // degrade to the name-based list — the project's own `Person#shout` — rather than to
    // silence, which is what a bare `None` from `receiver_for` would produce.
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

    // Indexing still has to happen — the gem is navigable either way, the client just does not
    // get told when. Which means the only honest way to wait is to keep asking, exactly as a
    // progress-less editor would as the user works.
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
