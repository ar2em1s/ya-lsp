//! `Analysis::run` on its own thread, driven over the real channel.
//!
//! `mod tests` calls `analysis.handle(task)` directly, on the test's thread. That is right for
//! asking what an answer *is*, but it cannot ask what happens when two things arrive at once, which
//! is most of the run loop:
//! - the debounce timer;
//! - the `receiver.is_empty()` check that makes gem indexing yield to editor traffic;
//! - `Cancellations`;
//! - the order every request is answered in.
//!
//! None of that is reachable synchronously, and all of it is load-bearing.
//!
//! So these tests spawn the thread as `server::serve` does and talk to it as the main loop does:
//! tasks in over a `Sender<Task>`, messages out over a `Receiver<Message>`, cancellations through
//! the shared set. **The subject is order, not content**: which answer arrives before which
//! notification. `Threaded` keeps every message it took off the channel, in send order, and
//! assertions are about positions in that list. A test about the *shape* of an answer belongs in
//! `mod tests`, which is faster and easier to read.

use lsp_server::ErrorCode;
use serde_json::json;

use super::*;

/// How long a wait gives the thread before calling it a hang.
///
/// Far past anything a passing run needs: every wait below is for work of a few milliseconds, so
/// only a stopped thread reaches this. It buys a failed test with a sentence in it, not a CI job
/// that runs until the runner kills it.
const PATIENCE: Duration = Duration::from_secs(30);

/// How many source files the fixture gem holds, and how many lines of body each one carries.
///
/// Sized so the bundle takes many batches: enough round trips fit before it finishes that the
/// test's four have a wide margin. A bundle that finishes first fails the test for the opposite of
/// its reason, so the margin lives in the fixture, not in a sleep.
const GEM_FILES: usize = 1200;
const GEM_FILE_LINES: usize = 800;

/// The buffer the debounce test types into, and how many questions it asks about it.
///
/// Sized so answering all of them takes several times [`RESOLVE_DEBOUNCE`], so the deadline falls
/// partway through the sequence. The margin only runs one way: a slower machine reaches the
/// deadline *earlier*, and the assertion is only that it falls somewhere inside. The first answer
/// always precedes it, on any machine, because the `didOpen` right before arms the timer.
///
/// **Sized against [`RESOLVE_DEBOUNCE`].** Far fewer requests, or a much faster machine, and the
/// lot would be answered before the deadline fell. That fails loudly (the assertion is that
/// diagnostics arrive *among* the answers), but re-read this when the constant moves.
const FOLD_BLOCKS: usize = 12_000;
const FOLD_REQUESTS: usize = 40;

/// How many questions the editor asks while the bundle is going in.
///
/// More than one on purpose. The first request is already queued when the run loop starts, so alone
/// it would only show that the *first* batch yielded. Every later one is asked from inside the
/// index, which is the claim.
const ROUND_TRIPS: usize = 4;

/// How many methods the file for the semantic-token measurement carries.
///
/// Five lines each, so about 10,000 lines: far past a typical Rails file. The question is whether a
/// whole-document request at typing speed can block the loop, and that only gets interesting above
/// the sizes that exist.
const TOKEN_FILE_METHODS: usize = 2_000;

/// The ceiling on a `semanticTokens/full` answer over [`TOKEN_FILE_METHODS`] methods.
///
/// A canary's ceiling, not a benchmark's. It catches an accidental quadratic, a change in *kind*; a
/// shared CI runner with a cold page cache moves the number by a small multiple. The ceiling is
/// roughly twenty times a debug-build answer, since the suite runs in debug.
const TOKEN_CEILING: Duration = Duration::from_millis(500);

/// The ceiling on a `documentLink` answer over the same file [`TOKEN_FILE_METHODS`] builds.
///
/// Its own constant because it is its own measurement, though the argument is [`TOKEN_CEILING`]'s:
/// a second whole-file request on the same serialized loop, which an editor asks for unprompted, on
/// open and after every settled edit. The cost is the Prism parse of the whole buffer, since a file
/// has a handful of requires however long it is. This catches a walk that starts scaling with the
/// body instead of the require count.
///
/// The ceiling is roughly twenty times a debug-build answer: the same multiple, for the same
/// reason.
const LINK_CEILING: Duration = Duration::from_millis(150);

