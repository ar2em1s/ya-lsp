//! Where the log goes, and every decision that puts it there.
//!
//! The log **is** an interface. When a user asks why they have no completions, this is the only
//! thing that answers: which Ruby was picked, how many gems resolved, which `ya-lsp.toml` was
//! read, whether the request arrived at all. That last one is the reason this module exists —
//! a log that cannot separate *the request never arrived* from *the request arrived and
//! answered nothing* cannot diagnose the largest class of defect this server has had.
//!
//! # Two sinks, two filters, and what may be swapped after the fact
//!
//! [`MakeWriterExt::and`] would tee one `fmt` layer into both sinks in a line, and it is the
//! wrong shape: it gives the two sinks **one** filter, and the entire point of the file is that
//! it can sit at `debug` while stderr stays at `info`. So there are two layers, each with its
//! own [`EnvFilter`].
//!
//! **Both layers are registered at startup and neither is ever replaced**, which is not a
//! preference — it is the only shape that works. A per-layer filter is handed a `FilterId` when
//! the layer is registered with the subscriber, so a `reload` handle that swaps a *layer* in
//! afterwards installs one that was never registered, and the first event to reach it panics
//! with *a `Filtered` layer was used, but it had no `FilterId`*. What reloads instead is the
//! part that may: each layer's **filter**, and the file the second one writes to, behind
//! [`Switch`]. The file layer exists from the first line with its filter at `off`, which is what
//! makes turning the file on a reload rather than a re-registration.
//!
//! # Why anything reloads at all
//!
//! `init` runs in `main`, before the handshake; the workspace root that `tmp/ya-lsp.log` is
//! relative to does not exist until `initialize`, and `[log]` itself is not read until
//! `workspace::config::load` has answered. The alternative to a reload handle is what the
//! extension documented for two releases — *the filter is read once, before the server has a
//! client to be configured by* — which is why `ya-lsp.logLevel` used to restart the process.
//!
//! # What may be written
//!
//! Methods, counts, durations, positions, paths and identifiers. **Never a line of the user's
//! source, a rendered hover card or a completion list.** `rename` logging two identifiers and
//! `search` logging the query are the standing precedent and they stay; a card is a different
//! thing, and once there is a copy on disk the difference is the difference between a diagnostic
//! and a transcript of somebody's private repository.
//!
//! [`MakeWriterExt::and`]: tracing_subscriber::fmt::writer::MakeWriterExt::and

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use tracing_subscriber::{
    EnvFilter, Layer, Registry,
    fmt::{self, MakeWriter},
    reload,
};

use crate::{messages, workspace::config::LogConfig};

/// The log filter the server falls back to when nothing says otherwise.
///
/// Named rather than written inline because it is documented somewhere else: it is what
/// `ya-lsp.logLevel` defaults to in the VS Code manifest, which spent two releases saying `warn`
/// instead. `tests/vscode_manifest.rs` holds the two together.
pub const DEFAULT_LOG_FILTER: &str = "info";

/// One destination the log is written to, with its own filter already attached.
type Sink = Box<dyn Layer<Registry> + Send + Sync>;

/// What a sink that is switched off filters at. A real directive rather than a flag, so the
/// "is the file on" question has exactly one answer and it is the one the subscriber reads.
const SILENT: &str = "off";

