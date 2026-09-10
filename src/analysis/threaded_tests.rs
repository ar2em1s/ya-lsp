//! `Analysis::run` on its own thread, driven over the real channel.
//!
//! Everything in `mod tests` calls `analysis.handle(task)` straight, on the test's own thread.
//! That is the right shape for asking what an answer *is*, and it cannot ask anything about
//! what happens when two things arrive at once — which is most of what the run loop is: the
//! debounce timer, the `receiver.is_empty()` check that makes gem indexing yield to editor
//! traffic, `Cancellations`, and the ordering every request is answered under. None of it is
//! reachable from a synchronous harness, and all of it is load-bearing.
//!
//! So these tests spawn the thread the way `server::serve` does and talk to it the way the main
//! loop does: tasks in over a `Sender<Task>`, messages out over a `Receiver<Message>`,
//! cancellations through the shared set rather than the queue. **The subject is order, not
//! content** — which answer arrives before which notification — so `Threaded` keeps every
//! message it has taken off the channel, in the order the thread sent them, and the assertions
//! are about positions in that list. A test whose only claim is the *shape* of an answer belongs
//! in `mod tests` instead, which is faster to run and easier to read.

use serde_json::json;

use super::*;

/// How long a wait here gives the thread before it is called a hang.
///
/// Deliberately far past anything a passing run needs: every wait below is for work that takes
/// single-digit milliseconds, so a number in this range is only ever reached by a thread that
/// has stopped. What it buys is a failed test with a sentence in it rather than a CI job that
/// runs until the runner kills it.
const PATIENCE: Duration = Duration::from_secs(30);

/// How many source files the fixture gem holds, and how many lines of body each one carries.
///
/// Measured rather than guessed: 1,200 files is twelve batches, and on the machine this was
/// written on twelve round trips fit inside them before the bundle finishes. The test asks
/// four. Sizing it the other way — a bundle that finishes first — fails the test for the
/// opposite of the reason it exists, so the margin is in the fixture rather than in a sleep.
const GEM_FILES: usize = 1200;
const GEM_FILE_LINES: usize = 800;

/// The buffer the debounce test types into, and how many questions it asks about it.
///
/// Both are sized so that answering all of them takes several times [`RESOLVE_DEBOUNCE`] —
/// ~1.5 s against 500, three deep, with the deadline falling around the sixteenth answer. The
/// margin only ever runs one way: a slower machine reaches the deadline *earlier* in the
/// sequence, and the assertion is only that it is reached somewhere inside it. The first answer
/// always precedes it, whatever the machine, because the timer is armed by the `didOpen`
/// immediately before them.
///
/// **Sized against [`RESOLVE_DEBOUNCE`].** Half as many requests would fit under two deadlines
/// rather than six, and a machine three times quicker would answer the lot before the deadline
/// fell at all. That fails loudly rather than passing vacuously — the assertion is that
/// diagnostics arrive *among* the answers — but a test depending on a constant has to be re-read
/// when the constant moves.
const FOLD_BLOCKS: usize = 12_000;
const FOLD_REQUESTS: usize = 40;

/// How many questions the editor asks while the bundle is going in.
///
/// More than one on purpose. The first request is already queued when the run loop starts, so
/// on its own it would only show that the *first* batch yielded; every one after it is asked
/// from inside the index, which is the claim. Four against the twelve that fit.
const ROUND_TRIPS: usize = 4;

/// How many methods the file the semantic-token measurement runs against carries.
///
/// Five lines each, so ~10,000 lines — comfortably past the largest file in the Rails
/// application the canary opens, which is 1,116. A measurement taken on a file nobody has is
/// still the right one here: the question is whether a whole-document request at typing speed
/// can block the loop, and the answer only becomes interesting above the sizes that exist.
const TOKEN_FILE_METHODS: usize = 2_000;

/// The ceiling on a `semanticTokens/full` answer over [`TOKEN_FILE_METHODS`] methods.
///
/// A canary's ceiling rather than a benchmark's: what it exists to catch is an accidental
/// quadratic — a change in *kind* — while a shared CI runner with a cold page cache moves the
/// number by a small multiple. Over ~10,000 lines the answer takes about 25 ms in a debug build
/// and 5 ms in a release one, and a request queued behind it a few ms more. The ceiling is twenty
/// times the debug number, because that is the build the suite runs in.
const TOKEN_CEILING: Duration = Duration::from_millis(500);