/// The ceiling on a `textDocument/inlayHint` answer over the whole of the same file.
///
/// [`TOKEN_CEILING`]'s argument again, for a request normally asked about a window that can still
/// be asked about a document. The largest of the three because the work is: 2,000 labels means
/// 2,000 receivers classified out of the buffer and looked up in the graph, against one parse for
/// the other two. Same twenty-times multiple, same reason.
///
/// What a *window* costs has no ceiling here. It is dominated by the whole-buffer parse every
/// request pays, so the clock cannot tell a bounded walk from a filtered one on a busy runner.
/// `hints.rs` counts the work instead.
const HINT_CEILING: Duration = Duration::from_millis(1_500);

/// A workspace with gems and Ruby's own signatures off, as `Harness` builds one, for the same
/// reason: hundreds of signature files nothing here asks about.
fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("ya-lsp.toml"),
        "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
    )
    .unwrap();
    root
}

fn write(root: &Path, relative: &str, source: &str) -> DocUri {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, source).unwrap();
    DocUri::from_path(&path).unwrap()
}

/// A URI under the workspace for a file that is not on disk, which is what an unsaved buffer is.
fn buffer_uri(root: &Path, relative: &str) -> DocUri {
    DocUri::from_path(&root.join(relative)).unwrap()
}

/// The analysis thread, spawned, with the two ends the main loop holds.
struct Threaded {
    /// `None` once joined. `Drop` joins whatever is left, so a failing test cannot leave a thread
    /// holding a `TempDir` open.
    analysis: Option<AnalysisHandle>,
    outgoing: Receiver<Message>,
    cancellations: Cancellations,
    /// Every message the thread sent that a wait has already taken off the channel, in send order.
    seen: Vec<Message>,
    root: tempfile::TempDir,
    /// The gem home, for fixtures that have one. Dropping it deletes the gems, so it must outlive
    /// the thread reading them.
    _gem_home: Option<tempfile::TempDir>,
    next_id: i32,
}

impl Threaded {
    fn start(root: tempfile::TempDir) -> Self {
        Self::start_with_env(root, None, gems::Env::default())
    }

