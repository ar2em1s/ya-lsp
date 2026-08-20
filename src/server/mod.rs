//! LSP lifecycle, capability negotiation, and the message dispatch loop.
//!
//! The main thread only reads from the connection and routes. Everything that touches the
//! graph happens on the analysis thread, which holds a clone of the connection's sender and
//! writes its own responses. That keeps the main thread free to answer `$/cancelRequest` and
//! `shutdown` promptly even while analysis is busy.

pub mod capabilities;

use anyhow::Context as _;
use lsp_server::{Connection, Message, Notification};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, InitializeParams,
};

use crate::{
    analysis::{self, Cancellations, ClientSupport, Task, TextChange, position::PositionEncoding},
    workspace::{DocUri, Workspace, uri::workspace_root},
};

/// Run the server over stdio until the client shuts it down.
///
/// # Errors
///
/// Returns an error if the LSP handshake fails or the transport breaks.
pub fn run_stdio() -> anyhow::Result<()> {
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

    let outcome = main_loop(&connection, analysis.sender(), &cancellations);
    // The analysis thread holds a clone of the sender; it has to go before `connection` does.
    analysis.join();
    outcome
}

fn main_loop(
    connection: &Connection,
    analysis: &crossbeam_channel::Sender<Task>,
    cancellations: &Cancellations,
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
                if let Some(task) = route_notification(notification, cancellations) {
                    send(analysis, task);
                }
            }
            // Responses to server-initiated requests — today only
            // `window/workDoneProgress/create`, whose answer carries no information we act on:
            // the token is ours either way, and a client that refuses it simply ignores the
            // notifications that follow. Read and dropped rather than left unread, because an
            // unread response is a message the transport keeps buffering forever.
            Message::Response(response) => {
                tracing::debug!("response to server request {:?}", response.id);
            }
        }
    }
    Ok(())
}

/// Translate a notification into analysis work, or handle it here if it must not queue.
fn route_notification(notification: Notification, cancellations: &Cancellations) -> Option<Task> {
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
        "workspace/didChangeWatchedFiles" => Some(Task::ReloadConfig),
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
    if let Ok(params) = serde_json::to_value(params) {
        let _ = connection.sender.send(Message::Notification(Notification {
            method: "window/showMessage".to_owned(),
            params,
        }));
    }
}
