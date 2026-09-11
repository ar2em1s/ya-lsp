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
        // `Notification::new` serializes for us. It is infallible here — `ProgressParams` is
        // strings and an enum — and hand-rolling the struct only to add a dead error arm below
        // it was a branch no test could ever take.
        //
        // A send failure means the client is gone and the main loop is already tearing down.
        let _ = self.outgoing.send(Message::Notification(Notification::new(
            "$/progress".to_owned(),
            params,
        )));
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

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
    fn a_report_lands_once_the_rate_limit_has_passed() {
        // Back-dating `last_report` rather than sleeping: what is being tested is the decision,
        // and a quarter of a second per run is a quarter of a second nobody gets back.
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut progress =
            Progress::begin(&sender, true, "gems", "Indexing gems", "0/10".to_owned()).unwrap();

        progress.report("swallowed".to_owned(), 10);
        progress.last_report = Instant::now()
            .checked_sub(MIN_REPORT_INTERVAL)
            .expect("the process has been running for at least MIN_REPORT_INTERVAL");
        // Over 100: the count and the total are read from different places, and a status bar
        // asked to paint 140% is a bug report rather than a progress stream.
        progress.report("shown".to_owned(), 140);
        progress.end("done".to_owned());
        drop(sender);

        let received: Vec<Message> = receiver.iter().collect();
        assert_eq!(
            kinds(&received),
            vec![
                "window/workDoneProgress/create",
                "$/progress:begin",
                "$/progress:report",
                "$/progress:end"
            ]
        );
        let report = match &received[2] {
            Message::Notification(notification) => &notification.params["value"],
            other => panic!("expected a report, got {other:?}"),
        };
        assert_eq!(report["message"], "shown");
        assert_eq!(report["percentage"], 100, "clamped, not sent as 140");
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

    #[test]
    fn a_reload_during_gem_indexing_closes_the_progress_stream_it_cancelled() {
        // The queued files refer to the old configuration's gem roots, and the graph they were
        // going to be indexed into no longer exists. A stream left open is a spinner forever.
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        // Queued but not stepped: the files are still waiting when the config changes, which is
        // the whole situation this is about.
        harness.analysis.queue_background_indexing();
        let started: Vec<String> = harness
            .progress()
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(started, vec!["begin".to_owned()], "{started:?}");

        harness.run(Task::ReloadConfig);

        // The cancelled stream is closed *before* the reload's own indexing opens a new one.
        // Leaving the first open would put two spinners in the status bar, one of them forever.
        let progress = harness.progress();
        let kinds: Vec<&str> = progress.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, vec!["end", "begin"], "{progress:?}");
        assert_eq!(progress[0].1, "cancelled", "{progress:?}");
    }

    #[test]
    fn a_reload_with_no_progress_stream_open_cancels_just_as_quietly() {
        // The same cancellation, for a client that never advertised `window/workDoneProgress`.
        // There is a stream to close only when there was one to open, and reaching for it
        // unconditionally would take the analysis thread down on the client that asked for
        // least — which is the one least likely to be tested against.
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.analysis.client.work_done_progress = false;
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        harness.analysis.queue_background_indexing();
        assert!(
            harness.analysis.gem_work.is_some(),
            "there is background work to cancel"
        );

        harness.run(Task::ReloadConfig);

        assert_eq!(
            harness.progress(),
            Vec::new(),
            "no stream, no notifications"
        );
        assert_eq!(harness.messages(), Vec::<String>::new());
    }

    #[test]
    fn gem_indexing_reports_progress_and_always_closes_the_stream() {
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        let _ = harness.progress();

        harness.index_gems();

        let progress = harness.progress();
        let kinds: Vec<&str> = progress.iter().map(|(kind, _)| kind.as_str()).collect();
        // A stream that begins and never ends leaves a spinner in the status bar forever.
        assert_eq!(kinds.first(), Some(&"begin"), "{progress:?}");
        assert_eq!(kinds.last(), Some(&"end"), "{progress:?}");
        assert!(
            progress
                .last()
                .is_some_and(|(_, message)| message.contains("1 gems")),
            "{progress:?}"
        );
    }

    #[test]
    fn a_project_with_no_gems_starts_no_progress_stream() {
        // An empty spinner for work that never happens is worse than silence.
        let mut harness = Harness::new();
        harness.write("app/main.rb", "class Mine; end\n");
        harness.index();
        let _ = harness.progress();

        harness.index_gems();
        assert_eq!(harness.progress(), Vec::new());
    }
}