    fn start_with_env(
        root: tempfile::TempDir,
        gem_home: Option<tempfile::TempDir>,
        env: gems::Env,
    ) -> Self {
        let (sender, outgoing) = crossbeam_channel::unbounded();
        // See `testing::Harness`: the fixture is a Rails project and says so through the client's
        // layer, not by writing a marker file into the index.
        let options = serde_json::json!({ "rails": { "enabled": true } });
        let (workspace, problems) =
            Workspace::load_with_env(root.path().to_path_buf(), Some(options), env);
        assert!(problems.is_empty(), "{problems:?}");

        let cancellations = Cancellations::default();
        let analysis = super::spawn(
            workspace,
            PositionEncoding::Utf16,
            ClientSupport {
                hierarchical_symbols: true,
                definition_links: true,
                // As in `testing::Harness`: the shape a real client negotiates.
                implementation_links: false,
                type_definition_links: true,
                declaration_links: true,
                show_document: true,
                generated_content: true,
                // On: the gem index's `$/progress` stream is how a test knows when background work
                // started and finished.
                work_done_progress: true,
                versioned_edits: true,
                // On, so the refresh the gem index sends at the end lands in `outgoing`, where a
                // test can see it.
                hint_refresh: true,
            },
            super::testing::document_registrar(),
            sender,
            cancellations.clone(),
            crate::logging::Reload::default(),
        );

        Self {
            analysis: Some(analysis),
            outgoing,
            cancellations,
            seen: Vec::new(),
            root,
            _gem_home: gem_home,
            next_id: 0,
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    /// Queue a task without waiting for anything, which is all the main loop ever does.
    fn send(&self, task: Task) {
        self.analysis
            .as_ref()
            .expect("still running")
            .sender()
            .send(task)
            .expect("the analysis thread is still receiving");
    }

    fn open(&self, uri: &DocUri, text: &str) {
        self.send(Task::DidOpen {
            uri: uri.clone(),
            text: text.to_owned(),
            version: Some(1),
        });
    }

    /// Queue a request and hand back the id it will be answered under.
    fn request(&mut self, method: &str, params: serde_json::Value) -> RequestId {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.send(Task::Request(Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        }));
        id
    }

    /// Wait until the thread has sent something `matches` accepts, and return its position in send
    /// order.
    ///
    /// Messages already taken off the channel are searched first, so a position is stable however
    /// many waits ran before: two calls about the same message agree.
    fn wait_for(&mut self, what: &str, matches: impl Fn(&Message) -> bool) -> usize {
        loop {
            if let Some(at) = self.seen.iter().position(&matches) {
                return at;
            }
            match self.outgoing.recv_timeout(PATIENCE) {
                Ok(message) => self.seen.push(message),
                Err(_) => panic!(
                    "the analysis thread never sent {what}. It sent: {}",
                    self.summary()
                ),
            }
        }
    }

    /// Where the response to `id` sits in the stream.
    fn response_at(&mut self, id: &RequestId) -> usize {
        let what = format!("a response to request {id}");
        self.wait_for(
            &what,
            |message| matches!(message, Message::Response(response) if &response.id == id),
        )
    }

    fn response(&mut self, id: &RequestId) -> Response {
        let at = self.response_at(id);
        match &self.seen[at] {
            Message::Response(response) => response.clone(),
            other => unreachable!("{other:?}"),
        }
    }

    fn ask(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.request(method, params);
        self.response(&id)
            .response_result
            .expect("handlers never error")
    }

    /// Where the first `$/progress` of `kind` sits in the stream.
    fn progress_at(&mut self, kind: &str) -> usize {
        let what = format!("a $/progress of kind {kind}");
        let wanted = kind.to_owned();
        self.wait_for(&what, move |message| match message {
            Message::Notification(notification) => {
                notification.method == "$/progress"
                    && notification.params["value"]["kind"] == wanted.as_str()
            }
            _ => false,
        })
    }

    /// Where the first `publishDiagnostics` for `uri` sits in the stream.
    fn published_at(&mut self, uri: &DocUri) -> usize {
        let what = format!("diagnostics for {uri}");
        let wanted = uri.as_str().to_owned();
        self.wait_for(&what, move |message| match message {
            Message::Notification(notification) => {
                notification.method == "textDocument/publishDiagnostics"
                    && notification.params["uri"] == wanted.as_str()
            }
            _ => false,
        })
    }

    /// The stream, one line per message, for a failure message. Whole `Message`s are pages long,
    /// and a failure here always asks "in what order?".
    fn summary(&self) -> String {
        self.seen
            .iter()
            .enumerate()
            .map(|(at, message)| {
                let line = match message {
                    Message::Request(request) => {
                        format!("request {} {}", request.id, request.method)
                    }
                    Message::Response(response) => format!("response {}", response.id),
                    Message::Notification(notification) => match notification.method.as_str() {
                        "$/progress" => format!(
                            "$/progress {}",
                            notification.params["value"]["kind"].as_str().unwrap_or("?")
                        ),
                        "textDocument/publishDiagnostics" => format!(
                            "publishDiagnostics {}",
                            notification.params["uri"].as_str().unwrap_or("?")
                        ),
                        method => method.to_owned(),
                    },
                };
                format!("\n  {at}: {line}")
            })
            .collect()
    }

    /// Close the queue and wait for the thread, as `server::serve` does on the way out.
    fn join(&mut self) {
        if let Some(analysis) = self.analysis.take() {
            analysis.join();
        }
    }
}

impl Drop for Threaded {
    fn drop(&mut self) {
        // A failed assertion unwinds through here with the thread still running and the `TempDir`
        // about to be deleted. Joining first matters: the thread indexes files, and deleting the
        // tree under it turns one clear failure into a second, unrelated one.
        self.join();
    }
}

/// A file of `TOKEN_FILE_METHODS` methods, each with a local, a parameter and two calls in it.
fn a_large_file() -> String {
    (0..TOKEN_FILE_METHODS)
        .map(|n| format!("def method_{n}(scale)\n  size = scale.abs\n  size.to_s\nend\n\n"))
        .collect()
}

#[test]
fn semantic_tokens_for_a_large_file_and_what_it_costs_the_request_behind_it() {
    // This request ships on a stated precondition: a whole-file answer at typing speed, on a loop
    // that serialises everything. This harness asks what that costs.
    //
    // **What matters is not this request's latency but what it does to the next one.** The loop
    // answers in order, so everything queued behind a whole-file request waits for all of it, and
    // an editor sends `semanticTokens/full` on every edit, while the user types. So a cheap request
    // is queued with it before the thread looks at either. The claim: it waits for the tokens
    // answer, which is bounded, then costs what it always costs.
    let root = project();
    let source = a_large_file();
    let uri = write(root.path(), "app/big.rb", &source);
    let mut server = Threaded::start(root);
    server.open(&uri, &source);

    let document = json!({ "textDocument": { "uri": uri.as_str() } });
    // One round trip first, so the measurement is the request, not the thread reaching the buffer
    // for the first time.
    server.ask("textDocument/foldingRange", document.clone());

    let started = Instant::now();
    let tokens = server.request("textDocument/semanticTokens/full", document.clone());
    let behind = server.request(
        "textDocument/selectionRange",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "positions": [{ "line": 1, "character": 3 }],
        }),
    );

    let answer = server
        .response(&tokens)
        .response_result
        .expect("handlers never error");
    let answered = started.elapsed();
    server.response(&behind);
    let queued = started.elapsed();

    let data = answer["data"].as_array().expect("token data");
    // Five numbers per token, seven tokens per method: the name it is defined under, its parameter,
    // the parameter read, two calls, and two reads of the local.
    assert_eq!(data.len(), TOKEN_FILE_METHODS * 7 * 5, "{}", data.len());
    // The head-of-line bound: what every request queued behind it waits.
    assert!(
        answered < TOKEN_CEILING,
        "{TOKEN_FILE_METHODS} methods took {answered:.2?} to colour"
    );
    // And the request behind it then runs at its own speed, not a degraded one.
    assert!(
        queued - answered < TOKEN_CEILING,
        "the request behind it then took {:.2?} of its own",
        queued - answered
    );
    server.join();
}

