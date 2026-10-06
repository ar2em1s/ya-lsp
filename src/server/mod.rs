//! LSP lifecycle, capability negotiation, and the message dispatch loop.
//!
//! The main thread only reads from the connection and routes. Everything that touches the graph
//! runs on the analysis thread, which holds a clone of the connection's sender and writes its own
//! responses. So the main thread stays free to answer `$/cancelRequest` and `shutdown` while
//! analysis is busy.

pub mod capabilities;
pub(crate) mod watcher;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams,
};

use crate::{
    analysis::{
        self, Cancellations, ClientSupport, Task, TextChange, Watched, position::PositionEncoding,
    },
    workspace::{
        DocUri, Workspace,
        config::{CONFIG_FILE_NAME, IndexConfig},
        uri::workspace_root,
    },
};

/// Run the server over stdio until the client shuts it down.
///
/// # Errors
///
/// Returns an error if the LSP handshake fails or the transport breaks.
pub fn run_stdio(reload: crate::logging::Reload) -> anyhow::Result<()> {
    // Before the handshake, not after. A client that sends a malformed `initialize`, or none, is
    // exactly when you need to know which build answered, and by then `serve` has returned. Spelled
    // as `--version` spells it, so one pattern finds both.
    tracing::info!("ya-lsp {} starting", env!("CARGO_PKG_VERSION"));

    let (connection, io_threads) = Connection::stdio();

    // `serve` takes the connection by value on purpose. lsp-server's writer thread stops only once
    // every clone of `Connection::sender` is dropped, so holding one here would make the join below
    // block forever after a clean shutdown.
    let result = serve(connection, reload);

    // Join the transport threads even on failure, or the process leaks them on exit.
    let joined = io_threads.join();
    result?;
    joined.context("LSP transport thread failed")?;
    Ok(())
}

