//! `$/progress`: telling the editor that indexing is still going.
//!
//! Gem indexing takes seconds on a large bundle. Without a progress stream the user sees a server
//! that answers some questions and not others, with no explanation; with one they see "Indexing
//! gems 40/151" and know to wait.

use lsp_server::{Message, Notification, Request, RequestId};
use lsp_types::{
    NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
    WorkDoneProgressCreateParams, WorkDoneProgressEnd, WorkDoneProgressReport,
};

use crossbeam_channel::Sender;
use std::time::{Duration, Instant};

/// Do not repaint the editor's status bar more often than this. Reporting per batch would send
/// hundreds of notifications for something a human reads once a second.
const MIN_REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// One in-flight work-done progress stream.
#[derive(Debug)]
pub struct Progress {
    token: String,
    outgoing: Sender<Message>,
    last_report: Instant,
    /// Whether the closing message has already gone out: the only thing [`Drop`] needs to know. See
    /// [`Progress::end`].
    ended: bool,
}

impl Progress {
    /// Start a progress stream, or return `None` when the client did not ask for one.
    ///
    /// The spec says a server should wait for the `window/workDoneProgress/create` response before
    /// its first `$/progress`. We do not, for rust-analyzer's reason: waiting blocks the analysis
    /// thread on a round trip, and a client that rejects the token simply ignores the notifications
    /// that follow.
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
            ended: false,
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

    /// Close the stream, saying what was finished.
    ///
    /// Taking `self` by value is what makes the *message* meaningful: a stream closes once, and
    /// only its owner knows what it accomplished. What guarantees the stream closes at all is
    /// [`Drop`] below, not this signature.
    pub fn end(mut self, message: String) {
        self.ended = true;
        self.send(WorkDoneProgress::End(WorkDoneProgressEnd {
            message: Some(message),
        }));
    }

    fn send(&self, value: WorkDoneProgress) {
        let params = ProgressParams {
            token: NumberOrString::String(self.token.clone()),
            value: ProgressParamsValue::WorkDone(value),
        };
        // `Notification::new` serializes for us. It is infallible here (`ProgressParams` is strings
        // and an enum), and hand-rolling the struct would only add a dead error arm no test could
        // take.
        //
        // A send failure means the client is gone and the main loop is already tearing down.
        let _ = self.outgoing.send(Message::Notification(Notification::new(
            "$/progress".to_owned(),
            params,
        )));
    }
}