#[test]
fn document_links_for_a_large_file_and_what_it_costs_the_request_behind_it() {
    // The second whole-file request on the loop, asked the same way. An editor sends `documentLink`
    // unprompted (on open, and after each settled edit), so what matters is again what the request
    // queued behind it waits for.
    //
    // The file is the token measurement's with two requires in front. A real file has a handful of
    // requires however long it is, so the cost is parsing everything after them. A walk that
    // started scaling with the body instead of the require count would show up here and nowhere
    // else.
    let root = project();
    let library = write(root.path(), "lib/person.rb", "class Person\nend\n");
    let source = format!("require \"person\"\nrequire \"nope\"\n{}", a_large_file());
    let uri = write(root.path(), "app/big.rb", &source);
    let mut server = Threaded::start(root);
    server.open(&uri, &source);

    let document = json!({ "textDocument": { "uri": uri.as_str() } });
    // One round trip first, so the measurement is the request, not the thread reaching the buffer
    // for the first time.
    server.ask("textDocument/foldingRange", document.clone());

    let started = Instant::now();
    let links = server.request("textDocument/documentLink", document);
    let behind = server.request(
        "textDocument/selectionRange",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "positions": [{ "line": 3, "character": 3 }],
        }),
    );

    let answer = server
        .response(&links)
        .response_result
        .expect("handlers never error");
    let answered = started.elapsed();
    server.response(&behind);
    let queued = started.elapsed();

    // One link, not two: `person` is a file this workspace has and `nope` is not. A path the graph
    // cannot place is left plain, not underlined and dead.
    assert_eq!(
        answer,
        json!([{
            "range": {
                "start": { "line": 0, "character": 9 },
                "end": { "line": 0, "character": 15 },
            },
            "target": library.as_str(),
        }])
    );
    // The head-of-line bound: what every request queued behind it waits.
    assert!(
        answered < LINK_CEILING,
        "{TOKEN_FILE_METHODS} methods took {answered:.2?} to link"
    );
    // And the request behind it then runs at its own speed, not a degraded one.
    assert!(
        queued - answered < LINK_CEILING,
        "the request behind it then took {:.2?} of its own",
        queued - answered
    );
    server.join();
}

/// A workspace like [`project`] but with enough RBS to type something.
///
/// The hint measurement needs real answers: a walk that finds nothing measures the walk, not the
/// work. One class of two methods is all the file below asks about.
fn project_with_a_signature() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    let signatures = root.path().join("sig");
    std::fs::create_dir_all(signatures.join("core")).unwrap();
    std::fs::write(
        signatures.join("core/core.rbs"),
        "class String\n  def upcase: () -> String\n  def length: () -> Integer\nend\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("ya-lsp.toml"),
        format!(
            "[gems]\ndefault_gems = false\n\n[rbs]\npath = {:?}\n",
            signatures.display().to_string()
        ),
    )
    .unwrap();
    root
}