/// A workspace with gems and Ruby's own signatures off, as `Harness` builds one and for the
/// same reason: ~800 files of signature work nothing here is asking about.
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
    /// `None` once it has been joined; `Drop` joins whatever is left so a failing test cannot
    /// leave a thread behind holding a `TempDir` open.
    analysis: Option<AnalysisHandle>,
    outgoing: Receiver<Message>,
    cancellations: Cancellations,
    /// Every message the thread has sent that a wait has already taken off the channel, in the
    /// order it sent them.
    seen: Vec<Message>,
    root: tempfile::TempDir,
    /// The gem home, for the fixtures that have one: dropping it deletes the gems, so it has to
    /// outlive the thread reading them.
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
        let (workspace, problems) = Workspace::load_with_env(root.path().to_path_buf(), None, env);
        assert!(problems.is_empty(), "{problems:?}");

        let cancellations = Cancellations::default();
        let analysis = super::spawn(
            workspace,
            PositionEncoding::Utf16,
            ClientSupport {
                hierarchical_symbols: true,
                definition_links: true,
                // On, because the gem index's `$/progress` stream is how a test here knows the
                // background work has started and when it finished.
                work_done_progress: true,
                versioned_edits: true,
            },
            sender,
            cancellations.clone(),
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

    /// Wait until the thread has sent something `matches` accepts, and hand back where it sits
    /// in the order the thread sent them.
    ///
    /// Messages already taken off the channel are searched first, so a position is stable
    /// however many waits ran before it: two calls asking about the same message agree.
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

    /// The stream, one line each, for a failure message. Whole `Message`s are pages long and
    /// the question a failure here asks is always "in what order".
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
        // A test that failed an assertion is unwinding through here with the thread still
        // running and the `TempDir` about to be deleted underneath it. Joining first is not
        // tidiness: the thread indexes files, and deleting the tree while it does turns one
        // legible assertion failure into a second, unrelated one.
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
    // This request ships on a stated precondition — it is a whole-file answer at
    // typing speed on a loop that serialises everything, and there was no harness that could
    // ask what that costs. This is that harness, and this is that question.
    //
    // **The number that matters is not this request's latency but what it does to the next
    // one.** The loop answers in order, so everything queued behind a whole-file request waits
    // for the whole of it — and an editor sends `semanticTokens/full` on every edit, while the
    // user is still typing. So a cheap request goes into the queue with it, before the thread
    // has looked at either, and the claim is that it is not made slow: it waits for the tokens
    // answer, which is bounded, and then costs what it always costs.
    let root = project();
    let source = a_large_file();
    let uri = write(root.path(), "app/big.rb", &source);
    let mut server = Threaded::start(root);
    server.open(&uri, &source);

    let document = json!({ "textDocument": { "uri": uri.as_str() } });
    // One round trip first, so what is measured is the request rather than the thread getting
    // to the buffer for the first time.
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
    // Five numbers per token, and seven tokens per method: the name it is defined under, its
    // parameter, the parameter read, two calls and two reads of the local.
    assert_eq!(data.len(), TOKEN_FILE_METHODS * 7 * 5, "{}", data.len());
    // The head-of-line bound: this is what every request queued behind it waits.
    assert!(
        answered < TOKEN_CEILING,
        "{TOKEN_FILE_METHODS} methods took {answered:.2?} to colour"
    );
    // And the request behind it then runs at its own speed rather than at a degraded one.
    assert!(
        queued - answered < TOKEN_CEILING,
        "the request behind it then took {:.2?} of its own",
        queued - answered
    );
    server.join();
}

