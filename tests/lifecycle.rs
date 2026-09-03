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
    params["capabilities"]["workspace"] =
        serde_json::json!({ "workspaceEdit": { "documentChanges": true } });
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
    assert_eq!(result["capabilities"]["documentHighlightProvider"], true);
    assert_eq!(result["capabilities"]["selectionRangeProvider"], true);
    assert_eq!(result["capabilities"]["foldingRangeProvider"], true);
    assert_eq!(result["capabilities"]["workspaceSymbolProvider"], true);
    // The one capability `lsp-types` has no field for, so it is added on the way to JSON.
    // Asserted here as well as in `capabilities::tests` because the flatten that carries it also
    // carries every provider above, and a flatten that stopped flattening would look like this
    // line passing and the rest of them failing.
    assert_eq!(result["capabilities"]["typeHierarchyProvider"], true);
    // `prepareProvider` is the load-bearing half: it is what lets the server decline a position
    // before the editor has asked the user to type a new name for it.
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
    // Prism's own words, forwarded verbatim: ya-lsp owns the severity and the code and not one
    // word of the text. `parse_errors_read_the_way_prism_wrote_them` pins the whole set; here
    // it is the wire that is being checked, so one sentence is enough — but a sentence, not a
    // length. "Non-empty" was the entire contract until v0.2.0, which is the same gap as an
    // unranked completion list: the mechanism tested, the content not.
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
fn signature_help_over_stdio() {
    // The end-to-end claim of v0.3.0's first item: an editor asking what a half-written call
    // takes gets the method's real parameters, with the one being typed marked — over the wire,
    // from a buffer the disk has never seen, and with the offsets in the encoding the client
    // negotiated rather than in bytes.
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

    // Two arguments in, and the answer follows the cursor rather than the request.
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

    // And outside a call there is nothing to say, which is what closes the popup.
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
    // The end-to-end claim of v0.3.0's second item, and the one that needs a real server to
    // make: the two halves of the answer come from different places — a Prism walk of the
    // buffer for the local, the graph for the method — and an editor cannot tell, because both
    // arrive as ranges in the encoding it negotiated over a buffer the disk has never seen.
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
                // `total` is a local in one method and a different local in the other; `sum` is
                // a method with a call. The comment is the word match this replaces.
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

    // The parameter on line 2 and its use on line 3, and nothing on lines 7 and 8 where the
    // other method spells the same six letters.
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

    // And `null` in the comment, which is what hands the word matching back to the client for
    // the positions ya-lsp cannot speak for.
    assert_eq!(ask(&mut server, 1, 6), serde_json::Value::Null);

    shut_down(server);
}