fn serve(connection: Connection, reload: crate::logging::Reload) -> anyhow::Result<()> {
    let (initialize_id, initialize_params) = connection
        .initialize_start()
        .context("waiting for the initialize request")?;

    // Kept beside the parsed copy for the one client capability `lsp-types` has no field for:
    // `workspace.textDocumentContent`, read in `ClientSupport::negotiate`. One clone at startup is
    // cheaper than any way of keeping it typed.
    let raw_params = initialize_params.clone();
    let params: InitializeParams =
        serde_json::from_value(initialize_params).context("parsing initialize params")?;

    // Encoding is negotiated first. A wrong choice is invisible on ASCII and silently corrupts
    // every position on a line with an emoji, accent or CJK character.
    let encoding = PositionEncoding::negotiate(
        params
            .capabilities
            .general
            .as_ref()
            .and_then(|general| general.position_encodings.as_deref()),
    );

    let root = workspace_root(&params);
    let (workspace, mut problems) =
        Workspace::load(root.clone(), params.initialization_options.clone());

    // Before the first line that describes the workspace, on purpose. This is where `[log]` first
    // exists, and everything worth a log file (the config, the Ruby, the bundle, every request)
    // comes after. A `[log]` that cannot be honoured is shown beside the config's own problems.
    problems.extend(reload.apply(&workspace.config().log, &root));
    // After the log is pointed: detection runs inside `Workspace::load`, before `[log]` exists.
    workspace.say_which_way_rails_went();

    let changed = workspace.config().changed_from_defaults();
    tracing::info!(
        "workspace {}, position encoding {:?}, config {} ({})",
        root.display(),
        encoding,
        workspace
            .config_path()
            .map_or_else(|| "defaults".to_owned(), |path| path.display().to_string()),
        if changed.is_empty() {
            "nothing set that the defaults do not already say".to_owned()
        } else {
            format!("changes {}", changed.join(", "))
        }
    );

    connection
        .initialize_finish(
            initialize_id,
            serde_json::json!({
                "capabilities": capabilities::advertised(encoding, &root),
                "serverInfo": capabilities::server_info(),
            }),
        )
        .context("sending the initialize response")?;

    // After the handshake, not inside it. File watching has no static form in the protocol, so only
    // `client/registerCapability` can ask for it. `initialize_finish` waits for `initialized`,
    // after which a server may send its own requests.
    let client_watches = register_file_watchers(
        &connection,
        &params.capabilities,
        &root,
        &workspace.config().index,
        &workspace.lockfiles(),
    );
    // Cloned before the workspace moves onto the analysis thread. The watcher outlives it and keeps
    // the startup configuration; see `watcher::Collector`.
    let index = workspace.config().index.clone();

    // The one file the watcher above covers, spelled as an incoming notification will spell it.
    // Held here, not on the analysis thread, because routing is the main thread's decision: a
    // reload drops the whole graph, so it must not queue behind anything.
    let config = DocUri::from_path(&root.join(CONFIG_FILE_NAME));

    let cancellations = Cancellations::default();
    // Negotiated here, where the client's capabilities are. Sent from the analysis thread, the only
    // place that learns the gem roots. What crosses is a closure, not the table behind it, so the
    // request thread never names the capabilities module; see `analysis::DocumentRegistrar`.
    let requested = capabilities::dynamic_documents(encoding, &params.capabilities, &root);
    let documents: analysis::DocumentRegistrar =
        Box::new(move |prefixes| capabilities::document_registrations(&requested, prefixes));
    let analysis = analysis::spawn(
        workspace,
        encoding,
        ClientSupport::negotiate(
            &params.capabilities,
            raw_params
                .get("capabilities")
                .unwrap_or(&serde_json::Value::Null),
        ),
        documents,
        connection.sender.clone(),
        cancellations.clone(),
        reload,
    );

    for problem in problems {
        tracing::warn!("{problem}");
        show_warning(&connection, &problem);
    }

    // After the analysis thread exists, since the task sender comes from it. Only where the client
    // has no watcher of its own.
    let watching = (!client_watches)
        .then(|| watcher::watch(root, index, config.clone(), analysis.sender().clone()))
        .flatten();

    let outcome = main_loop(
        &connection,
        analysis.sender(),
        &cancellations,
        config.as_ref(),
    );
    // Before the join. The watcher's collector thread holds a clone of the task sender. A live
    // sender means the channel never disconnects, the run loop never breaks, and the join waits
    // forever.
    drop(watching);
    // The analysis thread holds a clone of the sender; it has to go before `connection` does.
    analysis.join();
    outcome
}

