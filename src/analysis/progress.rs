//! `$/progress` — telling the editor that indexing is still going.
//!
//! Gem indexing runs for seconds on a large bundle. Without a progress stream the user sees a
//! server that answers some questions and not others, with no explanation; with one they see
//! "Indexing gems 40/151" and know to wait.

use lsp_server::{Message, Notification, Request, RequestId};
use lsp_types::{
    NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
    WorkDoneProgressCreateParams, WorkDoneProgressEnd, WorkDoneProgressReport,
};

use crossbeam_channel::Sender;
use std::time::{Duration, Instant};

/// Don't repaint the editor's status bar more often than this. Reporting per batch would send
/// hundreds of notifications for something a human reads once a second.
const MIN_REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// One in-flight work-done progress stream.
#[derive(Debug)]
pub struct Progress {
    token: String,
    outgoing: Sender<Message>,
    last_report: Instant,
}

impl Progress {
    /// Start a progress stream, or return `None` when the client did not ask for one.
    ///
    /// The spec says a server should wait for the `window/workDoneProgress/create` response
    /// before sending its first `$/progress`. We do not, for the same reason rust-analyzer does
    /// not: waiting means blocking the analysis thread on a round trip, and a client that
    /// rejects the token simply ignores the notifications that follow.
    pub fn begin(
        outgoing: &Sender<Message>,
        supported: bool,
        token: &str,
        title: &str,
        message: String,
    ) -> Option<Self> {
        if !supported {
            return None;
        }

        let params = serde_json::to_value(WorkDoneProgressCreateParams {
            token: NumberOrString::String(token.to_owned()),
        })
        .ok()?;

        // A string id, in the server's own id space. Client and server number their requests
        // independently, so this cannot collide with anything the client sent.
        let _ = outgoing.send(Message::Request(Request {
            id: RequestId::from(format!("ya-lsp/{token}")),
            method: "window/workDoneProgress/create".to_owned(),
            params,
        }));

        let progress = Self {
            token: token.to_owned(),
            outgoing: outgoing.clone(),
            last_report: Instant::now(),
        };
        progress.send(WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: title.to_owned(),
            cancellable: Some(false),
            message: Some(message),
            percentage: Some(0),
        }));
        Some(progress)
    }

    /// Update the stream, unless the last update was too recent to be worth another repaint.
    pub fn report(&mut self, message: String, percentage: u32) {
        if self.last_report.elapsed() < MIN_REPORT_INTERVAL {
            return;
        }
        self.last_report = Instant::now();
        self.send(WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: Some(false),
            message: Some(message),
            percentage: Some(percentage.min(100)),
        }));
    }

    /// Close the stream. Taking `self` by value is the point: a stream that is begun and never
    /// ended leaves a spinner in the editor's status bar forever.
    pub fn end(self, message: String) {
        self.send(WorkDoneProgress::End(WorkDoneProgressEnd {
            message: Some(message),
        }));
    }

    fn send(&self, value: WorkDoneProgress) {
        let params = ProgressParams {
            token: NumberOrString::String(self.token.clone()),
            value: ProgressParamsValue::WorkDone(value),
        };
        let Ok(params) = serde_json::to_value(params) else {
            return;
        };
        // A send failure means the client is gone and the main loop is already tearing down.
        let _ = self.outgoing.send(Message::Notification(Notification {
            method: "$/progress".to_owned(),
            params,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(received: &[Message]) -> Vec<String> {
        received
            .iter()
            .map(|message| match message {
                Message::Request(request) => request.method.clone(),
                Message::Notification(notification) => {
                    let kind = notification.params["value"]["kind"]
                        .as_str()
                        .unwrap_or("?")
                        .to_owned();
                    format!("{}:{kind}", notification.method)
                }
                Message::Response(_) => "response".to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_client_that_did_not_ask_for_progress_is_sent_none() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        assert!(Progress::begin(&sender, false, "t", "Indexing", String::new()).is_none());
        drop(sender);
        assert_eq!(receiver.iter().count(), 0);
    }

    #[test]
    fn a_stream_creates_its_token_before_reporting_and_always_ends() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut progress =
            Progress::begin(&sender, true, "gems", "Indexing gems", "0/10".to_owned()).unwrap();
        // Immediately after `begin`, so the rate limit swallows it — that is the point of the
        // limit, and it must not swallow the `end`.
        progress.report("5/10".to_owned(), 50);
        progress.end("done".to_owned());
        drop(sender);

        let received: Vec<Message> = receiver.iter().collect();
        assert_eq!(
            kinds(&received),
            vec![
                "window/workDoneProgress/create",
                "$/progress:begin",
                "$/progress:end"
            ]
        );
    }

    #[test]
    fn the_token_is_the_same_on_every_message_of_a_stream() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let progress =
            Progress::begin(&sender, true, "gems", "Indexing gems", String::new()).unwrap();
        progress.end(String::new());
        drop(sender);

        for message in receiver {
            let params = match message {
                Message::Request(request) => request.params,
                Message::Notification(notification) => notification.params,
                Message::Response(_) => continue,
            };
            assert_eq!(params["token"], "gems");
        }
    }
}