/// The same file, written so that every method carries exactly one hint.
fn a_large_hinted_file() -> String {
    (0..TOKEN_FILE_METHODS)
        .map(|n| format!("def method_{n}(scale)\n  size = \"x\".upcase\n  size.length\nend\n\n"))
        .collect()
}

#[test]
fn inlay_hints_for_a_large_file_and_what_it_costs_the_request_behind_it() {
    // The third whole-file request on the loop, asked the same way. An editor normally asks about
    // its visible window (`textDocument/inlayHint` carries a range), but a client may ask for the
    // whole document, and a short file *is* the whole document. So what matters is again what the
    // request queued behind it waits for.
    //
    // Every method carries exactly one hint: the worst shape, not a typical one. 2,000 labels is
    // more than any real file, and each is a receiver classified out of the buffer and looked up in
    // the graph.
    //
    // **What the range buys is deliberately not measured here.** That it bounds the *work* (a
    // binding outside the window is never classified) is a claim about work, not order, and this
    // harness sees work only as clock time. Over one screen, the clock is mostly the whole-buffer
    // parse, so under load a ratio between window and file collapses for reasons unrelated to
    // hints. It is counted instead, beside the module that makes the claim:
    // `hints::tests::hints_answer_for_the_range_they_were_asked_about`.
    let root = project_with_a_signature();
    let source = a_large_hinted_file();
    let uri = write(root.path(), "app/big.rb", &source);
    let mut server = Threaded::start(root);
    server.open(&uri, &source);

    let whole = json!({
        "textDocument": { "uri": uri.as_str() },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": TOKEN_FILE_METHODS * 5, "character": 0 },
        },
    });
    // One round trip first, so the measurement is the request, not the thread reaching the buffer
    // for the first time.
    server.ask(
        "textDocument/foldingRange",
        json!({ "textDocument": { "uri": uri.as_str() } }),
    );

    let started = Instant::now();
    let hints = server.request("textDocument/inlayHint", whole);
    let behind = server.request(
        "textDocument/selectionRange",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "positions": [{ "line": 1, "character": 3 }],
        }),
    );

    let answer = server
        .response(&hints)
        .response_result
        .expect("handlers never error");
    let answered = started.elapsed();
    server.response(&behind);
    let queued = started.elapsed();

    assert_eq!(answer.as_array().expect("hints").len(), TOKEN_FILE_METHODS);
    // The head-of-line bound: what every request queued behind it waits.
    assert!(
        answered < HINT_CEILING,
        "{TOKEN_FILE_METHODS} hints took {answered:.2?}"
    );
    // And the request behind it then runs at its own speed, not a degraded one.
    assert!(
        queued - answered < HINT_CEILING,
        "the request behind it then took {:.2?} of its own",
        queued - answered
    );
    server.join();
}

#[test]
fn the_margin_is_redrawn_when_the_background_index_finishes() {
    // The one answer in the crate that goes stale without the document changing. An editor re-asks
    // for inlay hints when the buffer changes or the window scrolls, and neither happens while
    // someone waits for a cold index. A file opened before the signatures arrived would keep its
    // empty margin until touched.
    //
    // `workspace/inlayHint/refresh` is the protocol's fix, and it is a *request*: it goes out like
    // `client/registerCapability`, and the main loop reads and drops the reply. Sent only when the
    // client says it supports one; otherwise the hints are right from the next keystroke.
    let (root, gem_home, env) = project_with_a_gem();
    let uri = write(root.path(), "app/main.rb", "class Person\nend\n");
    let mut server = Threaded::start_with_env(root, Some(gem_home), env);

    server.progress_at("begin");
    server.open(&uri, "class Person\nend\n");

    let finished = server.progress_at("end");
    let refreshed = server.wait_for("an inlay hint refresh", |message| {
        matches!(message, Message::Request(request) if request.method == "workspace/inlayHint/refresh")
    });
    assert!(
        finished < refreshed,
        "the margin was redrawn before the index finished: {}",
        server.summary()
    );
    server.join();
}