/// Ask the client to watch `ya-lsp.toml`, the project's Ruby and its lockfiles. Returns whether it
/// will.
///
/// `false` is not a failure and is not logged as one: many clients take no dynamic registration. It
/// decides whether [`watcher::watch`] runs. **A client that took the registration keeps sending
/// events, and the server never watches the same tree twice.**
fn register_file_watchers(
    connection: &Connection,
    capabilities: &ClientCapabilities,
    root: &Path,
    index: &IndexConfig,
    lockfiles: &[PathBuf],
) -> bool {
    let Some(registration) = capabilities::watched_files(root, index, lockfiles, capabilities)
    else {
        return false;
    };
    // A string id in the server's own id space, as `Progress::begin` uses. Client and server number
    // their requests independently, so this cannot collide. The loop below reads and drops the
    // answer, warning on an error.
    //
    // `json!`, not `to_value`, as in the initialize response above. The one thing that could fail
    // to serialize is `config_watcher`, which answers `None` there, so a second guard here would be
    // a branch nothing can take.
    let _ = connection.sender.send(Message::Request(Request {
        id: RequestId::from("ya-lsp/config-watcher".to_owned()),
        method: "client/registerCapability".to_owned(),
        params: serde_json::json!({ "registrations": [registration] }),
    }));
    true
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
                if !send(analysis, Task::Request(request)) {
                    anyhow::bail!("the analysis thread stopped");
                }
            }
            Message::Notification(notification) => {
                if let Some(task) = route_notification(notification, cancellations, config)
                    && !send(analysis, task)
                {
                    anyhow::bail!("the analysis thread stopped");
                }
            }
            // Responses to our own requests: `window/workDoneProgress/create` and the
            // `client/registerCapability` above. Neither answer changes what the server does: the
            // progress token is ours either way, and a refused registration cannot be retried.
            // - **Read and dropped**, because the transport buffers an unread response forever.
            // - **A refusal is logged**, because it costs the user a missing spinner or a
            //   `ya-lsp.toml` that stops reloading.
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
/// `config` is the `ya-lsp.toml` the watcher was registered for; the `didChangeWatchedFiles` arm
/// says what it is compared against.
fn route_notification(
    notification: Notification,
    cancellations: &Cancellations,
    config: Option<&DocUri>,
) -> Option<Task> {
    match notification.method.as_str() {
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams = parse(notification)?;
            let uri = opened_uri(&params.text_document)?;
            Some(Task::DidOpen {
                uri,
                text: params.text_document.text,
                version: Some(params.text_document.version),
            })
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams = parse(notification)?;
            let uri = document_uri(&params.text_document.uri)?;
            // Incremental sync: every change must reach the analysis thread, in order. Each range
            // is expressed against the text the previous change produced, so keeping only the last
            // would corrupt the buffer.
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
            // Three outcomes per path:
            // - **The config file**: reload. It drops the whole graph and re-runs the gem index, so
            //   it is reserved for the one file that decides what the index is.
            // - **Any other path**: re-index that file alone.
            // - **Ignore it**: the common case. Watchers are the client's, shared across every
            //   server it runs, so a client may deliver anything here. The analysis thread decides,
            //   since the workspace lives there.
            let params: lsp_types::DidChangeWatchedFilesParams = parse(notification)?;
            let mut seen = std::collections::HashSet::with_capacity(params.changes.len());
            let mut uris = Vec::with_capacity(params.changes.len());
            for change in &params.changes {
                // **File-backed only, on purpose.** A watcher watches paths, so nothing can report
                // a change to an unsaved buffer. A path-less URI here means a client confused two
                // notifications, and the buffer it names is already its own newest copy.
                let Some(uri) = DocUri::from_lsp(&change.uri).filter(|uri| !uri.is_untitled())
                else {
                    continue;
                };
                if config == Some(&uri) {
                    // The reload re-indexes the whole workspace from disk, which covers everything
                    // else this notification carried.
                    return Some(Task::ReloadConfig);
                }
                // A create and a change for one path in one notification is ordinary (a generator
                // writes a file, a checkout rewrites it). Indexing twice is work the debounce
                // cannot coalesce, since it only coalesces the resolve.
                if seen.insert(uri.clone()) {
                    uris.push(uri);
                }
            }
            if uris.is_empty() {
                tracing::trace!("nothing in the watched change has a path this server can read");
                return None;
            }
            Some(Task::WatchedFiles {
                uris,
                watched: Watched::ByTheClient,
            })
        }
        "workspace/didChangeConfiguration" => {
            // LSP wraps the payload in `settings`, in whatever shape the server asked for: here,
            // the shape of `initializationOptions`. A `null` (VS Code sends one when it has nothing
            // to say) means "back to the defaults", which is what dropping the layer produces.
            let params: lsp_types::DidChangeConfigurationParams = parse(notification)?;
            let options = match params.settings {
                serde_json::Value::Null => None,
                settings => Some(settings),
            };
            Some(Task::ChangeConfig { options })
        }
        "$/cancelRequest" => {
            // Recorded here, not queued. Tasks run in order, so a queued cancel would always arrive
            // after the request it cancels.
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
        // A remote scheme, or a document of this server's own: nothing to index either way.
        tracing::debug!("ignoring non-file document {}", uri.as_str());
    }
    canonical
}