/// Install the log and hand back the one thing that can change it later.
///
/// `env` is `YA_LSP_LOG`, read at the edge and passed in, so every decision below is a pure
/// function of its arguments — `coverage.md`'s rule for process-wide state, applied to the one
/// piece of it this crate has left.
///
/// The returned layers go into a `Registry` and nowhere else; the caller does the `.init()`,
/// because that is the one line here that cannot be undone or tested.
#[must_use]
pub fn install(env: Option<String>) -> (Vec<Sink>, Reload) {
    // Whatever an unreadable `YA_LSP_LOG` would be worth saying is dropped here and said again
    // by the first `apply`, which is the earliest point there is a client to tell.
    let mut ignored = Vec::new();
    let (stderr_filter, stderr_handle) = reload::Layer::new(match &env {
        Some(level) => filter("YA_LSP_LOG", level, &mut ignored),
        None => EnvFilter::new(directive(DEFAULT_LOG_FILTER)),
    });
    // Off, and present. The file cannot be *added* later — see the module docstring — so it is
    // here from the first line, writing to a switch that holds nothing yet.
    let (file_filter, file_handle) = reload::Layer::new(EnvFilter::new(SILENT));
    let switch = Switch::default();

    let sinks: Vec<Sink> = vec![
        fmt::layer()
            .with_writer(io::stderr)
            .with_ansi(false)
            .with_target(false)
            .with_filter(stderr_filter)
            .boxed(),
        fmt::layer()
            .with_writer(switch.clone())
            .with_ansi(false)
            .with_target(false)
            .with_filter(file_filter)
            .boxed(),
    ];

    (
        sinks,
        Reload {
            stderr: Some(stderr_handle),
            file: Some(file_handle),
            switch,
            env,
        },
    )
}

/// The handle that re-points the log once, and again on every `ya-lsp.toml` change.
///
/// Cloned rather than shared: the main thread applies the first `[log]` at `initialize` and the
/// analysis thread applies every later one, because that is the thread a reload arrives on.
///
/// [`Reload::default`] is **detached** — it decides everything and reaches no subscriber. That
/// is what the in-process harnesses hold, and it is deliberately not a no-op: a test that turns
/// the file on still opens the file and still gets told when it could not.
#[derive(Clone, Default)]
pub struct Reload {
    stderr: Option<reload::Handle<EnvFilter, Registry>>,
    file: Option<reload::Handle<EnvFilter, Registry>>,
    /// Where the file sink writes, or nothing. Shared with the layer installed at startup.
    switch: Switch,
    /// `YA_LSP_LOG`, read once before the handshake and held because every later reload has to
    /// keep honouring it.
    env: Option<String>,
}

/// Written by hand because neither a filter handle nor a file is `Debug` — and what a reader of
/// a dump wants to know about this value is not either of them: it is whether it reaches a
/// subscriber at all, and what the environment said.
impl std::fmt::Debug for Reload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reload")
            .field("live", &self.stderr.is_some())
            .field("env", &self.env)
            .finish()
    }
}

impl Reload {
    /// Point the log at what `config` asks for, and hand back anything the user should be told.
    ///
    /// Never fatal. A `[log]` that cannot be honoured leaves stderr exactly where it was rather
    /// than taking the log away, which is the failure that would make the next bug unreportable.
    pub fn apply(&self, config: &LogConfig, root: &Path) -> Vec<String> {
        let mut problems = Vec::new();

        // `YA_LSP_LOG` outranks `[log] level`, which is the one place this crate's config
        // precedence runs the other way. The variable is what somebody debugging from a terminal
        // types; a project file that silently overrode it would be the opposite of a debugging
        // aid.
        let (field, level) = match self.env.as_deref() {
            Some(level) => ("YA_LSP_LOG", level),
            None => ("log.level", config.level.as_str()),
        };
        let stderr = filter(field, level, &mut problems);

        let path = log_path(root, &config.file_path);
        let file = match config.file.then(|| open(&path)) {
            Some(Ok(file)) => {
                self.switch.point_at(Some(Arc::new(file)));
                filter("log.file_level", &config.file_level, &mut problems)
            }
            Some(Err(error)) => {
                problems.push(messages::log_file_unwritable(&path, &error));
                self.switch.point_at(None);
                EnvFilter::new(SILENT)
            }
            None => {
                self.switch.point_at(None);
                EnvFilter::new(SILENT)
            }
        };

        // **Both, and never `||` between them.** Short-circuiting would leave the file's filter
        // unchanged whenever stderr's handle answered first with an error, which is a sink
        // silently kept at whatever it was last told.
        let stderr_gone = self
            .stderr
            .as_ref()
            .is_some_and(|handle| handle.reload(stderr).is_err());
        let file_gone = self
            .file
            .as_ref()
            .is_some_and(|handle| handle.reload(file).is_err());
        // One `|` and not `||`, for the same reason the two reloads are separate statements: the
        // handles live and die together, so a short-circuit here leaves an arm no run can take.
        if stderr_gone | file_gone {
            // The subscriber is gone, which happens once: while the process is exiting. Not a
            // `messages::` sentence, because there is nobody left to show it to.
            tracing::debug!("the log cannot be re-pointed; the subscriber is already gone");
        }

        problems
    }
}