/// **A stream that begins and never ends leaves a spinner in the status bar forever**, and "every
/// path calls `end`" is not something a signature can promise.
///
/// The path that does not is a panic. Three seams in this crate catch one and carry on:
/// - `Analysis::serve`, around a request;
/// - `Analysis::resolve`, around the link;
/// - `indexer`, around one file.
///
/// So a panic between [`Progress::begin`] and [`Progress::end`] does **not** take the process with
/// it. The thread survives, the server keeps answering, and the only trace is a status bar that
/// spins until the editor restarts. That is the worst defect this type can have, because everything
/// else still works.
///
/// So the close is a destructor, and the flag keeps it from doubling: [`Progress::end`] sets
/// `ended` and then drops normally, one message either way. The message here is deliberately not a
/// success sentence: whatever was being reported did not finish.
impl Drop for Progress {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        self.send(WorkDoneProgress::End(WorkDoneProgressEnd {
            message: Some("interrupted".to_owned()),
        }));
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
        // Immediately after `begin`, so the rate limit swallows it. That is the point of the limit,
        // and it must not swallow the `end`.
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
    fn a_stream_dropped_without_being_ended_closes_itself() {
        // The path no signature can cover: a panic between `begin` and `end`. Three seams in this
        // crate catch one and keep the thread alive, so the stream is not cleaned up by the process
        // dying; it just spins forever while everything else works.
        let (sender, receiver) = crossbeam_channel::unbounded();
        let progress =
            Progress::begin(&sender, true, "gems", "Indexing gems", String::new()).unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = progress;
            panic!("the indexer went down");
        }));
        assert!(caught.is_err(), "the panic is the point of the fixture");
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
        let end = match received.last() {
            Some(Message::Notification(notification)) => &notification.params["value"],
            other => panic!("expected an end, got {other:?}"),
        };
        // Not a success sentence: whatever was being reported did not finish.
        assert_eq!(end["message"], "interrupted");
    }

    #[test]
    fn a_stream_that_was_ended_is_not_ended_again_when_it_drops() {
        // The other half of the flag. Two `end` messages for one stream tell a client to close
        // something already closed: the defect the destructor would introduce if it did not check.
        let (sender, receiver) = crossbeam_channel::unbounded();
        Progress::begin(&sender, true, "gems", "Indexing gems", String::new())
            .unwrap()
            .end("done".to_owned());
        drop(sender);

        assert_eq!(
            kinds(&receiver.iter().collect::<Vec<Message>>()),
            vec![
                "window/workDoneProgress/create",
                "$/progress:begin",
                "$/progress:end"
            ]
        );
    }

    #[test]
    fn a_report_lands_once_the_rate_limit_has_passed() {
        // Back-dating `last_report` instead of sleeping: what is tested is the decision, and a
        // quarter of a second per run is time nobody gets back.
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut progress =
            Progress::begin(&sender, true, "gems", "Indexing gems", "0/10".to_owned()).unwrap();

        progress.report("swallowed".to_owned(), 10);
        progress.last_report = Instant::now()
            .checked_sub(MIN_REPORT_INTERVAL)
            .expect("the process has been running for at least MIN_REPORT_INTERVAL");
        // Over 100: the count and the total are read from different places, and a status bar asked
        // to paint 140% is a bug report, not a progress stream.
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
        // going into no longer exists. A stream left open is a spinner forever.
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        // The pipeline's last stage opens a stream of its own, and `index` runs it, so drain that
        // pair before the one this test is about.
        let _ = harness.progress();
        // Queued but not stepped: the files are still waiting when the config changes, which is the
        // situation this test is about.
        harness.analysis.queue_background_indexing();
        let started: Vec<String> = harness
            .progress()
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(started, vec!["begin".to_owned()], "{started:?}");

        harness.run(Task::ReloadConfig);

        // The cancelled stream is closed *before* the reload's own indexing opens a new one.
        // Leaving it open would put two spinners in the status bar, one of them forever.
        //
        // The pair in the middle is the rebuild's own last stage: a reload re-runs the cold start,
        // including the generator pass, which streams. The gem stream the reload re-queues comes
        // back last.
        let progress = harness.progress();
        let kinds: Vec<&str> = progress.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, vec!["end", "begin", "end", "begin"], "{progress:?}");
        assert_eq!(progress[0].1, "cancelled", "{progress:?}");
    }

    #[test]
    fn a_reload_with_no_progress_stream_open_cancels_just_as_quietly() {
        // The same cancellation, for a client that never advertised `window/workDoneProgress`.
        // There is a stream to close only if one was opened, and reaching for it unconditionally
        // would take the analysis thread down on the client that asked for least, the one least
        // likely to be tested against.
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.analysis.client.work_done_progress = false;
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        harness.analysis.queue_background_indexing();
        assert!(
            harness.analysis.stage.indexing_bundle(),
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
        // Two streams, in this order: the bundle's, then the pipeline's last stage, which runs the
        // moment the last gem is in. A stream that begins and never ends leaves a spinner forever,
        // so both pairs are asserted, not just the outside of them.
        assert_eq!(kinds, vec!["begin", "end", "begin", "end"], "{progress:?}");
        assert!(progress[1].1.contains("1 gems"), "{progress:?}");
    }

    #[test]
    fn the_last_stage_of_the_pipeline_streams_and_closes_under_its_own_token() {
        // **The window this exists for is the one between the two streams.** Everything before it
        // is fast or streamed; the generator pass is neither, and on a large application it can run
        // for over a second saying nothing. To whoever is watching that looks like a hang, and to
        // the audit's `settle` it is silence to guess about instead of a statement to wait on.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", "class Story\nend\n");

        harness.index();

        let tokens: Vec<String> = harness
            .notifications("$/progress")
            .iter()
            .map(|notification| {
                notification.params["token"]
                    .as_str()
                    .unwrap_or("?")
                    .to_owned()
            })
            .collect();
        // A token of its own, and not for tidiness: the gem stream's `end` and this one's `begin`
        // are adjacent, so a shared token would let an editor render the two as one stream, and the
        // second `end` would close a stream the first already closed.
        assert_eq!(
            tokens,
            vec!["ya-lsp/generate".to_owned(), "ya-lsp/generate".to_owned()],
            "{tokens:?}"
        );
    }

    #[test]
    fn a_client_that_declined_progress_gets_no_stream_from_the_last_stage_either() {
        // The half that is easy to leave out: `Progress::begin` answers `None`, and the `if let`
        // keeps the analysis thread off a `None` it would otherwise unwrap. The client that
        // advertised least is the one least likely to be tested against.
        let mut harness = Harness::new();
        harness.analysis.client.work_done_progress = false;
        harness.write("app/models/story.rb", "class Story\nend\n");

        harness.index();

        assert_eq!(
            harness.progress(),
            Vec::new(),
            "no stream, no notifications"
        );
    }

    #[test]
    fn a_project_with_no_gems_starts_no_progress_stream() {
        // An empty spinner for work that never happens is worse than silence. The drain below makes
        // this about the *gem* stream: `index` has already run the pipeline's last stage, which
        // opens its own stream whether or not there is a bundle.
        let mut harness = Harness::new();
        harness.write("app/main.rb", "class Mine; end\n");
        harness.index();
        let _ = harness.progress();

        harness.index_gems();
        assert_eq!(harness.progress(), Vec::new());
    }
}