#[test]
fn type_hierarchy_over_stdio() {
    // Three requests and one round trip, which is why this needs a real server: the item the
    // client expands is the item the server sent, `data` and all, and nothing in-process can
    // check that the field survives serialisation both ways. The capability is here too, because
    // it is the one `lsp-types` has no field for and is added on the way to JSON.
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

    // Expanded upwards with the item exactly as it arrived, which is what an editor sends.
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

    // A method is not a type, and the editor is told so with a `null` rather than an empty tree.
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
    // Through the shipped binary because this is the one request that *writes*: the edit has to
    // survive serialisation and arrive as something an editor can apply, and the version it
    // carries comes from the `didOpen` rather than from anything in-process.
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
    // The version the buffer was opened at, which is what lets the client refuse an edit the
    // user has typed past.
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

    // A method is refused, and the refusal reaches the user as a message rather than only as a
    // `null` the editor turns into its own generic sentence. The message goes out ahead of the
    // response, so it is read first — `response` steps over notifications and would drop it.
    let id = server.request(
        "textDocument/prepareRename",
        serde_json::json!({
            "textDocument": { "uri": scratch },
            "position": { "line": 1, "character": 7 }
        }),
    );
    let mut said = String::new();
    while !said.contains("renaming a method") {
        // Stepping over whatever startup said: this client takes no dynamic registrations, so
        // it has already been told that files cannot be watched.
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

/// The command line, which no editor uses and every packager does.
///
/// `--version` is what a Homebrew formula or a CI step calls to check what it installed;
/// `--help` is what someone types after the binary did nothing they expected. Both write to
/// stdout, which is the LSP transport in every other mode — hence the assertion that the
/// *other* stream stays empty.
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

/// An argument nobody recognises fails loudly, on stderr, with the usage attached.
///
/// Exiting 0 here would let a typo in an editor's configuration look like a server that starts
/// and then says nothing — the single most confusing way for this binary to fail.
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
/// Closing stdin immediately is what an editor that crashed on startup looks like from here.
/// The exit code is the only thing a supervisor can see, so it has to be non-zero.
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

/// The first log line names the version, and it does not wait for the handshake.
///
/// A pasted log is the only thing a bug report reliably carries, and every line in one is
/// worthless without knowing which build wrote it. Asserted on the path where `initialize` never
/// arrives, because that is the case the placement exists for: put this after the handshake and
/// the logs from a client that cannot complete one — the reports hardest to reproduce — carry no
/// version at all.
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

#[test]
fn the_index_follows_files_written_deleted_and_rewritten_on_disk() {
    // v0.3.0's item 0, through the shipped binary. Until this landed the watcher covered
    // `ya-lsp.toml` and nothing else, so a `git checkout`, a `git pull`, a rebase or a
    // `rails g model` changed Ruby under a running server and nothing re-indexed it — a
    // deleted file kept its declarations until someone restarted. Every step here happens with
    // no `didOpen` anywhere, because that is the situation: the editor never touched the file.
    let root = fixture();
    let mut server = Server::start(root.path());
    let mut params = initialize_params(root.path());
    params["capabilities"]["workspace"] =
        serde_json::json!({ "didChangeWatchedFiles": { "dynamicRegistration": true } });
    let id = server.request("initialize", params);
    server.response(&id).response_result.expect("initialize");
    server.notify("initialized", serde_json::json!({}));

    // The registration covers the project's Ruby now, not only its configuration.
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
        vec![spelled("ya-lsp.toml"), spelled("**/*.rb")],
        "the config, and index.include verbatim"
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

    // A file appears, and the file that uses it is rewritten — one `git pull` in miniature.
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

    // And references, which is the half that reads the *other* file the change touched.
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

    // The same file, rewritten: the declarations it used to hold have to go with it.
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

    // And gone: the one change nothing else in the protocol can stand in for. Before item 0 a
    // deleted file kept every declaration it had for the life of the process.
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
    // Item 8's whole claim, through the shipped binary rather than an in-process connection:
    // an editor that takes a dynamic registration is asked to watch `ya-lsp.toml`, and the
    // change it reports back actually changes an answer. Until v0.2.0 nothing in the server ever
    // sent `client/registerCapability`, so this worked in exactly one editor — the one whose
    // extension brought a watcher of its own — and the release notes said otherwise.
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
    // `parse-error` only: Prism also reports the indentation as a `parse-warning`, and this
    // test is about the one rule the file being edited turns on and off.
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
    // Item 6's diagnostics half, through the shipped binary and through the layer the *editor*
    // sends — `ya-lsp.toml` is what `the_config_file_reloads_without_a_restart` drives, and it is
    // the editor's view that item 6 is about. The rule is `parse-warning` because that is the one
    // a project may already be linting for itself: the public Rails app measured for this release
    // switches off exactly its ground (`Lint/UselessAssignment`) in `.standard.yml` while a file
    // that does not parse is still worth a squiggle. One file carries both, so the assertion is
    // that the warning goes and the errors beside it stay.
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

    // The same workspace, started with the one thing the extension sends for
    // `ya-lsp.diagnostics.rules`. `lib/unused.rb` now has nothing left to report and so is never
    // published at all, which is why the assertion is made on the file that still has errors.
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
/// Sorted rather than pinned in order: which end of a broken file rubydex reports first is not
/// something this test is entitled to an opinion about.
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
    // The end-to-end claim of v0.3.0's third item. Both are pure functions of one buffer, so what
    // a real server adds over the unit tests is the wire: a chain arrives as a nest of `parent`
    // objects rather than a list, a fold arrives as two line numbers with no characters on it,
    // and both are measured against a buffer the disk has never seen.
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
    // And the last link is the whole buffer, which is what makes every position answerable.
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