/// `level` as an `EnvFilter`, or the default plus a sentence saying so.
///
/// A directive nobody can read must not be silent: it is a setting the user believes is in
/// effect, and the symptom — a log at a level they did not choose — is the one symptom nobody
/// attributes to a typo in the setting that decides it.
fn filter(field: &str, level: &str, problems: &mut Vec<String>) -> EnvFilter {
    match EnvFilter::try_new(directive(level)) {
        Ok(filter) => filter,
        Err(_) => {
            problems.push(messages::invalid_log_level(
                field,
                level,
                DEFAULT_LOG_FILTER,
            ));
            EnvFilter::new(directive(DEFAULT_LOG_FILTER))
        }
    }
}

/// A bare level is scoped to ya-lsp; a real filter is passed through as written.
///
/// `debug` on its own means *every crate at debug* to `EnvFilter`, which is not what anybody
/// picking `debug` out of a drop-down meant — it is rubydex's and lsp-server's logs as well as
/// ours. The extension has always sent `ya_lsp=${level}` for exactly this reason; doing it here
/// is what lets the setting stop being an environment variable without changing what it means.
fn directive(level: &str) -> String {
    if level.contains(['=', ',']) {
        level.to_owned()
    } else {
        format!("ya_lsp={level}")
    }
}

/// Where the file sink writes: `file_path` as given if absolute, and under the workspace root
/// if not.
fn log_path(root: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        root.join(configured)
    }
}

/// Open the log for appending, making the directory it lives in if it does not exist yet.
///
/// **Append and never truncate.** Truncate-on-start throws away the session the user is trying
/// to report, and two windows on one project are two processes with one file between them —
/// which is also why every line carries a pid. The directory is created because the default path
/// is inside `tmp/`, and a fresh clone has no `tmp/`.
fn open(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    File::options().append(true).create(true).open(path)
}

/// The file the second sink writes to, or nothing — and the reason the sink itself never moves.
///
/// One `write` per event with the pid in front of it, and both halves are about the
/// two-servers-one-file case. `O_APPEND` makes a single `write` atomic against another
/// process' single `write`, so an event formatted into one buffer and handed over in one call
/// cannot interleave with another server's; splitting the prefix into a second call is what
/// would let it. The pid is what tells the two apart afterwards.
#[derive(Clone, Default)]
struct Switch(Arc<std::sync::RwLock<Option<Arc<File>>>>);

impl Switch {
    /// Point the sink at a file, or at nothing.
    fn point_at(&self, file: Option<Arc<File>>) {
        // A panic in another thread must not take the log down; the value is plain data and is
        // safe to keep using, which is the same call `Cancellations` makes one level up.
        let mut held = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = file;
    }
}