#[test]
fn a_request_queued_behind_an_edit_is_answered_against_the_edit() {
    // A `didChange` records its edit and indexes nothing, so a request that reads only the buffer
    // does not queue behind an index it never reads. This is the half that makes that safe, asked
    // over the real loop: `settle` is the one thing that drains `pending_index`, and a request that
    // reaches the graph forces it. The request below is sent with no gap, so it arrives while the
    // debounce is still running and must still be answered against the edit.
    //
    // **There is deliberately no latency assertion.** Deferred indexing saves a lot on a real
    // workspace and little on a fixture: indexing one document costs in proportion to how much of
    // the graph names it, not to the file. A ceiling here would look like a guard and hold nothing.
    let root = project();
    let source = "class Person\nend\n";
    let uri = write(root.path(), "lib/person.rb", source);
    let mut server = Threaded::start(root);
    server.open(&uri, source);

    server.send(Task::DidChange {
        uri: uri.clone(),
        changes: vec![TextChange {
            range: None,
            text: "class Person\n  def zzz_typed\n  end\nend\n".to_owned(),
        }],
        version: Some(2),
    });

    let found = server.ask("workspace/symbol", json!({ "query": "zzz_typed" }));

    assert_eq!(found[0]["name"], "zzz_typed", "{found}");
    server.join();
}

#[test]
fn the_thread_answers_a_request_that_arrived_over_the_channel() {
    // The harness, proven: a task goes in one end and the answer comes out the other, with the
    // startup index done on the thread, not in the test.
    let root = project();
    write(
        root.path(),
        "lib/person.rb",
        "class Person\n  def shout\n  end\nend\n",
    );
    let mut server = Threaded::start(root);

    let found = server.ask("workspace/symbol", json!({ "query": "Person" }));

    assert_eq!(found[0]["name"], "Person", "{found}");
    server.join();
}

#[test]
fn a_request_queued_behind_a_configuration_change_is_answered_against_the_new_index() {
    // Both tasks are queued before the thread looks at either, as a client does: VS Code sends
    // `didChangeConfiguration` and the next keystroke without waiting. A reload throws the whole
    // graph away and builds another, so a request that overtook it would be answered from an index
    // that no longer exists.
    let root = project();
    write(root.path(), "lib/person.rb", "class Person\nend\n");
    write(root.path(), "generated/legacy.rb", "class Legacy\nend\n");
    let mut server = Threaded::start(root);

    assert_eq!(
        server.ask("workspace/symbol", json!({ "query": "Legacy" }))[0]["name"],
        "Legacy",
        "the generated file is in the index to begin with"
    );

    server.send(Task::ChangeConfig {
        options: Some(json!({ "index": { "exclude": ["generated/**/*"] } })),
    });
    let found = server.ask("workspace/symbol", json!({ "query": "Legacy" }));

    assert!(
        found.as_array().is_none_or(|symbols| symbols.is_empty()),
        "the answer came from the graph the reload replaced: {found}"
    );
    assert_eq!(
        server.ask("workspace/symbol", json!({ "query": "Person" }))[0]["name"],
        "Person",
        "and the rest of the workspace is still indexed"
    );
    server.join();
}

#[test]
fn a_cancellation_that_lands_while_the_thread_is_busy_is_honoured() {
    // Why `Cancellations` is shared state and not a `Task`. Tasks are answered in order, so a
    // `$/cancelRequest` sent down the queue would always arrive *after* its request was answered:
    // when cancelling is worth nothing.
    //
    // The thread is kept busy by a buffer that takes tens of milliseconds to index, against
    // microseconds for the two sends and the mutex insert below. That margin makes this a test, not
    // a coin toss.
    let root = project();
    let mut server = Threaded::start(root);

    let uri = buffer_uri(server.path(), "app/big.rb");
    let big: String = (0..5_000)
        .map(|n| format!("class Big{n}\n  def call{n}\n  end\nend\n"))
        .collect();
    server.open(&uri, &big);

    let id = server.request("workspace/symbol", json!({ "query": "Big" }));
    server.cancellations.cancel(id.clone());

    let error = server
        .response(&id)
        .response_result
        .expect_err("a cancelled request is answered with an error, not with null");
    assert_eq!(error.code, ErrorCode::RequestCanceled as i32, "{error:?}");
    server.join();
}

