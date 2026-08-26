//! LSP lifecycle, capability negotiation, and the message dispatch loop.
//!
//! The main thread only reads from the connection and routes. Everything that touches the
//! graph happens on the analysis thread, which holds a clone of the connection's sender and
//! writes its own responses. That keeps the main thread free to answer `$/cancelRequest` and
//! `shutdown` promptly even while analysis is busy.

pub mod capabilities;

use std::path::Path;

use anyhow::Context as _;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams,
};

use crate::{
    analysis::{self, Cancellations, ClientSupport, Task, TextChange, position::PositionEncoding},
    workspace::{DocUri, Workspace, config::CONFIG_FILE_NAME, uri::workspace_root},
};

/// Run the server over stdio until the client shuts it down.
///
/// # Errors
///
/// Returns an error if the LSP handshake fails or the transport breaks.
pub fn run_stdio() -> anyhow::Result<()> {
    // Before the handshake, not after it: a client that sends a malformed `initialize` — or none
    // at all — is exactly when one needs to know which build was answering, and by then `serve`
    // has already returned. Spelled as `--version` spells it, so one pattern finds both.
    tracing::info!("ya-lsp {} starting", env!("CARGO_PKG_VERSION"));

    let (connection, io_threads) = Connection::stdio();

    // `serve` takes the connection by value on purpose. lsp-server's writer thread only stops
    // once every clone of `Connection::sender` is dropped, so holding one here would make the
    // join below block forever after a clean shutdown.
    let result = serve(connection);

    // Join the transport threads even on failure, or the process leaks them on exit.
    let joined = io_threads.join();
    result?;
    joined.context("LSP transport thread failed")?;
    Ok(())
}

fn serve(connection: Connection) -> anyhow::Result<()> {
    let (initialize_id, initialize_params) = connection
        .initialize_start()
        .context("waiting for the initialize request")?;

    let params: InitializeParams =
        serde_json::from_value(initialize_params).context("parsing initialize params")?;

    // Encoding is negotiated before anything else: the wrong choice is invisible on ASCII and
    // silently corrupts every position on a line with an emoji, accent, or CJK character.
    let encoding = PositionEncoding::negotiate(
        params
            .capabilities
            .general
            .as_ref()
            .and_then(|general| general.position_encodings.as_deref()),
    );

    let root = workspace_root(&params);
    let (workspace, problems) =
        Workspace::load(root.clone(), params.initialization_options.clone());

    tracing::info!(
        "workspace {}, position encoding {:?}, config {}",
        root.display(),
        encoding,
        workspace
            .config_path()
            .map_or_else(|| "defaults".to_owned(), |path| path.display().to_string())
    );

    connection
        .initialize_finish(
            initialize_id,
            serde_json::json!({
                "capabilities": capabilities::server_capabilities(encoding),
                "serverInfo": capabilities::server_info(),
            }),
        )
        .context("sending the initialize response")?;

    // After the handshake and not inside it: file watching has no static form in the protocol,
    // so the response cannot announce it and `client/registerCapability` is the only way to ask.
    // `initialize_finish` is what waits for `initialized`, which is the point a server is
    // allowed to send requests of its own.
    register_config_watcher(&connection, &params.capabilities, &root);

    // The one file the watcher above covers, spelled the way an incoming notification will be.
    // Held here rather than on the analysis thread because the routing decision is the main
    // thread's: a reload drops the whole graph, so it must not be queued for anything else.
    let config = DocUri::from_path(&root.join(CONFIG_FILE_NAME));

    let cancellations = Cancellations::default();
    let analysis = analysis::spawn(
        workspace,
        encoding,
        ClientSupport::negotiate(&params.capabilities),
        connection.sender.clone(),
        cancellations.clone(),
    );

    for problem in problems {
        tracing::warn!("{problem}");
        show_warning(&connection, &problem);
    }

    let outcome = main_loop(
        &connection,
        analysis.sender(),
        &cancellations,
        config.as_ref(),
    );
    // The analysis thread holds a clone of the sender; it has to go before `connection` does.
    analysis.join();
    outcome
}