impl io::Write for Switch {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let held = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Nothing to write to is not an error. The filter above this is at `off` whenever that
        // is true, so this arm is the window between a reload and the event that raced it.
        let Some(file) = held.as_ref() else {
            return Ok(buf.len());
        };
        let prefix = format!("[{}] ", std::process::id());
        let mut line = Vec::with_capacity(prefix.len() + buf.len());
        line.extend_from_slice(prefix.as_bytes());
        line.extend_from_slice(buf);
        let mut file: &File = file;
        file.write_all(&line)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let held = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match held.as_ref() {
            Some(file) => {
                let mut file: &File = file;
                file.flush()
            }
            None => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for Switch {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// The file sink on, at its default path.
    fn on() -> LogConfig {
        LogConfig {
            file: true,
            ..LogConfig::default()
        }
    }

    /// A configured stderr level, with the file off.
    fn level(level: &str) -> LogConfig {
        LogConfig {
            level: level.to_owned(),
            ..LogConfig::default()
        }
    }

    #[test]
    fn a_bare_level_is_scoped_to_ya_lsp_and_a_filter_is_not() {
        // `debug` out of a drop-down means "ya-lsp, in detail" and never "every crate linked
        // into it", which is rubydex and lsp-server as well.
        assert_eq!(directive("debug"), "ya_lsp=debug");
        assert_eq!(directive("off"), "ya_lsp=off");
        assert_eq!(directive("ya_lsp=trace"), "ya_lsp=trace");
        assert_eq!(directive("info,ya_lsp=trace"), "info,ya_lsp=trace");
    }

    #[test]
    fn a_directive_nobody_can_read_falls_back_and_says_so() {
        let mut problems = Vec::new();
        let _ = filter("log.level", "verbose", &mut problems);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("log.level"), "{problems:?}");
        assert!(problems[0].contains("verbose"), "{problems:?}");

        // And a readable one is silent.
        let mut quiet = Vec::new();
        let _ = filter("log.level", "debug", &mut quiet);
        assert!(quiet.is_empty(), "{quiet:?}");
    }

    #[test]
    fn the_path_is_the_root_unless_the_setting_is_absolute() {
        let root = Path::new("/w");
        assert_eq!(
            log_path(root, Path::new("tmp/ya-lsp.log")),
            PathBuf::from("/w/tmp/ya-lsp.log")
        );
        assert_eq!(
            log_path(root, Path::new("/var/log/ya-lsp.log")),
            PathBuf::from("/var/log/ya-lsp.log")
        );
    }

    #[test]
    fn both_sinks_exist_from_the_first_line_and_only_one_of_them_is_on() {
        // The shape the module docstring argues for: a per-layer filter is given its id when the
        // layer is registered, so a file layer added later would panic on the first event that
        // reached it. It is registered from the start, at `off`.
        let (sinks, reload) = install(None);
        assert_eq!(sinks.len(), 2);
        assert!(reload.switch.0.read().unwrap().is_none(), "nothing on disk");
    }

    #[test]
    fn nothing_is_written_or_created_until_the_file_is_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let problems = Reload::default().apply(&LogConfig::default(), dir.path());
        assert!(problems.is_empty(), "{problems:?}");
        // Off by default means off on disk: nothing is created by asking.
        assert!(!dir.path().join("tmp").exists());
    }

    #[test]
    fn turning_the_file_on_makes_the_directory_it_needs() {
        // The default path is inside `tmp/`, and a fresh clone has no `tmp/`. Without this the
        // setting would fail for exactly the people who have never built anything yet.
        let dir = tempfile::tempdir().unwrap();
        let reload = Reload::default();
        assert!(reload.apply(&on(), dir.path()).is_empty());
        assert!(dir.path().join("tmp/ya-lsp.log").is_file());
        assert!(
            reload.switch.0.read().unwrap().is_some(),
            "the sink points at it"
        );

        // And turning it back off puts the sink back rather than leaving the file held open.
        assert!(reload.apply(&LogConfig::default(), dir.path()).is_empty());
        assert!(reload.switch.0.read().unwrap().is_none());
    }