#[test]
fn the_editor_is_answered_against_a_finished_pipeline() {
    // **A question that reads the graph is answered once the pipeline is finished, and the first
    // question is what finishes it.** A settle during the bundle would *link* the graph, handing
    // the first generator pass a resolved graph, which `Analysis::regenerate` says it must never
    // get. So `settle` does not fork at `Stage::Bundle`.
    //
    // Still asserted:
    // - the keystroke is indexed there and then, not queued behind the bundle (`Persona` is in no
    //   file on disk);
    // - every round answers it;
    // - the fifth answer is the first answer.
    //
    // `workspace/symbol` is not one of the three `defers` names, so it settles. A caret asking
    // `completion`, `hover` or `definition` still answers over the map without settling, which is
    // where a cold start is felt.
    let (root, gem_home, env) = project_with_a_gem();
    let uri = write(root.path(), "app/main.rb", "class Person\nend\n");
    let mut server = Threaded::start_with_env(root, Some(gem_home), env);

    server.progress_at("begin");
    // A keystroke, landing between batches. Asking for the name it introduced proves the edit was
    // indexed there and then, not queued behind the bundle: `Persona` is in no file on disk.
    server.open(&uri, "class Persona\nend\n");

    let mut answered = 0;
    for round in 0..ROUND_TRIPS {
        let id = server.request("workspace/symbol", json!({ "query": "Persona" }));
        answered = server.response_at(&id);
        let found = server
            .response(&id)
            .response_result
            .expect("handlers never error");
        assert_eq!(found[0]["name"], "Persona", "round {round}: {found}");
    }

    let finished = server.progress_at("end");
    assert!(
        answered > finished,
        "a settling request has to finish the pipeline before it answers: {}",
        server.summary()
    );
    server.join();
}

#[test]
fn push_diagnostics_for_an_edit_wait_for_the_background_index() {
    // A trade, not an accident: an executable note for whoever changes the priority next.
    //
    // `step_gem_indexing` returning true `continue`s, so while there is background work and an
    // empty queue the loop never reaches `resolve_at`. An edit during a cold start is *indexed*
    // immediately (the test above), but the resolve its debounce armed waits for the bundle, and so
    // does the squiggle for what was typed.
    //
    // Nothing else waits: the next request resolves what is there, and an editor asks something
    // after nearly every keystroke. The delay is bounded by the background index, a fraction of a
    // second for a real bundle in a release build. Settling an overdue resolve before each batch
    // would cost one resolve per debounce of typing; decide that against a measurement on a real
    // bundle, not inside a test.
    let (root, gem_home, env) = project_with_a_gem();
    let uri = write(root.path(), "app/main.rb", "class Person\nend\n");
    let mut server = Threaded::start_with_env(root, Some(gem_home), env);

    server.progress_at("begin");
    server.open(&uri, "class Person\n  def broken(\nend\n");

    let finished = server.progress_at("end");
    let published = server.published_at(&uri);
    assert!(
        finished < published,
        "the debounce now outranks a gem batch, which is a latency change worth measuring: {}",
        server.summary()
    );

    let diagnostics = match &server.seen[published] {
        Message::Notification(notification) => notification.params["diagnostics"].clone(),
        other => unreachable!("{other:?}"),
    };
    assert_eq!(diagnostics[0]["code"], "parse-error", "{diagnostics}");
    server.join();
}

#[test]
fn a_rename_queued_behind_an_edit_is_computed_against_the_edited_buffer() {
    // The one way this crate can damage a file. A rename reads spans out of a buffer and emits
    // edits against them; if the client sent an edit first and the rename did not see it, the
    // rename would write over the wrong bytes. The queue's order (one thread, one channel, no
    // overtaking) prevents that, and only this test can say so: a synchronous harness calls the two
    // in the order the test wrote them, so it cannot tell an ordered queue from an unordered one.
    let root = project();
    let mut server = Threaded::start(root);

    let uri = buffer_uri(server.path(), "app/person.rb");
    server.open(&uri, "class Person\nend\nPerson.new\n");
    server.send(Task::DidChange {
        uri: uri.clone(),
        changes: vec![TextChange {
            range: None,
            text: "# frozen_string_literal: true\nclass Person\nend\nPerson.new\n".to_owned(),
        }],
        version: Some(2),
    });

    let edit = server.ask(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 6 },
            "newName": "Human",
        }),
    );

    let document = &edit["documentChanges"][0];
    assert_eq!(
        document["textDocument"]["version"], 2,
        "the answer is stamped with the version it was computed against: {edit}"
    );
    let lines: Vec<u64> = document["edits"]
        .as_array()
        .expect("edits")
        .iter()
        .map(|edit| edit["range"]["start"]["line"].as_u64().unwrap_or_default())
        .collect();
    assert_eq!(
        lines,
        vec![1, 3],
        "the rename saw the buffer before the edit moved everything down a line: {edit}"
    );
    server.join();
}