/// Ask the client to watch `ya-lsp.toml`, or say why it will not be watched.
fn register_config_watcher(
    connection: &Connection,
    capabilities: &ClientCapabilities,
    root: &Path,
) {
    let Some(registration) = capabilities::config_watcher(root, capabilities) else {
        // Not a warning — plenty of clients are like this and it is nobody's mistake — but not
        // silence either. The alternative is an edit to `ya-lsp.toml` that does nothing at all,
        // with nothing anywhere connecting the two.
        tracing::info!(
            "this editor cannot be asked to watch files, so an edit to {CONFIG_FILE_NAME} \
             takes effect the next time ya-lsp starts"
        );
        return;
    };
    // A string id in the server's own id space, as `Progress::begin` uses: client and server
    // number their requests independently, so this cannot collide with anything the client sent.
    // The answer is read and dropped by the loop below, which warns if it is an error.
    //
    // `json!` rather than `to_value`, as the initialize response above: the one point where
    // this can fail to serialize is `config_watcher`, which answers `None` there, and a second
    // guard here would be a branch nothing can take.
    let _ = connection.sender.send(Message::Request(Request {
        id: RequestId::from("ya-lsp/config-watcher".to_owned()),
        method: "client/registerCapability".to_owned(),
        params: serde_json::json!({ "registrations": [registration] }),
    }));
}

fn main_loop(
    connection: &Connection,
    analysis: &crossbeam_channel::Sender<Task>,
    cancellations: &Cancellations,
    config: Option<&DocUri>,
) -> anyhow::Result<()> {
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection
                    .handle_shutdown(&request)
                    .context("handling shutdown")?
                {
                    return Ok(());
                }
                send(analysis, Task::Request(request));
            }
            Message::Notification(notification) => {
                if let Some(task) = route_notification(notification, cancellations, config) {
                    send(analysis, task);
                }
            }
            // Responses to server-initiated requests — `window/workDoneProgress/create` and
            // the `client/registerCapability` above. Neither answer changes what the server
            // does next: the progress token is ours either way, and a refused registration
            // cannot be retried into a client that does not do registrations. Read and dropped
            // rather than left unread, because an unread response is a message the transport
            // keeps buffering forever — but a refusal is logged, because the two things it
            // costs a user are a missing spinner and a `ya-lsp.toml` that stops reloading.
            Message::Response(response) => match response.response_result {
                Err(error) => tracing::warn!(
                    "the client refused server request {:?}: {}",
                    response.id,
                    error.message
                ),
                Ok(_) => tracing::debug!("response to server request {:?}", response.id),
            },
        }
    }
    Ok(())
}

/// Translate a notification into analysis work, or handle it here if it must not queue.
///
/// `config` is the `ya-lsp.toml` the watcher was registered for; see the `didChangeWatchedFiles`
/// arm for what it is compared against.
fn route_notification(
    notification: Notification,
    cancellations: &Cancellations,
    config: Option<&DocUri>,
) -> Option<Task> {
    match notification.method.as_str() {
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams = parse(notification)?;
            let uri = document_uri(&params.text_document.uri)?;
            Some(Task::DidOpen {
                uri,
                text: params.text_document.text,
                version: Some(params.text_document.version),
            })
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams = parse(notification)?;
            let uri = document_uri(&params.text_document.uri)?;
            // Incremental sync (M2): every change has to reach the analysis thread, in order.
            // Each range is expressed against the text the previous change produced, so
            // keeping only the last one — which full sync allowed — would corrupt the buffer.
            let changes: Vec<TextChange> = params
                .content_changes
                .into_iter()
                .map(|change| TextChange {
                    range: change.range,
                    text: change.text,
                })
                .collect();
            if changes.is_empty() {
                return None;
            }
            Some(Task::DidChange {
                uri,
                changes,
                version: Some(params.text_document.version),
            })
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams = parse(notification)?;
            Some(Task::DidClose {
                uri: document_uri(&params.text_document.uri)?,
            })
        }
        "textDocument/didSave" => {
            let params: DidSaveTextDocumentParams = parse(notification)?;
            Some(Task::DidSave {
                uri: document_uri(&params.text_document.uri)?,
            })
        }
        "workspace/didChangeWatchedFiles" => {
            // A reload drops the whole graph and re-runs the gem index, so the change has to be
            // the config file and not merely *a* file the client happens to watch. Watchers are
            // the client's, shared across every server it runs and every registration each one
            // made, and nothing stops a client delivering all of them here: answering this
            // unconditionally — which it did until v0.2.0 — turns one broad watcher into a
            // full re-index per saved file.
            let params: lsp_types::DidChangeWatchedFilesParams = parse(notification)?;
            let ours = config.is_some_and(|config| {
                params
                    .changes
                    .iter()
                    .any(|change| DocUri::from_lsp(&change.uri).as_ref() == Some(config))
            });
            if !ours {
                tracing::trace!("no watched change named {CONFIG_FILE_NAME}; not reloading");
                return None;
            }
            Some(Task::ReloadConfig)
        }
        "workspace/didChangeConfiguration" => {
            // LSP wraps the payload in `settings`, and its content is whatever the server said
            // it wanted — here, the same shape as `initializationOptions`. A client that sends
            // `null` (VS Code does, when it has nothing to say) means "back to the defaults",
            // which is exactly what dropping the layer produces.
            let params: lsp_types::DidChangeConfigurationParams = parse(notification)?;
            let options = match params.settings {
                serde_json::Value::Null => None,
                settings => Some(settings),
            };
            Some(Task::ChangeConfig { options })
        }
        "$/cancelRequest" => {
            // Recorded here rather than queued: tasks are processed in order, so a cancel sent
            // through the analysis queue would always arrive after the request it cancels.
            if let Some(params) = parse::<lsp_types::CancelParams>(notification) {
                cancellations.cancel(request_id(params.id));
            }
            None
        }
        method => {
            tracing::trace!("ignoring notification {method}");
            None
        }
    }
}

