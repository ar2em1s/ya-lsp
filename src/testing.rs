//! Test-only helpers shared across modules.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, Once};

use tracing::{
    Event, Level, Metadata,
    field::{Field, Visit},
    span,
    subscriber::Interest,
};

thread_local! {
    /// Where this thread's events go, when it has asked for any.
    static SINK: RefCell<Option<(Level, Arc<Mutex<String>>)>> = const { RefCell::new(None) };
}

/// A subscriber that is off for every thread that has not asked to capture.
///
/// The shape matters and took a wrong turn first. `tracing` caches an `Interest` per callsite
/// **globally**: the first test to reach a `tracing::info!` with no subscriber installed pins it
/// at `Interest::never()`, and a later `with_default` on another thread never re-evaluates it —
/// so the capture came back empty in the full suite while passing when run alone.
///
/// Answering `Interest::sometimes` is what fixes it: the callsite is re-asked per event, so a
/// thread with a sink captures and a thread without one does not. That second half is not an
/// optimisation, it is the honest part. Enabling the level process-wide would execute every
/// `info!` argument in the crate and mark ~30 lines covered that nothing asserts — which is the
/// gaming this milestone's rules forbid. Here a `tracing::` line is covered exactly when some
/// test asked to read it.
struct Capture;

impl tracing::Subscriber for Capture {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        SINK.with_borrow(|sink| {
            sink.as_ref()
                .is_some_and(|(level, _)| metadata.level() <= level)
        })
    }

    fn event(&self, event: &Event<'_>) {
        SINK.with_borrow(|sink| {
            let Some((_, buffer)) = sink.as_ref() else {
                return;
            };
            let mut line = format!(
                "{} {}: ",
                event.metadata().level(),
                event.metadata().target()
            );
            event.record(&mut Recorder(&mut line));
            let mut held = buffer.lock().expect("log buffer");
            held.push_str(&line);
            held.push('\n');
        });
    }

    // Spans are not used anywhere in the crate; these exist to satisfy the trait.
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

struct Recorder<'a>(&'a mut String);

impl Visit for Recorder<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

/// Run `body` with every event at `level` or above captured, and hand back what was logged.
///
/// The log **is** an interface. When a user asks why they have no completions, stderr is the
/// only thing that answers: which rbs was used, how many gems resolved, which `ya-lsp.toml` was
/// read. Those lines are as much a product as a hover card, and none of them was asserted
/// anywhere before this existed.
pub fn captured_logs<T>(level: Level, body: impl FnOnce() -> T) -> (T, String) {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        tracing::subscriber::set_global_default(Capture).expect("no other global subscriber");
    });

    let buffer = Arc::new(Mutex::new(String::new()));
    SINK.with_borrow_mut(|sink| *sink = Some((level, Arc::clone(&buffer))));
    let value = body();
    SINK.with_borrow_mut(|sink| *sink = None);

    let logged = buffer.lock().expect("log buffer").clone();
    (value, logged)
}