#[test]
fn a_debounce_whose_deadline_passed_while_the_thread_was_busy_settles_at_once() {
    // The debounce is a deadline, not a quiet period, and this arm makes the difference.
    // `recv_timeout` only waits when the loop reaches it with time left on the clock; otherwise the
    // resolve is overdue and runs now.
    //
    // In the wild, that happens when a client keeps the thread busy past the debounce. Here it is
    // `foldingRange`: one of the two requests exempt from settling because its answer does not come
    // from the graph, so it can be asked repeatedly without resetting or discharging the timer.
    // Without the arm, the loop would compute a wait from a deadline already past. With it,
    // diagnostics for what was typed arrive *while questions are still being answered*, which is
    // what the ordering below says.
    let root = project();
    let mut server = Threaded::start(root);

    // Not on disk: an unsaved buffer, so nothing was published for this URI at startup and the
    // settle's publish is unambiguous.
    let uri = buffer_uri(server.path(), "app/typing.rb");
    let mut source: String = (0..FOLD_BLOCKS)
        .map(|n| format!("def m{n}\n  puts 1\nend\n"))
        .collect();
    // The half-typed line that gives the settle something to publish.
    source.push_str("def broken(\n");
    server.open(&uri, &source);

    let asked: Vec<RequestId> = (0..FOLD_REQUESTS)
        .map(|_| {
            server.request(
                "textDocument/foldingRange",
                json!({ "textDocument": { "uri": uri.as_str() } }),
            )
        })
        .collect();

    let published = server.published_at(&uri);
    let first = server.response_at(&asked[0]);
    let last = server.response_at(asked.last().expect("FOLD_REQUESTS is not zero"));
    assert!(
        first < published && published < last,
        "the resolve waited for the client to stop asking: {}",
        server.summary()
    );

    let diagnostics = match &server.seen[published] {
        Message::Notification(notification) => notification.params["diagnostics"].clone(),
        other => unreachable!("{other:?}"),
    };
    assert_eq!(
        diagnostics[0]["code"], "parse-error",
        "and what it published is what was typed: {diagnostics}"
    );
    server.join();
}

#[test]
fn a_thread_that_died_is_said_out_loud_when_it_is_joined() {
    // The arm `join` exists for. A dead analysis thread is this server's worst failure: the editor
    // keeps sending, nothing answers, and it looks exactly like thinking. The one line saying it
    // happened is the whole diagnosis.
    let root = project();
    let mut server = Threaded::start(root);
    server.ask("workspace/symbol", json!({ "query": "anything" }));

    server.send(Task::Panic);
    let (_, logged) = crate::testing::captured_logs(tracing::Level::ERROR, || server.join());

    assert!(logged.contains("analysis thread panicked"), "{logged}");
}

/// A project with one installed gem big enough that indexing it takes several batches.
///
/// The bodies are `puts 1`, not more declarations: the test needs *time* in `step_gem_indexing`,
/// and a graph with a hundred thousand declarations would spend that time in the resolve every
/// round trip pays.
///
/// The gem home sits outside the project, where a version manager puts it.
fn project_with_a_gem() -> (tempfile::TempDir, tempfile::TempDir, gems::Env) {
    let body: String = std::iter::repeat_n("puts 1\n", GEM_FILE_LINES).collect();
    let root = project();
    let gem_home = tempfile::tempdir().expect("tempdir");

    std::fs::write(
        root.path().join("Gemfile.lock"),
        "GEM\n  remote: https://rubygems.org/\n  specs:\n    shouty (1.2.3)\n",
    )
    .unwrap();

    let lib = gem_home.path().join("gems/shouty-1.2.3/lib");
    std::fs::create_dir_all(&lib).unwrap();
    for file in 0..GEM_FILES {
        let source = format!("module Shouty{file}\n  class Loud\n  end\nend\n{body}");
        std::fs::write(lib.join(format!("shouty{file}.rb")), source).unwrap();
    }
    std::fs::create_dir_all(gem_home.path().join("specifications")).unwrap();
    std::fs::write(
        gem_home.path().join("specifications/shouty-1.2.3.gemspec"),
        "Gem::Specification.new do |s|\n  s.require_paths = [\"lib\".freeze]\nend\n",
    )
    .unwrap();

    let env = gems::Env {
        gem_home: Some(gem_home.path().to_path_buf()),
        ..gems::Env::default()
    };
    (root, gem_home, env)
}