    #[test]
    fn a_file_that_cannot_be_opened_leaves_stderr_alone_and_says_why() {
        // A directory where the file should be. The sink is missing; the log is not.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tmp/ya-lsp.log")).unwrap();
        let reload = Reload::default();
        let problems = reload.apply(&on(), dir.path());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("ya-lsp.log"), "{problems:?}");
        assert!(
            reload.switch.0.read().unwrap().is_none(),
            "and the sink is left pointing at nothing rather than at a half-open file"
        );
    }

    #[test]
    fn the_environment_outranks_the_file_and_is_reported_under_its_own_name() {
        let dir = tempfile::tempdir().unwrap();
        let (_layer, reload) = install(Some("verbose".to_owned()));
        let said = reload.apply(&level("nonsense"), dir.path());
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("YA_LSP_LOG"), "{said:?}");
        assert!(said[0].contains("verbose"), "{said:?}");

        // Held on the handle rather than re-read, because `apply` runs on the analysis thread
        // and a reload that forgot the variable would quietly undo it on the first ya-lsp.toml
        // change.
        let said = reload.apply(&LogConfig::default(), dir.path());
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("YA_LSP_LOG"), "{said:?}");

        // With no variable set it is the file's key that is named.
        let said = Reload::default().apply(&level("nonsense"), dir.path());
        assert!(said[0].contains("log.level"), "{said:?}");
    }

    #[test]
    fn every_line_in_the_file_carries_the_pid_and_arrives_in_one_write() {
        // Two windows on one project are two processes with one file between them. `O_APPEND`
        // makes a single `write` atomic; what this pins is that an event *is* a single write,
        // prefix included, rather than two that another process can interleave between.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/ya-lsp.log");
        let mut writer = Switch::default();
        writer.point_at(Some(Arc::new(open(&path).unwrap())));
        let written = writer.write(b"INFO hello\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(
            written,
            b"INFO hello\n".len(),
            "the prefix is not the caller's"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("[{}] INFO hello\n", std::process::id()));

        // And a second server appends rather than truncating.
        let mut again = Switch::default();
        again.point_at(Some(Arc::new(open(&path).unwrap())));
        again.write_all(b"INFO second\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn a_sink_pointing_at_nothing_swallows_the_line_rather_than_failing() {
        // The window between a reload that turned the file off and an event that raced it. The
        // filter is at `off` by then, so this is a guard rather than a path — and an error here
        // would reach `fmt`'s own "log line was dropped" reporting for no reason at all.
        let mut writer = Switch::default();
        assert_eq!(writer.write(b"INFO nowhere\n").unwrap(), 13);
        writer.flush().unwrap();
    }

    #[test]
    fn a_file_path_that_is_not_a_file_is_reported_rather_than_panicking() {
        // Two shapes, both of them something a user can type. `/` has no directory above it, so
        // the guard in `open` is what stands between a mistyped setting and `parent()` being
        // asked about a path that has none — and the answer either way is the sentence, not a
        // server that will not start.
        let dir = tempfile::tempdir().unwrap();
        let problems = Reload::default().apply(
            &LogConfig {
                file: true,
                file_path: PathBuf::from("/"),
                ..LogConfig::default()
            },
            dir.path(),
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("could not be opened"), "{problems:?}");
    }

    #[test]
    fn a_directory_that_cannot_be_made_is_the_same_answer_as_a_file_that_cannot_be_opened() {
        // The default path is two segments, so the *directory* is as likely to be the problem as
        // the file: a project with a file called `tmp` gets here, and used to get a `?` with
        // nothing on the other end of it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tmp"), "not a directory").unwrap();
        let problems = Reload::default().apply(&on(), dir.path());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("log.file_path"), "{problems:?}");
    }

    #[test]
    fn a_dead_subscriber_is_not_a_problem_anyone_is_shown() {
        // The shutdown path. There is nobody left to show a message to, and a warning nobody can
        // act on at the end of every session is noise in the file it lands in.
        let dir = tempfile::tempdir().unwrap();
        let (layer, reload) = install(None);
        assert!(reload.apply(&LogConfig::default(), dir.path()).is_empty());

        drop(layer);
        let problems = reload.apply(&LogConfig::default(), dir.path());
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_detached_handle_says_what_it_is() {
        let detached = Reload::default();
        assert_eq!(format!("{detached:?}"), "Reload { live: false, env: None }");
        let (_layer, live) = install(Some("debug".to_owned()));
        assert_eq!(
            format!("{live:?}"),
            "Reload { live: true, env: Some(\"debug\") }"
        );
    }
}