#[test]
fn a_request_queued_behind_an_edit_is_answered_against_the_edit() {
    // A `didChange` records its edit and indexes nothing, so that a request which reads only
    // the buffer is not queued behind an index it never reads. This is the half that
    // makes that safe, and it is asked over the real loop: `settle` is the one thing that
    // drains `pending_index`, and a request that reaches the graph is what forces it. The
    // request below is sent with no gap at all, so it arrives while the debounce is still
    // running, and must be answered against the edit anyway.
    //
    // **There is deliberately no latency assertion here.** The saving deferred indexing buys is
    // large on a real workspace and small on a fixture, because the cost of indexing one
    // document is a function of how much of the graph names it rather than of the file. A
    // ceiling here would look like a guard and hold nothing.
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
    // startup index having happened on the thread rather than in the test.
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
    // Both tasks are in the queue before the thread has looked at either, which is what the
    // client does: VS Code sends `didChangeConfiguration` and the keystroke that follows it
    // without waiting. A reload throws the whole graph away and builds another one, so a
    // request that overtook it would be answered from an index that no longer exists.
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
    // `$/cancelRequest` sent down the same queue would always arrive *after* the request it
    // cancels had been answered — the one case where cancelling is worth nothing at all.
    //
    // The thread is made busy by a buffer big enough to take tens of milliseconds to index,
    // against the microseconds the two sends and the mutex insert below cost. The margin is
    // what makes this a test rather than a coin toss.
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
fn the_editor_is_answered_while_the_bundle_is_still_going_in() {
    // `receiver.is_empty()` at the top of the run loop is the whole of "background". Without
    // it — with `step_gem_indexing` looping until it runs out — every question asked during a
    // cold start waits for the last gem, which on a real Rails bundle is seconds of a server
    // that looks broken rather than busy.
    //
    // The ordering assertion is the user's experience stated exactly: a keystroke and four
    // answers that know about it, every one of them before the bundle finished.
    let (root, gem_home, env) = project_with_a_gem();
    let uri = write(root.path(), "app/main.rb", "class Person\nend\n");
    let mut server = Threaded::start_with_env(root, Some(gem_home), env);

    server.progress_at("begin");
    // A keystroke, landing between batches. Asking for the name it introduced is what says the
    // edit was indexed there and then rather than queued behind the bundle: `Persona` is in no
    // file on disk.
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
        answered < finished,
        "the bundle finished before the editor got a word in: {}",
        server.summary()
    );
    server.join();
}

#[test]
fn push_diagnostics_for_an_edit_wait_for_the_background_index() {
    // A trade rather than an accident — an executable note for whoever changes the priority
    // next.
    //
    // `step_gem_indexing` returning true `continue`s, so while there is background work and an
    // empty queue the loop never reaches `resolve_at` at all. An edit made during a cold start
    // is *indexed* immediately — the test above is that — but the resolve its debounce armed
    // does not run until the bundle is in, so the squiggle for what was typed waits with it.
    //
    // Nothing else waits: the next request resolves what is there, and an editor asks something
    // after nearly every keystroke. The delay is bounded by the background index, a fraction of
    // a second for a real bundle in a release build. Settling an overdue resolve before the next
    // batch instead would cost one resolve per debounce of typing — a decision to take against a
    // measurement on a real bundle, not inside a test.
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
    // The one way this crate can damage a file: a rename reads spans out of a buffer and emits
    // edits against them, and an edit the client sent first that the rename did not see would
    // write over the wrong bytes. What makes that impossible is the queue's order — one thread,
    // one channel, no overtaking — and this is the test that says so, because nothing else
    // could: a synchronous harness calls the two in the order the test wrote them, so it cannot
    // tell an ordered queue from an unordered one.
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
    // The debounce is a deadline, not a quiet period, and this is the arm that makes the
    // difference: `recv_timeout` only waits when the loop reaches it with time still on the
    // clock, and when it does not, the resolve is overdue and runs now.
    //
    // What produces that in the wild is a client that keeps the thread busy past the debounce —
    // here
    // with `foldingRange`, which is one of the two requests deliberately exempt from settling
    // because its answer does not come from the graph, and so is the one shape of question that
    // can be asked repeatedly without either resetting the timer or discharging it. Without the
    // arm the loop would compute a wait from a deadline already behind it; with it, the
    // diagnostics for what was typed arrive *while the questions are still being answered*,
    // which is what the ordering below says.
    let root = project();
    let mut server = Threaded::start(root);

    // Not on disk: an unsaved buffer, so nothing was published for this URI at startup and the
    // publish the settle produces is unambiguous.
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
    // The arm `join` exists for. A dead analysis thread is the worst failure this server has —
    // the editor keeps sending and nothing ever answers, which looks exactly like a server that
    // is thinking — so the one line that says it happened is the whole of the diagnosis.
    let root = project();
    let mut server = Threaded::start(root);
    server.ask("workspace/symbol", json!({ "query": "anything" }));

    server.send(Task::Panic);
    let (_, logged) = crate::testing::captured_logs(tracing::Level::ERROR, || server.join());

    assert!(logged.contains("analysis thread panicked"), "{logged}");
}

/// A project with one installed gem big enough that indexing it takes several batches.
///
/// The bodies are `puts 1` rather than more declarations: what the test needs is *time* in
/// `step_gem_indexing`, and a graph with a hundred thousand declarations in it would spend that
/// time in the resolve every round trip pays for instead.
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