fn document_uri(uri: &lsp_types::Uri) -> Option<DocUri> {
    let canonical = DocUri::from_lsp(uri);
    if canonical.is_none() {
        // Untitled buffers and remote schemes have nothing on disk to index.
        tracing::debug!("ignoring non-file document {}", uri.as_str());
    }
    canonical
}

fn parse<T: serde::de::DeserializeOwned>(notification: Notification) -> Option<T> {
    let method = notification.method.clone();
    match serde_json::from_value(notification.params) {
        Ok(params) => Some(params),
        Err(error) => {
            tracing::warn!("malformed {method}: {error}");
            None
        }
    }
}

fn request_id(id: lsp_types::NumberOrString) -> lsp_server::RequestId {
    match id {
        lsp_types::NumberOrString::Number(number) => number.into(),
        lsp_types::NumberOrString::String(string) => string.into(),
    }
}

fn send(analysis: &crossbeam_channel::Sender<Task>, task: Task) {
    if analysis.send(task).is_err() {
        tracing::error!("analysis thread is gone; dropping work");
    }
}

fn show_warning(connection: &Connection, message: &str) {
    let params = lsp_types::ShowMessageParams {
        typ: lsp_types::MessageType::WARNING,
        message: message.to_owned(),
    };
    let _ = connection
        .sender
        .send(Message::Notification(Notification::new(
            "window/showMessage".to_owned(),
            params,
        )));
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    use lsp_server::{Request, RequestId, Response};

    /// A notification as it arrives off the wire, before lsp-types has looked at it.
    fn notification(method: &str, params: serde_json::Value) -> Notification {
        Notification {
            method: method.to_owned(),
            params,
        }
    }

    /// A `textDocument` identifier for a real path, so `DocUri::from_lsp` accepts it.
    fn document(path: &std::path::Path) -> serde_json::Value {
        serde_json::json!({ "uri": url::Url::from_file_path(path).unwrap().to_string() })
    }

    /// The `ya-lsp.toml` the routing tests pretend the watcher was registered for.
    fn watched_config() -> DocUri {
        DocUri::from_path(std::path::Path::new("/tmp/ya-lsp-route/ya-lsp.toml")).expect("a path")
    }

    fn route(method: &str, params: serde_json::Value) -> Option<Task> {
        route_notification(
            notification(method, params),
            &Cancellations::default(),
            Some(&watched_config()),
        )
    }

    // ------------------------------------------------------------------ document notifications

    #[test]
    fn an_open_document_carries_its_text_and_version() {
        let file = std::path::Path::new("/tmp/ya-lsp-route/lib/person.rb");
        let task = route(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": url::Url::from_file_path(file).unwrap().to_string(),
                    "languageId": "ruby",
                    "version": 7,
                    "text": "class Person\nend\n",
                }
            }),
        );
        match task {
            Some(Task::DidOpen { uri, text, version }) => {
                assert_eq!(uri, DocUri::from_path(file).unwrap());
                assert_eq!(text, "class Person\nend\n");
                assert_eq!(version, Some(7));
            }
            other => panic!("didOpen routed to {other:?}"),
        }
    }

    #[test]
    fn every_edit_reaches_analysis_in_the_order_the_client_sent_it() {
        // Ranges are expressed against the text the previous change produced, so coalescing or
        // reordering them corrupts the buffer. The order is the assertion.
        let file = std::path::Path::new("/tmp/ya-lsp-route/lib/person.rb");
        let task = route(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": document(file)["uri"], "version": 3 },
                "contentChanges": [
                    { "range": { "start": { "line": 0, "character": 0 },
                                 "end":   { "line": 0, "character": 0 } }, "text": "a" },
                    { "range": { "start": { "line": 0, "character": 1 },
                                 "end":   { "line": 0, "character": 1 } }, "text": "b" },
                    { "text": "whole buffer" },
                ]
            }),
        );
        match task {
            Some(Task::DidChange {
                changes, version, ..
            }) => {
                let texts: Vec<&str> = changes.iter().map(|change| change.text.as_str()).collect();
                assert_eq!(texts, ["a", "b", "whole buffer"]);
                // The last one is a full-buffer replacement, which carries no range.
                assert!(changes[0].range.is_some());
                assert!(changes[2].range.is_none());
                assert_eq!(version, Some(3));
            }
            other => panic!("didChange routed to {other:?}"),
        }
    }

    #[test]
    fn a_change_with_nothing_in_it_is_not_work() {
        let file = std::path::Path::new("/tmp/ya-lsp-route/lib/person.rb");
        let task = route(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": document(file)["uri"], "version": 4 },
                "contentChanges": []
            }),
        );
        assert!(task.is_none(), "an empty didChange should queue nothing");
    }

    #[test]
    fn a_closed_document_becomes_a_task() {
        let file = std::path::Path::new("/tmp/ya-lsp-route/lib/person.rb");
        match route(
            "textDocument/didClose",
            serde_json::json!({ "textDocument": document(file) }),
        ) {
            Some(Task::DidClose { uri }) => assert_eq!(uri, DocUri::from_path(file).unwrap()),
            other => panic!("didClose routed to {other:?}"),
        }
    }

    #[test]
    fn a_saved_document_becomes_a_task() {
        let file = std::path::Path::new("/tmp/ya-lsp-route/lib/person.rb");
        match route(
            "textDocument/didSave",
            serde_json::json!({ "textDocument": document(file) }),
        ) {
            Some(Task::DidSave { uri }) => assert_eq!(uri, DocUri::from_path(file).unwrap()),
            other => panic!("didSave routed to {other:?}"),
        }
    }

    #[test]
    fn a_document_with_nothing_on_disk_behind_it_is_ignored() {
        // Untitled buffers and remote schemes have no file to index. Dropping the notification
        // is the whole handling; the alternative is a document keyed by a URI nothing else uses.
        for method in [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didClose",
            "textDocument/didSave",
        ] {
            let task = route(
                method,
                serde_json::json!({
                    "textDocument": {
                        "uri": "untitled:Untitled-1",
                        "languageId": "ruby",
                        "version": 1,
                        "text": "class Person\nend\n",
                    },
                    "contentChanges": [{ "text": "x" }]
                }),
            );
            assert!(task.is_none(), "{method} on an untitled buffer queued work");
        }
    }

    // ------------------------------------------------------------------ workspace notifications

    #[test]
    fn a_watched_file_change_reloads_the_config() {
        // All three kinds, because the watcher is registered for all three: a deleted
        // `ya-lsp.toml` means back to the defaults, and one created where there was none means
        // the opposite.
        for kind in [1, 2, 3] {
            assert!(
                matches!(
                    route(
                        "workspace/didChangeWatchedFiles",
                        serde_json::json!({
                            "changes": [
                                { "uri": "file:///tmp/ya-lsp-route/ya-lsp.toml", "type": kind }
                            ]
                        })
                    ),
                    Some(Task::ReloadConfig)
                ),
                "a change of kind {kind} to ya-lsp.toml did not reload"
            );
        }
    }

    #[test]
    fn the_config_is_recognised_however_the_client_spells_it() {
        // The watcher is registered with our spelling of the root, but the notification comes
        // back through the client's URI writer. Percent-encoding is where the two diverge, and
        // a comparison that missed it would silently stop reloading.
        assert!(matches!(
            route(
                "workspace/didChangeWatchedFiles",
                serde_json::json!({
                    "changes": [{ "uri": "file:///tmp/ya-lsp%2Droute/ya%2Dlsp.toml", "type": 2 }]
                })
            ),
            Some(Task::ReloadConfig)
        ));
    }

    #[test]
    fn a_watched_change_to_anything_else_does_not_reload() {
        // A reload drops the whole graph and re-runs the gem index. Watchers belong to the
        // client and are shared across every server it runs, so a client is free to deliver
        // changes this server never asked for — and until v0.2.0 every one of them cost a full
        // re-index. A same-named file in a subdirectory is here for the same reason: it is not
        // the file this workspace is configured by.
        for uri in [
            "file:///tmp/ya-lsp-route/lib/person.rb",
            "file:///tmp/ya-lsp-route/Gemfile.lock",
            "file:///tmp/ya-lsp-route/vendor/thing/ya-lsp.toml",
            "untitled:Untitled-1",
        ] {
            assert!(
                route(
                    "workspace/didChangeWatchedFiles",
                    serde_json::json!({ "changes": [{ "uri": uri, "type": 2 }] })
                )
                .is_none(),
                "a change to {uri} queued a reload"
            );
        }
        assert!(
            route(
                "workspace/didChangeWatchedFiles",
                serde_json::json!({ "changes": [] })
            )
            .is_none(),
            "a notification carrying no changes queued a reload"
        );
    }

    #[test]
    fn one_notification_reloads_once_however_many_changes_it_carries() {
        // A save can arrive as create-then-change, and a reload is expensive enough that two of
        // them for one edit is worth ruling out here rather than trusting the client.
        let task = route(
            "workspace/didChangeWatchedFiles",
            serde_json::json!({
                "changes": [
                    { "uri": "file:///tmp/ya-lsp-route/lib/person.rb", "type": 2 },
                    { "uri": "file:///tmp/ya-lsp-route/ya-lsp.toml", "type": 1 },
                    { "uri": "file:///tmp/ya-lsp-route/ya-lsp.toml", "type": 2 },
                ]
            }),
        );
        assert!(matches!(task, Some(Task::ReloadConfig)));
    }

    #[test]
    fn a_workspace_with_no_spellable_config_path_reloads_for_nothing() {
        // `workspace_root` ends at `.` when the client sent no folder and the process has no
        // working directory, and there is no URI for that — so no watcher was registered and
        // there is no file to recognise. Reloading on whatever arrives would be a re-index
        // triggered by a request nobody made.
        let task = route_notification(
            notification(
                "workspace/didChangeWatchedFiles",
                serde_json::json!({
                    "changes": [{ "uri": "file:///tmp/ya-lsp-route/ya-lsp.toml", "type": 2 }]
                }),
            ),
            &Cancellations::default(),
            None,
        );
        assert!(task.is_none());
    }

    #[test]
    fn changed_settings_are_carried_rather_than_re_read() {
        // These never touch the filesystem — they are the editor's own settings, and the editor
        // is the only thing that knows them.
        match route(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": { "diagnostics": { "rules": { "undefined-method": "warning" } } } }),
        ) {
            Some(Task::ChangeConfig { options }) => {
                let options = options.expect("settings were sent");
                assert_eq!(
                    options["diagnostics"]["rules"]["undefined-method"],
                    "warning"
                );
            }
            other => panic!("didChangeConfiguration routed to {other:?}"),
        }
    }

    #[test]
    fn null_settings_mean_back_to_the_defaults() {
        // VS Code sends `null` when it has nothing to say. Dropping the layer is exactly what
        // "the defaults" means, so the task is still sent — with no options on it.
        match route(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": null }),
        ) {
            Some(Task::ChangeConfig { options }) => assert!(options.is_none()),
            other => panic!("a null settings payload routed to {other:?}"),
        }
    }

    // ------------------------------------------------------------------ cancellation

    #[test]
    fn a_cancel_is_recorded_here_rather_than_queued() {
        // Tasks are processed in order, so a cancel sent through the analysis queue would
        // always arrive after the request it cancels had already been answered.
        for (sent, expected) in [
            (serde_json::json!(42), RequestId::from(42)),
            (serde_json::json!("abc"), RequestId::from("abc".to_owned())),
        ] {
            let cancellations = Cancellations::default();
            let task = route_notification(
                notification("$/cancelRequest", serde_json::json!({ "id": sent })),
                &cancellations,
                None,
            );
            assert!(task.is_none(), "a cancel must not reach the analysis queue");
            assert!(
                cancellations.take(&expected),
                "{sent} was not recorded as cancelled"
            );
        }
    }

    #[test]
    fn a_malformed_cancel_records_nothing() {
        let cancellations = Cancellations::default();
        let task = route_notification(
            notification(
                "$/cancelRequest",
                serde_json::json!({ "id": { "not": "an id" } }),
            ),
            &cancellations,
            None,
        );
        assert!(task.is_none());
        assert!(!cancellations.take(&RequestId::from(1)));
    }

    // ------------------------------------------------------------------ the fallbacks

    #[test]
    fn an_unrecognised_notification_is_ignored() {
        assert!(route("initialized", serde_json::json!({})).is_none());
        assert!(route("$/setTrace", serde_json::json!({ "value": "off" })).is_none());
    }

    #[test]
    fn malformed_params_are_dropped_rather_than_taking_the_server_down() {
        // A notification has no reply, so there is nowhere to report this but the log. What
        // matters is that one bad message does not end the session.
        for method in [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didClose",
            "textDocument/didSave",
            "workspace/didChangeConfiguration",
            "workspace/didChangeWatchedFiles",
        ] {
            assert!(
                route(method, serde_json::json!({ "textDocument": 7 })).is_none(),
                "{method} accepted params it cannot possibly have parsed"
            );
        }
    }

    #[test]
    fn work_is_dropped_when_the_analysis_thread_is_gone() {
        // Shutdown races: the analysis thread can be joined while the main loop still holds a
        // message. Sending must not panic.
        let (sender, receiver) = crossbeam_channel::unbounded::<Task>();
        drop(receiver);
        send(&sender, Task::ReloadConfig);
    }

    // ------------------------------------------------------------------ the loop itself

    /// A workspace with nothing in it that could reach the machine's Ruby.
    fn workspace() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        root
    }

    /// Drive `serve` over an in-memory connection, as a client would.
    struct Client {
        connection: Connection,
        server: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
        next_id: i32,
    }

    impl Client {
        fn start(root: &std::path::Path) -> Self {
            Self::start_with(root, serde_json::json!({}))
        }

        /// The same, with the capabilities the client announces spelled out.
        fn start_with(root: &std::path::Path, capabilities: serde_json::Value) -> Self {
            let (server_side, client_side) = Connection::memory();
            let server = std::thread::spawn(move || serve(server_side));
            let mut client = Self {
                connection: client_side,
                server: Some(server),
                next_id: 0,
            };
            let uri = url::Url::from_file_path(root).unwrap().to_string();
            let id = client.request(
                "initialize",
                serde_json::json!({
                    "capabilities": capabilities,
                    "workspaceFolders": [{ "uri": uri, "name": "fixture" }],
                }),
            );
            client.response(&id);
            client.notify("initialized", serde_json::json!({}));
            client
        }

        fn request(&mut self, method: &str, params: serde_json::Value) -> RequestId {
            self.next_id += 1;
            let id = RequestId::from(self.next_id);
            self.connection
                .sender
                .send(Message::Request(Request {
                    id: id.clone(),
                    method: method.to_owned(),
                    params,
                }))
                .expect("send");
            id
        }

        fn notify(&self, method: &str, params: serde_json::Value) {
            self.connection
                .sender
                .send(Message::Notification(notification(method, params)))
                .expect("send");
        }

        /// Wait for the first notification the server sends with this method.
        fn wait_for(&self, method: &str) -> Notification {
            let deadline = std::time::Duration::from_secs(30);
            loop {
                match self.connection.receiver.recv_timeout(deadline) {
                    Ok(Message::Notification(sent)) if sent.method == method => return sent,
                    Ok(_) => {}
                    Err(error) => panic!("no {method} arrived: {error}"),
                }
            }
        }

        /// Wait for the first request the *server* sends with this method.
        fn wait_for_request(&self, method: &str) -> Request {
            let deadline = std::time::Duration::from_secs(30);
            loop {
                match self.connection.receiver.recv_timeout(deadline) {
                    Ok(Message::Request(sent)) if sent.method == method => return sent,
                    Ok(_) => {}
                    Err(error) => panic!("no {method} arrived: {error}"),
                }
            }
        }

        fn response(&self, id: &RequestId) -> Response {
            // Generous but finite: a server that stops answering should fail the test rather
            // than hang the suite.
            let deadline = std::time::Duration::from_secs(30);
            loop {
                match self.connection.receiver.recv_timeout(deadline) {
                    Ok(Message::Response(response)) if &response.id == id => return response,
                    Ok(_) => {}
                    Err(error) => panic!("no answer to {id}: {error}"),
                }
            }
        }

        /// Let go of the client end and take what `serve` returned.
        fn finish(mut self) -> anyhow::Result<()> {
            let server = self.server.take().expect("running");
            drop(self.connection);
            server.join().expect("the server thread panicked")
        }
    }

    #[test]
    fn a_client_that_hangs_up_ends_the_loop_cleanly() {
        // No `shutdown`, no `exit` — the editor process died. The loop ends when the receiver
        // does, and `serve` still joins the analysis thread on the way out.
        let root = workspace();
        let client = Client::start(root.path());
        client.finish().expect("a hang-up is not an error");
    }

    #[test]
    fn a_response_to_a_server_request_is_read_and_dropped() {
        // `window/workDoneProgress/create` is the only request the server makes today, and its
        // answer carries nothing to act on. It still has to be read: an unread response is a
        // message the transport buffers forever.
        let root = workspace();
        let mut client = Client::start(root.path());
        client
            .connection
            .sender
            .send(Message::Response(Response::new_ok(
                RequestId::from(9001),
                serde_json::Value::Null,
            )))
            .expect("send");
        // The server is still listening afterwards, which is the point.
        let id = client.request(
            "textDocument/documentSymbol",
            serde_json::json!({
                "textDocument": { "uri": "file:///nowhere/absent.rb" }
            }),
        );
        assert!(client.response(&id).response_result.is_ok());
        client.finish().expect("clean exit");
    }

    #[test]
    fn a_notification_the_server_does_not_route_leaves_the_loop_running() {
        // `route_notification` answering `None` is tested on its own above; this is the other
        // half of it — the loop must send nothing and carry on. Clients send notifications the
        // server never advertised (`$/setTrace`, `telemetry/event`) as a matter of course, and
        // treating one as a dead end would end the session on a message that means nothing.
        let root = workspace();
        let mut client = Client::start(root.path());
        client.notify("$/setTrace", serde_json::json!({ "value": "verbose" }));
        let id = client.request(
            "textDocument/documentSymbol",
            serde_json::json!({
                "textDocument": { "uri": "file:///nowhere/absent.rb" }
            }),
        );
        assert!(client.response(&id).response_result.is_ok());
        client.finish().expect("clean exit");
    }

    #[test]
    fn a_shutdown_followed_by_exit_ends_the_loop() {
        let root = workspace();
        let mut client = Client::start(root.path());
        let id = client.request("shutdown", serde_json::Value::Null);
        assert!(client.response(&id).response_result.is_ok());
        client.notify("exit", serde_json::Value::Null);
        client.finish().expect("clean exit");
    }

    #[test]
    fn a_shutdown_that_is_never_followed_by_exit_is_a_protocol_error() {
        // lsp-server waits for `exit` after answering `shutdown`; anything else is a client
        // that has lost the thread of the protocol, and the error has to reach `run_stdio`
        // rather than being swallowed into a clean return.
        let root = workspace();
        let mut client = Client::start(root.path());
        let id = client.request("shutdown", serde_json::Value::Null);
        assert!(client.response(&id).response_result.is_ok());
        client.notify(
            "textDocument/didSave",
            serde_json::json!({ "textDocument": { "uri": "file:///nowhere/absent.rb" } }),
        );
        let error = client.finish().expect_err("a missing exit is an error");
        assert!(
            format!("{error:#}").contains("shutdown"),
            "unhelpful error: {error:#}"
        );
    }

    #[test]
    fn the_config_watcher_is_registered_with_the_client() {
        // The whole of item 8. Until v0.2.0 nothing in `src/` ever sent
        // `client/registerCapability`, so `ya-lsp.toml` reloaded in exactly one editor — the one
        // whose extension brought a watcher of its own — and the changelog's "changes take
        // effect without a restart" was false everywhere else.
        let root = workspace();
        let mut client = Client::start_with(
            root.path(),
            serde_json::json!({
                "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } }
            }),
        );

        let request = client.wait_for_request("client/registerCapability");
        let registration = &request.params["registrations"][0];
        assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");
        let glob = registration["registerOptions"]["watchers"][0]["globPattern"]
            .as_str()
            .expect("a client without relative patterns is given the absolute form");
        assert_eq!(
            glob,
            format!(
                "{}/ya-lsp.toml",
                root.path().to_string_lossy().replace('\\', "/")
            ),
            "the watcher has to name this workspace's config and no other file"
        );

        // Answering is the client's half of the round trip, and the server has to stay up
        // whichever way it is answered.
        client
            .connection
            .sender
            .send(Message::Response(Response::new_ok(
                request.id.clone(),
                serde_json::Value::Null,
            )))
            .expect("send");
        let id = client.request(
            "textDocument/documentSymbol",
            serde_json::json!({
                "textDocument": { "uri": "file:///nowhere/absent.rb" }
            }),
        );
        assert!(client.response(&id).response_result.is_ok());
        client.finish().expect("clean exit");
    }

    #[test]
    fn a_refused_registration_is_logged_rather_than_swallowed() {
        // The server cannot retry into a client that does not do registrations, so the error
        // changes nothing it does — but it costs the user a `ya-lsp.toml` that stops reloading,
        // and the log is the only place that can say so.
        let root = workspace();
        let mut client = Client::start_with(
            root.path(),
            serde_json::json!({
                "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } }
            }),
        );
        let request = client.wait_for_request("client/registerCapability");
        client
            .connection
            .sender
            .send(Message::Response(Response::new_err(
                request.id,
                lsp_server::ErrorCode::InvalidRequest as i32,
                "no".to_owned(),
            )))
            .expect("send");
        let id = client.request(
            "textDocument/documentSymbol",
            serde_json::json!({
                "textDocument": { "uri": "file:///nowhere/absent.rb" }
            }),
        );
        assert!(client.response(&id).response_result.is_ok());
        client.finish().expect("clean exit");
    }

    #[test]
    fn a_client_that_did_not_ask_for_registrations_is_not_sent_one() {
        // The protocol has no static form for file watching, so there is nothing else to try —
        // and sending it anyway is not free: a client that does not implement
        // `client/registerCapability` answers with an error, and some log it as one.
        let root = workspace();
        let mut client = Client::start(root.path());
        let id = client.request(
            "textDocument/documentSymbol",
            serde_json::json!({
                "textDocument": { "uri": "file:///nowhere/absent.rb" }
            }),
        );
        // The registration, if there were one, would have been sent before this request was
        // even read — so the first message that is not a notification has to be its answer.
        let deadline = std::time::Duration::from_secs(30);
        loop {
            match client.connection.receiver.recv_timeout(deadline) {
                Ok(Message::Response(response)) => {
                    assert_eq!(response.id, id);
                    break;
                }
                Ok(Message::Request(sent)) => panic!("the server sent {}", sent.method),
                Ok(Message::Notification(_)) => {}
                Err(error) => panic!("no answer: {error}"),
            }
        }
        client.finish().expect("clean exit");
    }

    #[test]
    fn a_config_problem_is_shown_to_the_user_rather_than_logged_and_forgotten() {
        // A broken `ya-lsp.toml` must not take the server down, and must not be swallowed into
        // the log either: the file is the user's, and only the user can fix it.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("ya-lsp.toml"), "this is not toml\n").unwrap();

        let client = Client::start(root.path());
        let warning = client.wait_for("window/showMessage");
        assert_eq!(
            warning.params["type"], 2,
            "warnings are MessageType::WARNING"
        );
        let message = warning.params["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("ya-lsp.toml"),
            "the warning should name the file: {message}"
        );
        client.finish().expect("a broken config is not fatal");
    }

    #[test]
    fn initialize_params_that_do_not_parse_end_the_handshake() {
        // Nothing has been negotiated yet, so there is no encoding to answer in and no
        // workspace to open. Failing out is the only honest thing left.
        let (server_side, client_side) = Connection::memory();
        let server = std::thread::spawn(move || serve(server_side));
        client_side
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_owned(),
                params: serde_json::json!({ "capabilities": 7 }),
            }))
            .expect("send");
        let error = server
            .join()
            .expect("thread")
            .expect_err("unparseable params are an error");
        assert!(
            format!("{error:#}").contains("initialize params"),
            "unhelpful error: {error:#}"
        );
    }

    #[test]
    fn a_client_that_never_says_initialized_ends_the_handshake() {
        // The protocol requires `initialized` after the response. lsp-server treats anything
        // else as a client that has lost the thread, and the error has to reach `run_stdio`.
        let root = workspace();
        let (server_side, client_side) = Connection::memory();
        let server = std::thread::spawn(move || serve(server_side));
        let uri = url::Url::from_file_path(root.path()).unwrap().to_string();
        client_side
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_owned(),
                params: serde_json::json!({
                    "capabilities": {},
                    "workspaceFolders": [{ "uri": uri, "name": "fixture" }],
                }),
            }))
            .expect("send");
        client_side
            .sender
            .send(Message::Notification(notification(
                "textDocument/didSave",
                serde_json::json!({ "textDocument": { "uri": "file:///nowhere/absent.rb" } }),
            )))
            .expect("send");
        let error = server
            .join()
            .expect("thread")
            .expect_err("a missing initialized is an error");
        assert!(
            format!("{error:#}").contains("initialize response"),
            "unhelpful error: {error:#}"
        );
    }
}