/// The document a `didOpen` names, or `None` for one this server will not index.
///
/// **`languageId` matters for one kind of document only.**
/// - **A file on disk** is classified by its extension. An `.erb` is a template whatever the client
///   calls it, and the extension is what `workspace::indexes`, `erb::is_template` and every Rails
///   convention read. The field decides nothing here.
/// - **An unsaved buffer** has no extension, and VS Code calls a new tab `plaintext` until a
///   language is picked. A parse error in a document outside the project is published, so indexing
///   a shopping list as Ruby would show a wall of red. Such a buffer is admitted on the client's
///   word alone.
///
/// **The word must be `ruby`, not `erb`.** Five places recognise a template by its path with no
/// table or buffer in reach (`erb::is_template_uri` is a free function over a URI). An unsaved ERB
/// buffer would be indexed as Ruby with its markup in: the wall of red again.
///
/// `didOpen` is the only notification carrying a `languageId`, so it is the only place an unsaved
/// buffer can be admitted. A `didChange` for a buffer never opened is dropped; see the `didChange`
/// arm in `Analysis::handle`.
fn opened_uri(document: &lsp_types::TextDocumentItem) -> Option<DocUri> {
    let uri = document_uri(&document.uri)?;
    if uri.is_untitled() && document.language_id != "ruby" {
        tracing::debug!(
            "ignoring unsaved {} buffer {uri}; only Ruby is indexed without a file behind it",
            document.language_id
        );
        return None;
    }
    Some(uri)
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

/// Hand work to the analysis thread. `false` means the thread is gone.
///
/// The caller ends the loop on `false`, which is the point of the return value. A server whose
/// analysis thread died answers nothing, and that looks exactly like thinking: no error reaches the
/// editor, no process exits, the user waits for a hover that never comes. Exiting gets noticed:
/// every editor restarts a server that stops, none restarts one that goes quiet.
#[must_use]
fn send(analysis: &crossbeam_channel::Sender<Task>, task: Task) -> bool {
    if analysis.send(task).is_err() {
        tracing::error!("the analysis thread is gone; nothing can be answered from here");
        return false;
    }
    true
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
    fn an_unsaved_buffer_is_routed_on_the_client_s_word_and_nothing_else_is() {
        // The only place the door opens. A buffer with no file behind it is work for all four
        // notifications when the client calls it Ruby, and for none otherwise. `languageId` travels
        // on `didOpen` alone, so the other three route on the URI, and the analysis thread drops a
        // change for a buffer it never opened (`Analysis::handle`'s `DidChange` arm).
        let buffer = |language: &str| {
            serde_json::json!({
                "textDocument": {
                    "uri": "untitled:Untitled-1",
                    "languageId": language,
                    "version": 1,
                    "text": "class Person\nend\n",
                },
                "contentChanges": [{ "text": "x" }]
            })
        };
        for method in [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didClose",
            "textDocument/didSave",
        ] {
            assert!(
                route(method, buffer("ruby")).is_some(),
                "{method} on an unsaved Ruby buffer queued nothing"
            );
        }
        // A new VS Code tab is `plaintext` until the user picks a language, and a shopping list
        // indexed as Ruby would show a wall of parse errors, since a document outside the project
        // publishes them. ERB is refused for its own reason: a template is recognised by its path,
        // and this has none. See `opened_uri`.
        for language in ["plaintext", "markdown", "erb"] {
            assert!(
                route("textDocument/didOpen", buffer(language)).is_none(),
                "an unsaved {language} buffer was indexed as Ruby"
            );
        }
        // A scheme with neither a file nor a client to vouch for it is still refused.
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
                        "uri": "ya-lsp-generated:file:///p/db/schema.rb#class:Story",
                        "languageId": "ruby",
                        "version": 1,
                        "text": "class Person\nend\n",
                    },
                    "contentChanges": [{ "text": "x" }]
                }),
            );
            assert!(
                task.is_none(),
                "{method} on a generated document queued work"
            );
        }
    }

    // ------------------------------------------------------------------ workspace notifications

    #[test]
    fn a_watched_file_change_reloads_the_config() {
        // All three kinds, because the watcher is registered for all three. A deleted `ya-lsp.toml`
        // means back to the defaults; a new one means the opposite.
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
        // The watcher is registered with our spelling of the root, but the notification comes back
        // through the client's URI writer. Percent-encoding is where they diverge, and a comparison
        // that missed it would silently stop reloading.
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
    fn a_watched_change_to_anything_else_is_a_file_to_re_index_and_not_a_reload() {
        // A reload drops the whole graph and re-runs the gem index. Watchers are the client's and
        // shared across every server it runs, so a client may deliver changes this server never
        // asked for; a full re-index for each would be ruinous. A same-named file in a subdirectory
        // is not this workspace's config, so it is an ordinary path.
        //
        // Whether this workspace indexes an ordinary path is decided on the analysis thread, where
        // the workspace and its globs live. Routing only keeps it away from the expensive arm.
        for uri in [
            "file:///tmp/ya-lsp-route/lib/person.rb",
            "file:///tmp/ya-lsp-route/Gemfile.lock",
            "file:///tmp/ya-lsp-route/vendor/thing/ya-lsp.toml",
        ] {
            match route(
                "workspace/didChangeWatchedFiles",
                serde_json::json!({ "changes": [{ "uri": uri, "type": 2 }] }),
            ) {
                Some(Task::WatchedFiles { uris, .. }) => {
                    assert_eq!(uris.len(), 1);
                    assert_eq!(uris[0].as_str(), uri);
                }
                other => panic!("a change to {uri} routed to {other:?}"),
            }
        }
    }

    #[test]
    fn a_watched_change_with_no_file_behind_it_queues_nothing() {
        // **The one notification that still refuses an unsaved buffer.** A watcher watches paths,
        // so a change reported for a path-less document means a client confused two notifications,
        // and the buffer is already its own newest copy. The empty notification is here because it
        // must not cost a task either.
        for changes in [
            serde_json::json!([{ "uri": "untitled:Untitled-1", "type": 2 }]),
            serde_json::json!([]),
        ] {
            assert!(
                route(
                    "workspace/didChangeWatchedFiles",
                    serde_json::json!({ "changes": changes })
                )
                .is_none(),
                "{changes} queued work"
            );
        }
    }

    #[test]
    fn one_path_named_twice_in_a_notification_is_indexed_once() {
        // A save arrives as create-then-change, and a branch switch often names one path more than
        // once. Indexing is per file and the resolve debounce cannot coalesce it, so the duplicate
        // is dropped here, where it is cheap to see.
        match route(
            "workspace/didChangeWatchedFiles",
            serde_json::json!({
                "changes": [
                    { "uri": "file:///tmp/ya-lsp-route/lib/person.rb", "type": 1 },
                    { "uri": "file:///tmp/ya-lsp-route/lib/person.rb", "type": 2 },
                    { "uri": "file:///tmp/ya-lsp-route/lib/place.rb", "type": 3 },
                ]
            }),
        ) {
            Some(Task::WatchedFiles { uris, .. }) => assert_eq!(
                uris.iter().map(DocUri::as_str).collect::<Vec<_>>(),
                vec![
                    "file:///tmp/ya-lsp-route/lib/person.rb",
                    "file:///tmp/ya-lsp-route/lib/place.rb",
                ]
            ),
            other => panic!("routed to {other:?}"),
        }
    }

    #[test]
    fn one_notification_reloads_once_however_many_changes_it_carries() {
        // A save can arrive as create-then-change, and a reload is expensive enough that two for
        // one edit is worth ruling out here. The `.rb` beside them is dropped too: the reload
        // re-indexes the whole workspace from disk already.
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
        assert!(
            matches!(task, Some(Task::ReloadConfig)),
            "routed to {task:?}"
        );
    }

    #[test]
    fn a_workspace_with_no_spellable_config_path_reloads_for_nothing() {
        // `workspace_root` ends at `.` when the client sent no folder and the process has no
        // working directory. There is no URI for that, so no watcher was registered and there is no
        // file to recognise. Reloading on whatever arrives would be a re-index nobody asked for, so
        // the change goes down the per-file arm, where a root that strips nothing rejects it.
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
        assert!(
            matches!(task, Some(Task::WatchedFiles { .. })),
            "routed to {task:?}"
        );
    }

    #[test]
    fn changed_settings_are_carried_rather_than_re_read() {
        // These never touch the filesystem: they are the editor's own settings, and only the editor
        // knows them.
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
        // VS Code sends `null` when it has nothing to say. Dropping the layer is what "the
        // defaults" means, so the task is still sent, with no options.
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
        // Tasks run in order, so a cancel sent through the analysis queue would always arrive after
        // its request was answered.
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
        // A notification has no reply, so the log is the only place to report this. One bad message
        // must not end the session.
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
    fn work_for_a_gone_analysis_thread_is_reported_rather_than_dropped() {
        // Sending must not panic: the analysis thread can be joined while the main loop still holds
        // a message. It must not shrug either. The thread is the only thing that answers, so once
        // it is gone the loop stops, which an editor can see. Dropping work silently leaves a
        // server that reads forever and replies to nothing.
        let (sender, receiver) = crossbeam_channel::unbounded::<Task>();
        drop(receiver);
        assert!(!send(&sender, Task::ReloadConfig));
    }

    #[test]
    fn the_loop_stops_once_the_analysis_thread_is_gone() {
        // The end-to-end shape of the rule above: a request arrives, nobody can answer it, and
        // `serve` fails instead of looping. A rubydex resolver panic is a real way to get here.
        let (client, server) = Connection::memory();
        let (analysis, receiver) = crossbeam_channel::unbounded::<Task>();
        drop(receiver);

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "textDocument/hover".to_owned(),
                params: serde_json::json!({}),
            }))
            .unwrap();

        let outcome = main_loop(&server, &analysis, &Cancellations::default(), None);
        assert!(
            outcome.is_err(),
            "the loop kept running with nothing behind it"
        );
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
            let server =
                std::thread::spawn(move || serve(server_side, crate::logging::Reload::default()));
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
            // Generous but finite: a server that stops answering fails the test instead of hanging
            // the suite.
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
        // No `shutdown`, no `exit`: the editor process died. The loop ends with the receiver, and
        // `serve` still joins the analysis thread on the way out.
        let root = workspace();
        let client = Client::start(root.path());
        client.finish().expect("a hang-up is not an error");
    }

    #[test]
    fn a_response_to_a_server_request_is_read_and_dropped() {
        // The answer to `window/workDoneProgress/create` carries nothing to act on. It still has to
        // be read: the transport buffers an unread response forever.
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
        // `route_notification` answering `None` is tested above; this is the other half: the loop
        // sends nothing and carries on. Clients routinely send notifications the server never
        // advertised (`$/setTrace`, `telemetry/event`), and ending the session on one would end it
        // on nothing.
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
        // lsp-server waits for `exit` after answering `shutdown`. Anything else is a client that
        // lost the protocol, and the error must reach `run_stdio`, not become a clean return.
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
        // Without this registration, `ya-lsp.toml` reloads only in an editor whose extension brings
        // its own watcher. "Changes take effect without a restart" would be false everywhere else.
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

        // Answering is the client's half of the round trip. The server must stay up whichever way
        // it answers.
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
        // changes nothing it does. But it costs the user a `ya-lsp.toml` that stops reloading, and
        // only the log can say so.
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
        // File watching has no static form in the protocol, so there is nothing else to try.
        // Sending anyway is not free: a client without `client/registerCapability` answers with an
        // error, and some log it.
        let root = workspace();
        let mut client = Client::start(root.path());
        let id = client.request(
            "textDocument/documentSymbol",
            serde_json::json!({
                "textDocument": { "uri": "file:///nowhere/absent.rb" }
            }),
        );
        // A registration would have been sent before this request was read, so the first
        // non-notification message must be its answer.
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
        // A broken `ya-lsp.toml` must not take the server down, nor be buried in the log: the file
        // is the user's, and only the user can fix it.
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
        // Nothing is negotiated yet: no encoding to answer in, no workspace to open. Failing is the
        // only honest option.
        let (server_side, client_side) = Connection::memory();
        let server =
            std::thread::spawn(move || serve(server_side, crate::logging::Reload::default()));
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
        // The protocol requires `initialized` after the response. lsp-server treats anything else
        // as a client that lost the protocol, and the error must reach `run_stdio`.
        let root = workspace();
        let (server_side, client_side) = Connection::memory();
        let server =
            std::thread::spawn(move || serve(server_side, crate::logging::Reload::default()));
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
