//! The analysis thread: sole owner of the rubydex `Graph`.
//!
//! # Containment rule
//!
//! This is the only module allowed to name a rubydex type. rubydex is pre-1.0 with one
//! published release; when its API churns the blast radius must be one directory.
//!
//! # Threading
//!
//! rubydex is synchronous and `Graph` has a single `&mut` writer, so there is exactly one
//! analysis thread and every request serialises behind it. That is affordable because rubydex
//! is fast, but it makes debouncing and cancellation load-bearing rather than optional.
//!
//! # What is here and what is next door
//!
//! This file is the thread: the tasks it takes, the graph and buffers it owns, the indexing
//! lifecycle that keeps them current, the diagnostics it pushes without being asked, and the
//! run loop that orders all of it. The LSP request layer — every handler, the method table, and
//! the bulkhead around answering — is [`requests`], and the pass that writes the workspace's own
//! RBS is [`synthesize`]. Both are sibling files holding an `impl Analysis`, which Rust allows
//! because privacy is by module *descendant*.
//!
//! The same rule is why `testing` is here and not at the crate root: the `Harness` every
//! end-to-end test in the crate drives the server through holds an `Analysis`, and a child of
//! this module sees every private field of one. The tests left below are the thread's own — the
//! buffer against the disk, the skip list, the gem stepper, what gets published and whose code
//! it is; everything else went to the module whose invariant it would break.

pub mod annotations;
pub mod code_actions;
pub mod completion;
pub mod cursor;
pub mod diagnostics;
mod environment;
// The two lists a project may replace, and the only things this private module publishes.
//
// They are *defaults a user is shown*: the VS Code manifest documents both, and a default
// written out in JSON with nothing holding it to the code is documentation that rots — which is
// how `ya-lsp.logLevel` shipped two releases saying `warn`. `tests/vscode_manifest.rs` reads
// them from here so the manifest and the rule cannot drift.
pub use environment::{MIGRATION_PAIR, TEST_TREES};
pub mod erb;
pub mod hierarchy;
pub mod highlight;
pub mod hints;
pub mod hover;
pub mod indexer;
pub mod locator;
pub mod position;
pub mod progress;
pub mod ranges;
pub mod references;
pub mod rename;
pub mod render;
mod requests;
pub mod requires;
pub mod scopes;
pub mod search;
pub mod signature_help;
pub mod signatures;
pub mod structs;
pub mod symbols;
mod synthesize;
pub mod synthesized;
pub mod tokens;
pub mod types;
pub mod views;

use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_server::{Message, Request, RequestId, Response};
use lsp_types::{ClientCapabilities, DiagnosticSeverity};
use rubydex::{
    indexing::LanguageId,
    model::{graph::Graph, ids::UriId},
    resolution::Resolver,
};

use crate::messages;
use crate::workspace::{DocUri, Workspace, gems, rails};
use position::{PositionEncoding, Rebase, TextDocument};
use progress::Progress;
use synthesized::Synthesized;

/// How long to wait for typing to settle before the graph catches up with it.
///
/// What waits is all of it: the buffer's own index, the generator pass,
/// `Resolver::resolve` — which links declarations across the whole graph — and the diagnostics
/// push. A keystroke leaves its edit in `pending_index` and arms this timer; `Analysis::settle`
/// is the one place any of that happens. rubydex's resolver is incremental
/// (`Graph::take_pending_work`), so a settle coalesces a burst rather than repeating
/// whole-graph work.
///
/// **Why 500 ms, and why it is not freely tunable.** The timer is renewed by every edit, so it
/// fires only when the typist stops for longer than it, and the cost of a settle firing
/// mid-burst is `debounce + settle - gap`. Two values drive that to zero: one small enough that
/// the settle finishes inside the gap, and one larger than the gap itself. 150 ms is the first
/// and it stops working once the index is deferred, because a mid-burst settle is then the
/// *only* thing a completion waits for. Measured at
/// a 250 ms pace, a keystroke costs 2, 5, 4, 53, 11 and 261 ms at 150 on lobsters, chatwoot,
/// mastodon, forem, solidus and discourse — and 2, 5, 4, 9, 3 and 24 at 500. **A debounce equal
/// to the pace is worse than either**, because the settle then fires precisely between two
/// keystrokes: the first five measure 67, 116, 128, 167 and 130.
///
/// **Scaling it per workspace was built, measured and abandoned.** The
/// lower regime is worth nothing in latency — lobsters and chatwoot are identical at both values
/// — so its only value was keeping diagnostics prompt on small workspaces, and no stable signal
/// picks it: a settle costs what the *last edit* made it cost, and the cheap ones during a member
/// burst pull the estimate under any threshold within a keystroke or two.
///
/// So the trade is made once and stated: **diagnostics appear 350 ms later after the typist
/// stops**, in exchange for completion during typing that is 8 to 22 times faster and never
/// slower.
const RESOLVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// [`RESOLVE_DEBOUNCE`], with the override the benchmarks set it through.
///
/// Not a `ya-lsp.toml` key: the constant is calibrated against a measurement rather than a
/// preference, and a project that set it wrong would make every completion in it slower without
/// changing a single answer.
fn resolve_debounce() -> Duration {
    std::env::var("YA_LSP_RESOLVE_DEBOUNCE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(RESOLVE_DEBOUNCE, Duration::from_millis)
}

/// How many gem files one background step indexes before handing the thread back.
///
/// The cost of a step is not the indexing — that is a few milliseconds — but the *resolve* the
/// next request has to run over what the step added, which is what the user actually waits for.
/// Measured on a real Rails bundle (151 gems, 6159 files), asking for a documentSymbol as fast
/// as the server would answer, p90 latency during the index runs 94 ms at 50 files per step,
/// 117 ms at 100, 127 ms at 200 and 333 ms at 400, while the time to finish indexing when
/// nobody is asking is 0.25 s either way. 100 is where the worst case is still interactive and
/// the idle path pays nothing for it.
const GEM_FILES_PER_STEP: usize = 100;

/// How many symbols one `workspace/symbol` answers with.
///
/// A ceiling is not optional at gem scale: rubydex's fuzzy match is a subsequence test, so a
/// two-letter query matches a large fraction of a Rails bundle's declarations and the request
/// arrives on every keystroke. What the number buys is only how far down the ranking a client
/// that does its own filtering can still reach — VS Code shows a few dozen rows — so the cost
/// of raising it is paid on every keystroke and the benefit is invisible.
const MAX_WORKSPACE_SYMBOLS: usize = 256;

/// How many references one `textDocument/references` answers with.
///
/// Unlike the symbol ceiling this one should never be reached: it exists so that a name-based
/// match on `call` in a workspace of tens of thousands of files cannot hand the editor a
/// multi-megabyte response. Reaching it is logged, because a silently truncated "find all
/// references" is a wrong answer that looks like a right one.
const MAX_REFERENCES: usize = 10_000;

/// How many suggestions one `textDocument/completion` answers with.
///
/// Unlike `MAX_WORKSPACE_SYMBOLS`, this one is not a latency control, and measuring said so:
/// sweeping it over 128 / 256 / 512 / 1024 on a Rails app moved the worst request by about a
/// millisecond and typing latency not at all, because the cost is the graph work in front of it
/// and not the rows behind it. What it bounds is the *response* — a thousand items is around
/// 200 KB of JSON on every keystroke — and how deep a client that filters the list itself can
/// reach before the `isIncomplete` flag makes it ask again. Since reaching it is now the only
/// thing that sets that flag, it also decides how often the client has to come back at all.
const MAX_COMPLETION_ITEMS: usize = 512;

/// How many candidates the **name-based** list may have before it answers with nothing at all.
///
/// The admission ceiling, and a separate question from both of the others. [`MAX_COMPLETION_ITEMS`]
/// bounds the *response* of a list this server believes in; [`MAX_UNTYPED_COMPLETION_ITEMS`] bounds
/// how many rows of a **guess** are worth reading; this one decides whether the guess is worth
/// making. `completion::by_name` produces every method name in the project that matches what has
/// been typed, attached to no class in particular, and above this many of them there is no reason
/// to believe the ranking put the right one near the top.
///
/// **Measured by `make audit-prefix`, and it is the reason there are two ceilings rather than one.**
/// Following 335 untyped cursors over five corpora outwards through their own word, against a
/// binary built with this raised, the word the file wrote sits inside the first
/// [`MAX_UNTYPED_COMPLETION_ITEMS`] rows of the server's own order:
///
/// | candidates | lists | word in the first 128 |
/// |---|---|---|
/// | 1–128 | 159 | 159 |
/// | 129–256 | 131 | 131 |
/// | 257–512 | 140 | 140 |
/// | 513–1,024 | 157 | 152 |
/// | 1,025+ | 156 | 141 |
///
/// So 512 is the largest value with **no measured loss at all**, and it is where the table first
/// stops being perfect rather than a round number. Above it the ranking starts to slip — gradually,
/// which is why this is a judgement and not a cliff.
///
/// The empty prefix is untouched by any of this: the candidate set there is the project's whole
/// name universe, 26,073 to 45,953 over the five corpora, far above every row of that table. That
/// case declines, which is the defect this pair of ceilings exists for.
const MAX_UNTYPED_CANDIDATES: usize = 512;

/// How many rows of the **name-based** list are worth reading, once it is worth offering.
///
/// The display ceiling. [`MAX_UNTYPED_CANDIDATES`] decides whether to answer; this decides how much
/// of the answer to send, and the two are split because the measurement says the ranking is good
/// and the row count is not: a guess of 512 candidates holds the word in its first 128 rows every
/// time, and nobody reads 512 rows of anything.
///
/// **Nothing is offered for the first three keystrokes**, at any of these values — over five corpora
/// the share of untyped cursors whose candidates fit under 512 runs 0% at two characters, 45% at
/// three, 74% at four and 95% at five. So for the commonest case this is still a flat decline, and
/// the bounded list only exists mid-word.
///
/// Truncating here is honest where truncating at an **empty** prefix would not be: `tier` is 1 for
/// every row and `length` is 0 when nothing has been typed, leaving `Locality` — which directory
/// the name lives in — as the only live term, so the rows kept would be arbitrary. From three
/// characters on, `tier` and `length` are both live and rank p90 is 112 even in thousand-item
/// lists.
const MAX_UNTYPED_COMPLETION_ITEMS: usize = 128;

/// How many subtypes one `typeHierarchy/subtypes` answers with.
///
/// Finding them is free — rubydex maintains the reverse index as it linearizes, so the lookup is
/// the same work for three descendants as for thirty thousand. What costs is the *rows*: each has
/// to be placed in its own file, which is a read and a line index per file. So the cap bounds the
/// response *and* the only part of this request that scales.
///
/// The number is the measured worst *legitimate* question plus headroom rather than a guess.
/// `StandardError`'s 458 descendants in Ruby's own signatures is a real question with a real
/// answer, and is what ruled out 512; `Object`, `Kernel` and `BasicObject` come to about 1,980
/// each, and every namespace in a mid-sized Ruby project is far below that. All of those fit.
/// What does not is a Rails bundle, where the three roots are an order of magnitude larger and
/// the answer would be megabytes and a quarter of a second — the case this exists for, and
/// reaching it says so out loud.
const MAX_SUBTYPES: usize = 2048;

/// How many callers one `callHierarchy/incomingCalls` answers with.
///
/// The rows are buckets rather than call sites — one per method that calls, however many times it
/// calls — and drawing one reads the file it lives in, which is `MAX_SUBTYPES`' cost exactly. What
/// differs is how it is reached: a wide type hierarchy takes a deliberate click near the root of
/// the object model, and a wide *call* hierarchy takes a method named `call`, which is ordinary.
///
/// So the number is the measured worst *legitimate* question plus headroom, as `MAX_SUBTYPES` is.
/// Measured 2026-09-11 over five real applications at their pinned commits, asking for the callers
/// of the ten commonest method names in each: the widest answer anywhere was forem's `id` at
/// **1,475 callers over 1,103 files, in 64 ms**, with chatwoot's `id` at 1,202 and mastodon's at
/// 1,057. Nothing else on any corpus passed 716, and lobsters' widest was 97. Every one of those
/// is a real question with a real answer and fits; what does not is a workspace several times the
/// size of these, where the response rather than the latency is what fails.
const MAX_INCOMING_CALLS: usize = 2048;

/// The `$/progress` token for the gem index. A fixed string is fine — only one runs at a time.
const GEM_PROGRESS_TOKEN: &str = "ya-lsp/index-gems";

/// Work sent from the main loop to the analysis thread.
#[derive(Debug)]
pub enum Task {
    DidOpen {
        uri: DocUri,
        text: String,
        /// The editor's version of this buffer, echoed back on `publishDiagnostics` so the
        /// client can discard results for text it has already typed past.
        version: Option<i32>,
    },
    DidChange {
        uri: DocUri,
        /// In the order the client sent them. Each range is expressed against the text the
        /// previous change left behind, so they cannot be reordered or coalesced.
        changes: Vec<TextChange>,
        version: Option<i32>,
    },
    DidClose {
        uri: DocUri,
    },
    DidSave {
        uri: DocUri,
    },
    Request(Request),
    /// Paths a `workspace/didChangeWatchedFiles` named, deduplicated, with `ya-lsp.toml`
    /// already split off — that one is [`Task::ReloadConfig`], which is a different order of
    /// magnitude of work.
    ///
    /// No change *kind* is carried. The client sends created, changed and deleted, and all
    /// three are answered by looking: a file that is there is indexed and a file that is not is
    /// dropped. That is not a shortcut — during a branch switch the events and the filesystem
    /// genuinely disagree, and a `Deleted` for a path git has already written back would
    /// otherwise drop a file that exists.
    WatchedFiles {
        uris: Vec<DocUri>,
    },
    /// `ya-lsp.toml` changed on disk.
    ReloadConfig,
    /// The client changed the settings it sent as `initializationOptions`.
    ///
    /// Carried rather than re-read, because these never touch the filesystem: they are the
    /// editor's own settings, and the editor is the only thing that knows them.
    ChangeConfig {
        options: Option<serde_json::Value>,
    },
    /// Kill the analysis thread, from a test.
    ///
    /// A stand-in in the same sense [`crash_the_next_resolve_if_asked`] is, and for a narrower
    /// reason: the panic [`AnalysisHandle::join`] reports is by construction the *unforeseen*
    /// one — everything foreseen here is either handled or caught in [`Analysis::resolve`] — so
    /// there is no input that provokes it and nothing to reproduce. What the arm is worth
    /// pinning is ya-lsp's half: that a thread which died is noticed at all rather than joined
    /// silently, which is the difference between one line in the log and a server that answers
    /// nothing for the rest of the session with nothing said anywhere.
    #[cfg(test)]
    Panic,
}

impl Task {
    /// What one line of the log calls this task, and the one thing it is about.
    ///
    /// The names are the client's own — `didChange`, not `DidChange` — because the reader who
    /// needs this line has the editor's LSP trace open beside it and is matching the two up.
    fn describe(&self) -> (&'static str, &str) {
        match self {
            Task::DidOpen { uri, .. } => ("didOpen", uri.as_str()),
            Task::DidChange { uri, .. } => ("didChange", uri.as_str()),
            Task::DidClose { uri } => ("didClose", uri.as_str()),
            Task::DidSave { uri } => ("didSave", uri.as_str()),
            // The count goes on the arm that handles it rather than here: a branch switch names
            // thousands of paths, and a number is not a `&str` without allocating one per
            // notification.
            Task::WatchedFiles { .. } => ("didChangeWatchedFiles", "-"),
            Task::ChangeConfig { .. } => ("didChangeConfiguration", "-"),
            Task::ReloadConfig => ("ya-lsp.toml", "-"),
            Task::Request(request) => ("request", request.method.as_str()),
            #[cfg(test)]
            Task::Panic => ("panic", "-"),
        }
    }
}

/// One content change from `textDocument/didChange`.
#[derive(Debug)]
pub struct TextChange {
    /// `None` when the client sent the whole buffer instead of a range.
    pub range: Option<lsp_types::Range>,
    pub text: String,
}

/// How to ask the client for the files outside the workspace, given their URI prefixes.
///
/// A function rather than the table it reads. The table is `server::capabilities`, which already
/// reads `analysis::tokens` and `analysis::position`, so naming its types here would close a cycle
/// between capability negotiation and the thread that answers — and the only thing this side has
/// to know is the trade: hand over the prefixes, get back registrations to send. An empty answer
/// for a non-empty list of prefixes means the client takes no dynamic registration, which
/// [`Analysis::register_documents`] says once and then stops saying.
pub type DocumentRegistrar = Box<dyn Fn(&[String]) -> Vec<lsp_types::Registration> + Send>;

/// What a client that takes no dynamic registration gets: today's behaviour, unchanged.
#[must_use]
pub fn no_document_registrar() -> DocumentRegistrar {
    Box::new(|_| Vec::new())
}

/// The parts of the client's capabilities that change what we are allowed to send back.
///
/// Both of these default to `false` — the pre-3.10 shapes — because a client that does not
/// advertise a capability may not merely ignore the richer response; it can fail to parse it.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientSupport {
    /// `textDocument/documentSymbol` may answer with a nested tree rather than a flat list.
    pub hierarchical_symbols: bool,
    /// `textDocument/definition` may answer with `LocationLink`s, which carry the origin span
    /// and let the editor preview the target's name separately from its body.
    pub definition_links: bool,
    /// `window/workDoneProgress` — the client will render a progress stream if we open one.
    /// Without it, gem indexing has to happen silently.
    pub work_done_progress: bool,
    /// A `WorkspaceEdit` may be sent as `documentChanges` rather than as the older `changes`
    /// map. Worth negotiating rather than always sending the older shape, because the richer
    /// one carries the version of each file the edit was computed against — so a client can
    /// reject a rename the user has typed past instead of applying it to moved text.
    pub versioned_edits: bool,
    /// `workspace/inlayHint/refresh` — the client will ask for its inlay hints again if told to.
    ///
    /// The only capability here that is about an answer going *stale* rather than about the
    /// shape of one. A client re-asks for hints when the document changes and when the window
    /// scrolls, and neither happens while a user waits for a cold index to finish — so the file
    /// they opened would keep the margin it had before the types arrived, for as long as they
    /// left it alone. Nothing is lost where the client says no: the hints are right from the
    /// next keystroke, as they were before this existed.
    pub hint_refresh: bool,
}

impl ClientSupport {
    #[must_use]
    pub fn negotiate(capabilities: &ClientCapabilities) -> Self {
        let text_document = capabilities.text_document.as_ref();
        Self {
            hierarchical_symbols: text_document
                .and_then(|it| it.document_symbol.as_ref())
                .and_then(|it| it.hierarchical_document_symbol_support)
                .unwrap_or(false),
            definition_links: text_document
                .and_then(|it| it.definition.as_ref())
                .and_then(|it| it.link_support)
                .unwrap_or(false),
            work_done_progress: capabilities
                .window
                .as_ref()
                .and_then(|it| it.work_done_progress)
                .unwrap_or(false),
            versioned_edits: capabilities
                .workspace
                .as_ref()
                .and_then(|it| it.workspace_edit.as_ref())
                .and_then(|it| it.document_changes)
                .unwrap_or(false),
            hint_refresh: capabilities
                .workspace
                .as_ref()
                .and_then(|it| it.inlay_hint.as_ref())
                .and_then(|it| it.refresh_support)
                .unwrap_or(false),
        }
    }
}

/// Request ids the client has cancelled.
///
/// Owned by the main thread and read by the analysis thread. It has to be shared state rather
/// than another channel message: tasks are processed in order, so a `$/cancelRequest` sent
/// through the same queue would always arrive *after* the request it cancels had been answered.
#[derive(Debug, Clone, Default)]
pub struct Cancellations(Arc<Mutex<std::collections::HashSet<RequestId>>>);

impl Cancellations {
    pub fn cancel(&self, id: RequestId) {
        self.lock().insert(id);
    }

    /// Consume the cancellation for `id`, if any.
    pub(crate) fn take(&self, id: &RequestId) -> bool {
        self.lock().remove(id)
    }

    /// A request that completed normally leaves no cancellation behind.
    fn forget(&self, id: &RequestId) {
        self.lock().remove(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<RequestId>> {
        // A panic in another thread must not take the server down; the set is plain data and
        // is safe to keep using.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Handle to a running analysis thread.
#[derive(Debug)]
pub struct AnalysisHandle {
    sender: Sender<Task>,
    thread: std::thread::JoinHandle<()>,
}

impl AnalysisHandle {
    pub fn sender(&self) -> &Sender<Task> {
        &self.sender
    }

    /// Close the queue and wait for in-flight work to finish.
    pub fn join(self) {
        drop(self.sender);
        if self.thread.join().is_err() {
            tracing::error!("analysis thread panicked");
        }
    }
}

/// Start the analysis thread.
///
/// `outgoing` is a clone of the LSP connection's sender: the analysis thread writes responses
/// and notifications straight to the transport so the main loop never has to poll two channels.
pub fn spawn(
    workspace: Workspace,
    encoding: PositionEncoding,
    client: ClientSupport,
    documents: DocumentRegistrar,
    outgoing: Sender<Message>,
    cancellations: Cancellations,
    logging: crate::logging::Reload,
) -> AnalysisHandle {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let thread = std::thread::Builder::new()
        .name("ya-lsp-analysis".to_owned())
        .spawn(move || {
            let mut analysis = Analysis::new(
                workspace,
                encoding,
                client,
                documents,
                outgoing,
                cancellations,
                logging,
            );
            analysis.index_workspace();
            // The startup index is already resolved, so publish now rather than waiting for the
            // first edit: a project with a syntax error should light up before anyone types.
            analysis.publish_diagnostics();
            // Queued, not indexed: the gems go in during the run loop's idle time, so the
            // workspace is answering questions while the bundle is still arriving.
            analysis.queue_background_indexing();
            analysis.run(&receiver);
        })
        .expect("failed to spawn analysis thread");

    AnalysisHandle { sender, thread }
}

// The real trigger for the recovery below is rubydex's, and it needs a couple of hundred files
// of a real project to fire — solargraph v0.58.2, minus its own
// `lib/solargraph/yard_map/to_method.rb` — which is not something to vendor into this repository
// to hold one test up. So what is pinned here is ya-lsp's half of it, which is the half ya-lsp
// can be wrong about: that the panic is caught, that the user is told, that the graph comes
// back, and that a rebuild which crashes again stops rather than recurring.
#[cfg(test)]
thread_local! {
    /// How many of the next resolves a test has asked to crash.
    static RESOLVES_TO_CRASH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn crash_the_next_resolve_if_asked() {
    let remaining = RESOLVES_TO_CRASH.get();
    if remaining > 0 {
        RESOLVES_TO_CRASH.set(remaining - 1);
        panic!("a stand-in for rubydex resolution.rs:748");
    }
}

// The same shape for the request seam, and it is a stand-in for the same reason.
//
// The fixture that provoked it was real Ruby — a class reopened under a constant that aliases
// it, with the cursor on the `def` — and upstream's `ab88ef1` fixed the `graph.rs:452` unwraps behind
// it. `a_constant_alias_reopened_under_its_alias_answers_rather_than_crashing` still writes
// that Ruby and asserts the answer, so the fix is pinned; what has no input any more is the
// *seam*, and the seam is what has to keep working. `create_declaration`'s two unwraps are
// still on upstream's `main` and every handler below `dispatch` reaches the graph, so what is
// unavailable is a reproduction rather than the hazard.
#[cfg(test)]
thread_local! {
    /// How many of the next requests a test has asked to crash.
    static REQUESTS_TO_CRASH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn crash_the_next_request_if_asked() {
    let remaining = REQUESTS_TO_CRASH.get();
    if remaining > 0 {
        REQUESTS_TO_CRASH.set(remaining - 1);
        panic!("a stand-in for a handler that panics under rubydex");
    }
}

/// A buffer the editor has open, which shadows whatever is on disk.
///
/// We keep our own copy of the text because rubydex's `Document` exposes a `line_index()` but
/// not the source, and incremental sync and cursor context both need it.
#[derive(Debug)]
struct OpenDocument {
    text: TextDocument,
    version: Option<i32>,
}

/// Gem files waiting to be indexed in the background, plus what the editor is being told.
#[derive(Debug)]
struct GemIndexing {
    /// Reversed, so taking the next batch off the end is a pointer move rather than a shift.
    remaining: Vec<PathBuf>,
    total: usize,
    gems: usize,
    /// How many of `total` are RBS signatures rather than gem sources. Reported separately
    /// because "indexed 41,000 files from 151 gems" is a different claim from "and Ruby's own
    /// core", and the second one is the one that breaks silently.
    signature_files: usize,
    progress: Option<Progress>,
    started: Instant,
}

struct Analysis {
    graph: Graph,
    /// What RBS says the methods in the graph return.
    ///
    /// Beside the graph rather than in it, because rubydex models no types at all: see
    /// [`types`]. Filled as signature files are indexed, and thrown away with the graph.
    types: types::Types,
    /// Where the declarations ya-lsp wrote itself were really declared.
    ///
    /// Beside the graph for the reason the type table is. The mapping exists before any
    /// generator does, because a server that types `@story.title` and then jumps to a file the
    /// user does not have is worse than one that does not type it. See [`synthesized`].
    synthesized: Synthesized,
    /// What a template can call, rebuilt by the same pass and for the same
    /// reason the RBS is. Nothing in it is a declaration — see [`views`].
    views: views::Views,
    /// Which source files [`Analysis::synthesize`] generated from, the last time it ran.
    ///
    /// The pass's own bookkeeping and not the side table's: a source that stops declaring
    /// anything sends no notification, so the only way to be sure nothing stale is left is to
    /// compare what this pass wrote last time against what it just wrote. Scoped to this pass
    /// deliberately — the table is shared, and pruning "everything I did not just write" would
    /// delete anything else that ever records into it.
    generated: HashSet<String>,
    /// The projection [`Analysis::synthesize`] last ran the generators on, for the pass gate.
    ///
    /// `None` until the first pass has run, which is the honest answer for "would it write the
    /// same thing again": nothing is known about a pass that has not happened.
    generated_from: Option<crate::knowledge::Context>,
    /// What every document the walk visited contributed to `generated_from`.
    ///
    /// **Two jobs, one map.** As the *gate's evidence*: comparing the whole `Context` can only
    /// be asked after the walk, because the walk is the only thing that answers it, so a
    /// keystroke re-derives the contribution of the file it touched and compares that one — the
    /// other twenty-five thousand are known not to have moved because nothing else was indexed.
    /// As the *walk's memo*: that same sentence read the other way round, so the next walk takes
    /// the held value for every document rubydex has not re-indexed rather than projecting it
    /// again. On discourse that is 181 ms of a 247 ms walk.
    ///
    /// A document the walk does not visit has **no entry**, which is not the same as an entry
    /// for an empty contribution — see `Analysis::contribution`. `Analysis::walk` owns the
    /// invalidation, and it is the same `touched`/`touched_all` pair the gate narrows on.
    contributions: HashMap<rubydex::model::ids::UriId, crate::knowledge::Contribution>,
    /// Every body of knowledge this build has, and the only place one is named.
    ///
    /// **Not in the pass**, which is the whole of the seam: `synthesize` reads this and never a
    /// module, so a build that registers nothing still compiles, runs and declares nothing. See
    /// [`crate::knowledge`].
    knowledge: crate::knowledge::Registry,
    /// The text each open buffer was **last handed to the indexer** as, which is what makes a
    /// deferred index answerable: rubydex's `Document` keeps a `content_hash` and a `LineIndex`
    /// and not the source, so the graph cannot be asked what its own offsets index.
    ///
    /// Written by `index_buffer`, which is also where the text may not be the buffer's: a
    /// template is indexed as `erb::ruby_view` and an `.rbs` as `signatures::without_interfaces`.
    /// Recording what was *actually* indexed is what keeps the map honest for both — an `.rbs`
    /// simply produces a rebase that refuses nearly everything, which falls back rather than
    /// answering from a coordinate system it does not share.
    indexed_text: HashMap<DocUri, String>,
    /// Which documents have been re-indexed since that pass, where exactly one is known.
    touched: HashSet<String>,
    /// How many times the generators have actually run, rather than been gated out.
    ///
    /// The gate's own instrument. Its whole claim is that it changes no answer, so the
    /// only way to see it working at all is to count the passes it prevented.
    passes: u64,
    /// How many times the **walk** has run, which is the same instrument one level down.
    ///
    /// `passes` cannot see this: the outer gate stops the generators and leaves the projection
    /// they are handed being rebuilt in full to decide that. A pass that is gated out either
    /// walks or does not, and only a counter says which.
    walks: u64,

    /// Every file that pass read, and what it looked like on disk when it did.
    ///
    /// The half of the pass gate that needs no notification: the pass claims the answer is a
    /// function of what is on disk, so the gate has to look at the disk.
    stamps: Vec<(std::path::PathBuf, Option<(std::time::SystemTime, u64)>)>,
    /// Whether something was indexed that reported no document.
    ///
    /// Every bulk route sets it — the workspace walk, a gem batch, the file watcher, a rebuild
    /// — which is what keeps the gate a *narrowing* of one path rather than a claim about all
    /// of them. Only [`Analysis::index_buffer`] names its document, and that is the keystroke
    /// path the gate is for.
    touched_all: bool,
    open: HashMap<DocUri, OpenDocument>,
    encoding: PositionEncoding,
    client: ClientSupport,
    /// How to claim the files the server has answers about and the client's own selector did not.
    ///
    /// Held rather than called at startup because its one argument — the gem roots, Ruby's own
    /// library, the RBS root — does not exist until the bundle has been discovered, which happens
    /// on this thread and not in the handshake.
    documents: DocumentRegistrar,
    /// The document registrations currently live in the client, for unregistering them again.
    ///
    /// A reload can move every one of the prefixes — `[gems] enabled`, an `[rbs] path`, a
    /// workspace root — and re-registering an id the client already holds **replaces its record
    /// of the registration without disposing the provider behind it**, which leaves the old
    /// selector answering beside the new one. So the old ids go first, by name, which is why they
    /// are fixed strings rather than generated.
    documents_live: Vec<lsp_types::Unregistration>,
    /// Whether the client has already been told it will not be asked again.
    ///
    /// Said once per process, not once per reload: a client that declines dynamic registration
    /// declines it for the session, and a `ya-lsp.toml` saved five times would otherwise repeat
    /// the same sentence five times.
    documents_declined: bool,
    workspace: Workspace,
    outgoing: Sender<Message>,
    cancellations: Cancellations,
    /// Whether the graph holds indexed-but-unresolved work. Separate from `resolve_at` because
    /// the two answer different questions: this one is "would an answer be stale", which a
    /// request has to check, while `resolve_at` is only "when should we do it unprompted".
    /// Background gem indexing sets this without touching the timer, so a burst of gem work
    /// cannot keep pushing the user's own diagnostics further out.
    dirty: bool,
    /// When the debounced global resolve is due. `None` means no resolve is scheduled.
    resolve_at: Option<Instant>,
    /// Buffers whose edit has been applied and whose index has not run yet.
    ///
    /// A `didChange` does two things and only the second is expensive: it applies the edit to
    /// `open`, which is microseconds, and it puts the document into the graph, which is 265–305
    /// ms for `app/models/user.rb` on discourse because rubydex's invalidation cascades over
    /// every declaration that names it. The four requests `needs_the_graph` exempts read the
    /// buffer and never the graph, and an editor sends `semanticTokens/full` after every
    /// keystroke — so on the loop as it stood they waited for an index they do not read.
    ///
    /// Drained by [`Analysis::settle`] and nowhere else, and **that only works with two things
    /// under it**: the map, so a completion answers from the last settled graph instead of
    /// waiting for this, and [`RESOLVE_DEBOUNCE`] at 500 ms, so the settle falls outside the
    /// burst rather than into it. Without both it is a 2x regression on discourse — 155–198 ms
    /// a completion becomes 324–397 — because a 410 ms settle armed at a 150 ms debounce is
    /// still in flight when the next request arrives and cannot be interrupted. An eager drain
    /// at the loop's next idle moment is the other design and is **worse than either**: it
    /// leaves the graph holding a class with no members at all rather than a stale one.
    pending_index: Vec<DocUri>,
    /// Gem files still waiting to be indexed in the background.
    gem_work: Option<GemIndexing>,
    /// How many of the user's own files the index holds, against `index.max_files`.
    ///
    /// The walk's cap has to keep applying after the walk: a watcher can add files the walk
    /// stopped before, and the cap exists because a pathological repository exists. Counted as
    /// files come and go rather than recomputed, because the graph has no cheap answer — by the
    /// time a bundle is in, asking it means walking tens of thousands of documents, and a
    /// branch switch would ask once per changed file.
    workspace_files: usize,
    /// Documents whose last indexing attempt crashed rubydex — the bulkhead's skip list.
    ///
    /// The recovery a contained index panic wants is *not* [`Analysis::rebuild`]. `resolve`
    /// rebuilds because a half-linked graph is in an unknown state; a contained index panic
    /// leaves a known one — the graph minus one document — and the file is still on disk, so a
    /// rebuild re-reads it and trips over it again. So the file is remembered instead, and this
    /// set is the one thing `rebuild` deliberately does **not** clear.
    ///
    /// A document is on it exactly while the last attempt to index it panicked, which is what
    /// makes it self-clearing: every route that indexes because the text may have moved —
    /// `didOpen`, `didChange`, `didClose`, the watcher — simply tries again and removes the
    /// entry when it works. The two routes that re-read text which has *not* moved consult it
    /// instead: the workspace walk, which a rebuild runs again, and the gem batch, which a
    /// rebuild re-queues.
    skipped: HashSet<DocUri>,
    /// Whether a rebuild after a resolver panic is already under way.
    ///
    /// Guards the one recursion that matters: the rebuild indexes the workspace, indexing
    /// resolves, and a rebuild that panics again would rebuild again forever.
    recovering: bool,
    /// Whether the user has already been told the index is full, since the last reload.
    ///
    /// Said once. A `git checkout` in a workspace that is over the cap would otherwise raise
    /// the same notification on every branch switch for the life of the process.
    index_full_reported: bool,
    /// Every workspace document URI starts with this. Used to keep gem diagnostics off the
    /// screen without parsing a URL per diagnostic.
    workspace_prefix: String,
    /// The project's own code that is **not** under the root: `[index] load_paths` entries
    /// pointing outside it, as directory URI prefixes.
    ///
    /// A second way to be the user's own code, and the only one there is. A monorepo whose
    /// applications share a tree beside them names it here, and every surface that asks "may I
    /// act on this file?" has to say yes: without it a shared model gets no diagnostics, is not
    /// offered for rename, and ranks as somebody else's code in search — in a directory the
    /// project wrote down by hand. The gem roots stay out of it, which is the whole reason this
    /// is a separate list rather than a wider `workspace_prefix`.
    own_prefixes: Vec<String>,
    /// URI prefixes of everything indexed that is not the user's code: the gem roots, and the
    /// RBS root Ruby's own signatures came from.
    ///
    /// The workspace prefix alone is not enough: a *vendored* bundle lives at
    /// `vendor/bundle/ruby/<abi>` — inside the workspace root by construction — so every gem in
    /// it would otherwise pass the workspace test and publish diagnostics nobody can fix. The
    /// same goes for an `[rbs] path` pointing inside the project. Kept per root rather than per
    /// gem so the test stays a handful of comparisons instead of one per gem in the bundle.
    foreign_prefixes: Vec<String>,
    /// URI prefixes of the `app/` directory of every gem that has one — a Rails engine's Ruby.
    ///
    /// A *second* list beside `foreign_prefixes` rather than a hole in it, because the two
    /// answer different questions and only one of them has changed. `is_own_code` still says no
    /// to every one of these: nobody can fix a warning inside someone else's engine or rename a
    /// method in one, which is the reason that test exists. What an engine's `app/` is good for
    /// is the thing a generator wants — `has_many :attachments` on `ActiveStorage::Blob` is a
    /// member of a class the user really does name — so [`Analysis::walk`] asks
    /// [`Analysis::is_generator_source`] and every other caller goes on asking `is_own_code`.
    ///
    /// Filled from [`gems::Gems::engine_paths`] rather than guessed from a path, so the two
    /// cannot disagree about which directories were walked.
    engine_prefixes: Vec<String>,
    /// The load path as document-URI prefixes, in the order `require` searches them.
    ///
    /// A **third** list beside the two above, and the only one that is an order rather than a
    /// set: the other two ask *is this document foreign*, and this one asks *which of two
    /// copies of one file would this project load*. `locator::places` is the caller, and a
    /// bundle that pins `cgi` is the case — the gem's `lib/cgi/escape.rb` comes first and the
    /// copy inside Ruby is one `require` will never reach.
    ///
    /// From `Workspace::load_paths`, which is the same list `require` resolution reads, so the
    /// two cannot come apart about which copy wins. Empty until the bundle is discovered, which
    /// is exactly the window in which there is no second copy to be wrong about.
    load_prefixes: Vec<String>,
    /// The last non-empty diagnostic set we sent per URI.
    ///
    /// `publishDiagnostics` is stateful: whatever was last sent for a URI stays on screen until
    /// something else is sent for it. Keeping the last publish lets us send only what changed —
    /// and, just as importantly, an explicit empty set for a URI whose problems went away.
    published: HashMap<DocUri, Vec<lsp_types::Diagnostic>>,
    /// The one handle that can re-point the log, held because `[log]` is re-read here.
    ///
    /// A `ya-lsp.toml` change arrives on this thread, so this is the thread that has to apply
    /// it — otherwise every setting in the file reloads except the one that says what to write
    /// down about the reload.
    logging: crate::logging::Reload,
}

impl Analysis {
    /// Every body of knowledge this build has, in the order they run.
    ///
    /// **The one line core says about Rails**, and it is deliberately not in the pass: a build
    /// that registers nothing compiles, runs and declares nothing, which is the property that
    /// makes the seam real rather than asserted. See [`crate::knowledge`].
    /// How many source files every module has read off the disk or out of a buffer and parsed.
    ///
    /// The third instrument beside [`Analysis::passes`] and [`Analysis::walks`], and it is the
    /// modules' own now: each keeps its own memo, so each counts its own reads and this adds
    /// them up. It counts **files and not parses** — one file on three of one module's lists is
    /// one read and up to three readers, which is itself part of the saving.
    #[cfg(test)]
    fn reads(&self) -> u64 {
        let rails = self
            .knowledge
            .of::<crate::knowledge::rails::Rails>()
            .map_or(0, |module| module.reads);
        let annotated = self
            .knowledge
            .of::<crate::knowledge::annotations::Annotations>()
            .map_or(0, |module| module.reads);
        rails + annotated
    }

    fn registered() -> crate::knowledge::Registry {
        crate::knowledge::Registry::new(vec![
            Box::new(crate::knowledge::rails::Rails::default()),
            Box::new(crate::knowledge::annotations::Annotations::default()),
            Box::new(crate::knowledge::structs::Structs),
        ])
    }

    fn new(
        workspace: Workspace,
        encoding: PositionEncoding,
        client: ClientSupport,
        documents: DocumentRegistrar,
        outgoing: Sender<Message>,
        cancellations: Cancellations,
        logging: crate::logging::Reload,
    ) -> Self {
        // A directory URI, so the prefix test cannot match a sibling whose name merely starts
        // the same way (`/app` must not swallow `/app-vendor`).
        let workspace_prefix = DocUri::from_path(workspace.root())
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .unwrap_or_default();

        // Deliberately not calling `Graph::set_encoding`. It only feeds `Graph::encoding()`;
        // `Offset::to_location` ignores it entirely (`Encoding::to_wide` is never called in the
        // crate). Leaving it at the default keeps every offset rubydex hands us in bytes, which
        // is exactly what `position::TextDocument` expects to convert from.
        Self {
            graph: Graph::new(),
            types: types::Types::new(),
            synthesized: Synthesized::new(),
            views: views::Views::default(),
            generated: HashSet::new(),
            generated_from: None,
            walks: 0,
            contributions: HashMap::new(),
            knowledge: Self::registered(),
            indexed_text: HashMap::new(),
            touched: HashSet::new(),
            passes: 0,
            stamps: Vec::new(),
            touched_all: true,
            open: HashMap::new(),
            encoding,
            client,
            documents,
            documents_live: Vec::new(),
            documents_declined: false,
            workspace,
            outgoing,
            cancellations,
            dirty: false,
            foreign_prefixes: Vec::new(),
            engine_prefixes: Vec::new(),
            load_prefixes: Vec::new(),
            resolve_at: None,
            pending_index: Vec::new(),
            gem_work: None,
            workspace_files: 0,
            skipped: HashSet::new(),
            recovering: false,
            index_full_reported: false,
            workspace_prefix,
            // Filled by `index_workspace` rather than here, so that a `ya-lsp.toml` reload —
            // which re-runs that walk and can change `[index] load_paths` — cannot leave it
            // describing the previous configuration. Every other prefix list works this way.
            own_prefixes: Vec::new(),
            published: HashMap::new(),
            logging,
        }
    }

    /// Add the project's load paths that lie outside the workspace root to the walk's result.
    ///
    /// **`[index] load_paths` said "extra roots to index" for four releases and indexed nothing.**
    /// It reached `require` resolution and the prefix list that says which code is the project's
    /// own, and no walk ever visited it — so a monorepo that put its shared tree on the list got
    /// a `require` that resolved to a document the graph did not hold. The list is small, it is
    /// written by hand, and it is the only way to name project code the root does not contain:
    /// `discover` starts at the root and `follow_links` is off, so a sibling directory, or a
    /// symlink to one, is reached by nothing else.
    ///
    /// Taken as `.rb` and `.rbs`, the way every other load path in the crate is walked, rather
    /// than through `index.include`: those globs are written relative to the root and cannot
    /// describe a tree outside it. Inside `index.max_files` all the same — it is a budget over
    /// the project's own code, and this is the project's own code.
    fn collect_external_load_paths(&mut self, discovery: &mut crate::workspace::Discovery) {
        let external = self.workspace.external_load_paths();
        if external.is_empty() {
            return;
        }
        let budget = self.workspace.config().index.max_files;
        let already: std::collections::HashSet<&PathBuf> = discovery.files.iter().collect();
        let mut found: Vec<PathBuf> = gems::source_files(&external)
            .into_iter()
            .filter(|path| !already.contains(path))
            .collect();
        drop(already);

        let room = budget.saturating_sub(discovery.files.len());
        if found.len() > room {
            found.truncate(room);
            discovery.truncated = true;
        }
        tracing::info!(
            "{} file(s) from {} load path(s) outside the workspace root",
            found.len(),
            external.len()
        );
        discovery.files.extend(found);
    }

    /// Index everything in the workspace, once, at startup.
    fn index_workspace(&mut self) {
        let started = Instant::now();
        self.warn_about_unknown_rules();
        self.say_what_the_fences_replaced();
        // Before the walk that reads them, and re-read on every rebuild: `[index] load_paths` is
        // configuration, and a reload arrives here. A directory URI so the prefix test cannot
        // match a sibling whose name merely starts the same way.
        self.own_prefixes = self
            .workspace
            .external_load_paths()
            .iter()
            .filter_map(|path| DocUri::from_path(path))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .collect();
        let mut discovery = self.workspace.discover();
        self.collect_external_load_paths(&mut discovery);

        for problem in &discovery.problems {
            tracing::warn!("{problem}");
            self.show_warning(problem);
        }

        let count = discovery.files.len();
        // What the cap is measured against from here on. `truncated` already said its piece;
        // this is the same budget, carried forward so that a file created later still meets it.
        self.workspace_files = count;
        self.index_full_reported = discovery.truncated;
        // `index.include` covers `**/*.rbs` by default now, so a project that keeps its own
        // `sig/` reaches this path with no configuration at all — this path and no other, and an
        // `interface` there lands its members on `Object` exactly as one in Ruby's own
        // signatures would.
        // The bulkhead's skip list, consulted here because this is one of the two routes that
        // re-reads text which has not moved: whatever crashed the indexer is still on disk, and
        // `rebuild` runs this again. It goes first so that it covers the two pre-passes as well,
        // which are inline and would trip over the same file on the way past.
        let files = self.without_skipped(discovery.files);
        // Two pre-passes over the batch, each taking the files it has to edit before the
        // graph sees them and handing the rest on. Both are the same rule: a file that reaches
        // rubydex unedited is indexed *wrongly*, not merely differently.
        let files = self.index_edited_signatures(files);
        let files = self.index_templates(files);
        let batch = indexer::index_files(&mut self.graph, files);
        let indexed = started.elapsed();

        for error in &batch.errors {
            tracing::warn!("indexing error: {error:?}");
        }
        for uri in batch.skipped {
            self.record_skip(&uri);
        }

        // A whole tree just went into the graph and this route names no document for any of it,
        // which is exactly what `touched_all` means. It was the one bulk route whose docstring
        // claimed it and whose code did not. The cheap gate trusts `touched` to be the whole
        // of what moved, so a bulk route that forgets to say so makes it unsound. In production the two callers happen to cover it
        // (`generated_from` is `None` on startup and `Analysis::rebuild` clears it), which is a
        // property of the callers rather than of this method, and the suite caught it.
        self.touched_all = true;

        self.regenerate();
        let refused = self.skipped.len();
        tracing::info!(
            "indexed {count} files in {indexed:.2?}, {refused} refused, resolved in {:.2?} \
             (total {:.2?})",
            started.elapsed() - indexed,
            started.elapsed()
        );
    }

    /// Main loop with a debounce timer for the settle.
    fn run(&mut self, receiver: &Receiver<Task>) {
        loop {
            // Gem indexing runs only in the gaps. Checking the queue first is what makes
            // "background" true rather than aspirational: with work waiting, the editor's
            // request goes first and the bundle waits.
            //
            // The `continue` is also why an armed `resolve_at` is not looked at until the
            // background work runs out. An edit made during a cold start reaches the *buffer*
            // ahead of the bundle — it is a task, and tasks come first — but its index and the
            // resolve its debounce armed are both the settle's, and the settle waits for the
            // bundle. A request arriving meanwhile is not stalled by that: `defers` answers it
            // over the map, and one that cannot be mapped settles, which is what jumps the queue.
            // The delay is bounded by the background index, and reversing the order would cost a
            // resolve per debounce of typing — a decision for a measurement on a real bundle.
            // `threaded_tests::push_diagnostics_for_an_edit_wait_for_the_background_index` holds
            // the current answer, so changing it fails there and nowhere else.
            if receiver.is_empty() && self.step_gem_indexing() {
                continue;
            }

            let task = match self.resolve_at {
                Some(deadline) => {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        self.settle();
                        continue;
                    };
                    match receiver.recv_timeout(remaining) {
                        Ok(task) => task,
                        Err(RecvTimeoutError::Timeout) => {
                            self.settle();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match receiver.recv() {
                    Ok(task) => task,
                    Err(_) => break,
                },
            };

            self.handle(task);
        }

        // Drain any pending resolution so a shutdown does not leave the graph half-linked; a
        // future on-disk cache would be written from here.
        if self.dirty {
            self.settle();
        }
    }

    fn handle(&mut self, task: Task) {
        // **What arrived that was not a request.** A notification changes what every later answer
        // is made of and none of them said so: a `didChange` that was dropped, a watched-file
        // change that turned out to be nothing this workspace indexes, a configuration reload
        // that threw the graph away — all of it happened silently, and all of it is the
        // explanation for an answer somebody is about to report as wrong.
        //
        // A request is deliberately not logged here. `serve` writes its own pair, and a third
        // line in front of them would say the same thing less precisely.
        let (kind, about) = task.describe();
        if !matches!(task, Task::Request(_)) {
            tracing::debug!(
                kind,
                about,
                open = self.open.len(),
                dirty = self.dirty,
                "notification"
            );
        }

        match task {
            Task::DidOpen { uri, text, version } => {
                self.open.insert(
                    uri.clone(),
                    OpenDocument {
                        text: TextDocument::new(text.clone(), self.encoding),
                        version,
                    },
                );
                self.index_buffer(&uri, &text);
            }
            Task::DidChange {
                uri,
                changes,
                version,
            } => {
                if !self.open.contains_key(&uri) {
                    // A change for a buffer we never saw opened. Clients do occasionally get
                    // this wrong, and dropping the edit strands the file on stale content
                    // forever — but an incremental range only means anything against the exact
                    // text it was computed from. Recover only when the client sent a whole
                    // buffer; applying a range to the wrong base is worse than not applying it.
                    if !changes.iter().any(|change| change.range.is_none()) {
                        tracing::warn!(
                            "didChange for un-opened {uri}; an incremental edit cannot be \
                             reconstructed, so it is being dropped"
                        );
                        return;
                    }
                    tracing::warn!("didChange for un-opened {uri}; treating it as an open");
                    self.open.insert(
                        uri.clone(),
                        OpenDocument {
                            text: TextDocument::new(String::new(), self.encoding),
                            version,
                        },
                    );
                }

                {
                    let document = self.open.get_mut(&uri).expect("inserted above if missing");
                    for change in &changes {
                        document.text.apply(change.range, &change.text);
                    }
                    document.version = version;
                }
                // The edit is applied; the index is not. See `pending_index` — this is the one
                // route where something cheap is routinely queued behind it. `mark_dirty_for`
                // has to happen here rather than with the index, or a graph request arriving in
                // between would find `dirty` false and answer without the edit at all.
                if !self.pending_index.contains(&uri) {
                    self.pending_index.push(uri.clone());
                }
                self.mark_dirty_for(&uri);
            }
            Task::DidClose { uri } => {
                self.open.remove(&uri);
                // Closing a buffer does not remove the file from the project. Fall back to
                // whatever is on disk; only drop the document if the file is really gone.
                match uri.to_path().filter(|path| path.is_file()) {
                    Some(path) => match std::fs::read_to_string(&path) {
                        Ok(text) => self.index_buffer(&uri, &text),
                        Err(error) => {
                            tracing::warn!(
                                "could not re-read {} after close: {error}",
                                path.display()
                            );
                            self.forget(&uri);
                        }
                    },
                    None => self.forget(&uri),
                }
            }
            Task::DidSave { uri } => {
                // The buffer we already indexed is what got written, so there is nothing to do.
                tracing::trace!("saved {uri}");
            }
            Task::WatchedFiles { uris } => {
                tracing::debug!(paths = uris.len(), "watched files changed");
                self.refresh(uris);
            }
            Task::ChangeConfig { options } => {
                self.workspace.set_options(options);
                self.handle(Task::ReloadConfig);
            }
            Task::ReloadConfig => {
                let mut problems = self.workspace.reload();
                // The log is re-pointed before the line that says the reload happened, so a
                // file the user has just turned on holds the reload that turned it on.
                let root = self.workspace.root().to_path_buf();
                problems.extend(self.logging.apply(&self.workspace.config().log, &root));
                self.workspace.say_which_way_rails_went();
                for problem in problems {
                    tracing::warn!("{problem}");
                    self.show_warning(&problem);
                }
                let changed = self.workspace.config().changed_from_defaults();
                tracing::info!(
                    "reloaded configuration; re-indexing workspace (changes {})",
                    if changed.is_empty() {
                        "nothing".to_owned()
                    } else {
                        changed.join(", ")
                    }
                );
                self.rebuild();
            }
            Task::Request(request) => self.serve(request),
            #[cfg(test)]
            Task::Panic => panic!("the analysis thread, because a test asked it to"),
        }
    }

    // -----------------------------------------------------------------------
    // Gem indexing
    // -----------------------------------------------------------------------

    /// Find everything outside the workspace that belongs in the graph — Ruby's own signatures
    /// and the project's gems — and queue their files. Indexes nothing itself.
    ///
    /// Discovery walks a few hundred directories, so it is not free — but it is bounded and it
    /// happens once, whereas indexing the files it finds is seconds of work that has to be
    /// interleaved with serving requests.
    fn queue_background_indexing(&mut self) {
        let started = Instant::now();
        let max_files = self.workspace.config().gems.max_files;

        // Scoped so the `&mut Workspace` borrow ends before anything else touches `self`.
        let (
            problems,
            load_paths,
            signature_paths,
            engine_paths,
            gem_count,
            roots,
            held,
            signatures,
        ) = {
            let signatures = self.workspace.signatures().clone();
            let discovered = self.workspace.gems();
            (
                discovered.problems.clone(),
                discovered.load_paths(),
                discovered.signature_paths(),
                discovered.engine_paths(),
                discovered.gems.len(),
                discovered
                    .roots
                    .iter()
                    .chain(discovered.ruby_lib.iter())
                    .chain(discovered.rbs_collection.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
                // **The directories that hold something, which is a different list from the ones
                // searched.** `Gems::roots` is every gem path that exists on the machine — every
                // Ruby asdf has installed, and the system's own — because the first question when
                // no gems are found is which directories were looked in. That is the right list
                // for `is_own_code`, where a wider answer is still a correct one, and the wrong
                // list for a document selector: it would ask the editor to claim every Ruby file
                // under every Ruby installed here, most of them from bundles this project has
                // nothing to do with. One directory per gem the lockfile actually resolved, which
                // contains that gem's `lib/`, its `sig/` and an engine's `app/` alike.
                discovered
                    .gems
                    .iter()
                    .map(|gem| gem.path.clone())
                    .chain(discovered.ruby_lib.iter().cloned())
                    .chain(discovered.rbs_collection.iter().cloned())
                    .collect::<Vec<_>>(),
                signatures,
            )
        };

        // `.gem_rbs_collection/` is in `roots` for one reason and it is the reason a vendored
        // bundle is: it lives *inside* the workspace root, so without it every squiggle in
        // somebody else's curated signatures would be published as the user's own.
        self.foreign_prefixes = roots
            .iter()
            .chain(signatures.origin.is_some().then_some(&signatures.root))
            .filter_map(|root| DocUri::from_path(root))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .collect();
        // From the same list the walk below uses, so "indexed as an engine" and "read as an
        // engine" cannot come apart. A directory URI for `workspace_prefix`'s reason: `.../app`
        // must not match a gem that keeps an `app-bundle/` beside it.
        self.engine_prefixes = engine_paths
            .iter()
            .filter_map(|path| DocUri::from_path(path))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .collect();
        // The walk's memo is keyed by "has rubydex re-indexed this document", and this is the one
        // input it holds that is not a document: `Analysis::is_generator_source` reads the list
        // above, so a contribution projected under the previous one may admit a former engine's
        // `app/` or refuse a new one. The batch below sets `touched_all` and would drop the map
        // anyway; saying it here is what stops that being a fact two files apart.
        self.contributions.clear();
        // `Workspace::load_paths` rather than the `load_paths` above, because the project's own
        // `lib/` is on it and shadows a gem of the same name — which is what `require "version"`
        // inside an application means. Order preserved: it is the whole content of the list.
        self.load_prefixes = self
            .workspace
            .load_paths()
            .iter()
            .filter_map(|path| DocUri::from_path(path))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .collect();

        // Here rather than after the walk below, and deliberately not behind the gem budget: this
        // is a function of which roots were *discovered*, and a bundle large enough to be
        // truncated is the one whose gems a user is most likely to be reading in.
        // The project's own trees outside the root go on the same list as the gems, and for the
        // same reason: a client's selector is its folder, so a file outside every folder is one
        // nobody would ever send a request about. They are not *foreign* — `own_prefixes` keeps
        // them the user's own code — they are merely elsewhere, and being elsewhere is the whole
        // of what `register_documents` is for.
        let external = self.workspace.external_load_paths();
        self.register_documents(
            &held
                .iter()
                .chain(external.iter())
                .chain(signatures.origin.is_some().then_some(&signatures.root))
                .collect::<Vec<_>>(),
        );

        for problem in problems.iter().chain(signatures.problems.iter()) {
            tracing::warn!("{problem}");
            self.show_warning(problem);
        }

        // Signatures first, and outside the gem budget. They are 250 files against a bundle's
        // tens of thousands, and they are the difference between `String` existing and not —
        // letting `[gems] max_files` decide whether Ruby's own core gets indexed would make the
        // built-ins disappear on exactly the largest projects.
        let mut files = signatures.files();
        let signature_files = files.len();

        // Walked one load path at a time, rather than all of them at once, so the budget below
        // stops at a gem boundary and so the first gem in the lockfile is the first indexed.
        // That costs a deduplication here: Ruby's platform directory is nested *inside* its
        // library directory, and a vendored bundle can sit inside a gem root.
        let mut truncated = false;
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        // The bundle's own signatures before its code, so that a bundle large enough to hit the
        // budget still gets the part that answers what a method *returns* — 52 files against
        // lobsters' tens of thousands, and the cheapest declarations in the whole pass. They are
        // inside `[gems] max_files` all the same: unlike Ruby's own core they are not the
        // difference between `String` existing and not, and a budget with an exception per list
        // stops being a budget.
        // Then a Rails engine's `app/`, before the bundle's `lib/`. An engine declares
        // `require_paths = ["lib"]`, so `ActiveStorage::Blob` — a class thousands of
        // applications name — is on no load path, so without this it is not a document at all,
        // while `ActiveStorage::Service` one directory away in `lib/` always answers.
        // It goes ahead of the load paths on `sig/`'s argument and with `sig/`'s arithmetic:
        // 417 files against 24,425, so a bundle large enough to hit the budget still gets the
        // half that is missing rather than more of the half that is not.
        for path in signature_paths
            .iter()
            .chain(&engine_paths)
            .chain(&load_paths)
        {
            if files.len() - signature_files >= max_files {
                truncated = true;
                break;
            }
            files.extend(
                gems::source_files(std::slice::from_ref(path))
                    .into_iter()
                    .filter(|file| seen.insert(file.clone())),
            );
        }
        if truncated {
            let message = messages::gem_index_truncated(max_files);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }

        let total = files.len();
        if total == 0 {
            return;
        }
        let gems = gem_count;
        tracing::info!(
            "queued {signature_files} signature files and {} files from {gems} gems in {:.2?}",
            total - signature_files,
            started.elapsed()
        );

        // Popped from the back, so reversing here makes the head of the list the first indexed:
        // the signatures, then the first gem — which is the one the user is most likely to jump
        // into, since Bundler writes the lockfile in dependency order.
        files.reverse();
        self.gem_work = Some(GemIndexing {
            remaining: files,
            total,
            gems,
            signature_files,
            progress: Progress::begin(
                &self.outgoing,
                self.client.work_done_progress,
                GEM_PROGRESS_TOKEN,
                if gems == 0 {
                    "Indexing Ruby signatures"
                } else {
                    "Indexing gems"
                },
                format!("0/{total} files"),
            ),
            started: Instant::now(),
        });
    }

    /// Index one batch of background files. Returns true while there is more to do.
    fn step_gem_indexing(&mut self) -> bool {
        let Some(work) = self.gem_work.as_mut() else {
            return false;
        };

        let take = GEM_FILES_PER_STEP.min(work.remaining.len());
        let batch = work.remaining.split_off(work.remaining.len() - take);
        let finished = work.remaining.is_empty();
        let done = work.total - work.remaining.len();
        let total = work.total;

        if let Some(progress) = work.progress.as_mut() {
            let percentage = u32::try_from(done * 100 / total.max(1)).unwrap_or(100);
            progress.report(format!("{done}/{total} files"), percentage);
        }

        if !batch.is_empty() {
            // The second route that re-reads unmoved text: a rebuild re-queues the whole
            // bundle, and `mkmf-rice.rb` is a gem file.
            let batch = self.without_skipped(batch);
            let batch = self.index_edited_signatures(batch);
            let outcome = indexer::index_files(&mut self.graph, batch);
            for error in &outcome.errors {
                // Debug, not warn: a bundle of a hundred gems will always contain something
                // that does not parse, and none of it is the user's problem.
                tracing::debug!("gem indexing error: {error:?}");
            }
            // A crash is not that. It is one file's worth of a gem answering nothing, which is
            // exactly what a user would otherwise spend an afternoon on.
            for uri in outcome.skipped {
                self.record_skip(&uri);
            }
            // Marked dirty without arming the debounce timer. The next request resolves what is
            // there; scheduling a resolve per batch would re-link the whole graph dozens of
            // times for no one's benefit.
            //
            // `touched_all` for the pass gate's reason and not the timer's: a batch of gem
            // files is a change this loop cannot name a document for, and a gem's `app/` is on
            // four of the six lists. The `Context` comparison would catch anything the walk
            // can see anyway — this is the same sentence said where it is cheap rather than
            // inferred from another line.
            self.touched_all = true;
            self.dirty = true;
        }

        if !finished {
            return true;
        }

        let work = self
            .gem_work
            .take()
            .expect("present at the top of this method");
        let typed = self.types.len();
        tracing::info!(
            "indexed {} signature files and {} files from {} gems in {:.2?}, {typed} methods typed",
            work.signature_files,
            work.total - work.signature_files,
            work.gems,
            work.started.elapsed()
        );
        if let Some(progress) = work.progress {
            progress.end(format!(
                "{} files from {} gems",
                work.total - work.signature_files,
                work.gems
            ));
        }
        // Now arm the timer: everything is in, and the editor is owed the diagnostics that the
        // finished graph produces.
        self.mark_dirty();
        // And the hints, which have no timer of their own. This is the one moment the answer to
        // an already-answered request changes without the document changing: a file opened
        // against a graph with no signatures in it has a margin with nothing in it, and nothing
        // else would ever ask again.
        self.refresh_hints();
        false
    }

    /// Index the signature files in `batch` that need editing first, and hand back the rest.
    ///
    /// See [`signatures`] for what is edited and why. Every route an `.rbs` file can take into
    /// the graph goes through here or through [`Self::index_buffer`] — the signature root that
    /// `workspace::rbs` discovered, a `sig/` directory `index.include` was widened to cover, and
    /// a buffer the editor opened. The rule is one rule; a file that is indexed differently
    /// depending on how it was found is worse than one that is not filtered at all.
    ///
    /// Every `.rbs` path is read here — for the return types, which is a second thing this pass
    /// now does — and only the one in five that holds an interface leaves the parallel path.
    /// Everything else is still a plain path for a worker thread to read.
    ///
    /// Reading each signature twice, once here and once in the indexer, is what that costs.
    /// Measured over the 250 files of `vendor/rbs`: 10 ms of reading and 30 ms of parsing, in a
    /// pass that already takes seconds and runs in the background. Indexing them from the string
    /// this already holds would be the way to avoid it and is much worse — it would move 4 MB of
    /// parsing off the worker threads and onto this one.
    fn index_edited_signatures(&mut self, batch: Vec<PathBuf>) -> Vec<PathBuf> {
        batch
            .into_iter()
            .filter(|path| !self.index_edited_signature(path))
            .collect()
    }

    /// Whether `path` was a signature file with an interface in it, and has now been indexed.
    fn index_edited_signature(&mut self, path: &Path) -> bool {
        if path.extension() != Some(OsStr::new("rbs")) {
            return false;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            return false;
        };
        // Before the edit, and deliberately: the harvest skips `interface` blocks itself, so it
        // sees the same declarations either way, and doing it here means it happens for the four
        // files in five that leave this method by the early return below.
        self.types.harvest(&source);
        let Some(edited) = signatures::without_interfaces(&source) else {
            return false;
        };
        // The URI has to be spelled the way `index_files` would have spelled it, or this forks a
        // second document for the same file. `DocUri` is that spelling.
        let Some(uri) = DocUri::from_path(path) else {
            return false;
        };
        self.index_contained(&uri, &edited, &LanguageId::Rbs);
        true
    }

    /// Index the ERB templates in `batch`, and hand back the rest.
    ///
    /// The shape of [`Self::index_edited_signatures`] and for the same reason, but the stakes
    /// are higher: an `.rbs` file indexed unedited puts extra declarations in the graph, and a
    /// template indexed unedited records **no references at all** — rubydex reads the markup as
    /// Ruby, gives up somewhere in the first tag, and files a handful of parse errors instead of
    /// the call sites. See [`erb`].
    ///
    /// This runs on the walk and not only on `didOpen` because a real Rails application keeps
    /// **one method-call site in seven** in its templates. Indexing only what the editor has
    /// open would make `references` incomplete by an amount that changes as the user opens tabs,
    /// which is worse than a consistently narrow answer and is the exact failure
    /// `coverage.md` holds `references.rs` at 100% to prevent.
    ///
    /// Cost, over lobsters' 121 templates: 17 ms, against 35 ms for its 477 `.rb` files.
    fn index_templates(&mut self, batch: Vec<PathBuf>) -> Vec<PathBuf> {
        batch
            .into_iter()
            .filter(|path| !self.index_template(path))
            .collect()
    }

    /// Whether `path` was a template, and has now been indexed.
    fn index_template(&mut self, path: &Path) -> bool {
        if !erb::is_template(path) {
            return false;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            return false;
        };
        // Spelled the way `index_files` would have spelled it, or this forks a second document
        // for the same file.
        let Some(uri) = DocUri::from_path(path) else {
            return false;
        };
        self.index_contained(&uri, &erb::ruby_view(&source), &LanguageId::Ruby);
        true
    }

    /// Index whatever `didChange` deferred.
    ///
    /// [`Analysis::settle`] is the only caller, and it calls this first: the graph has to hold
    /// every edit before the generators read it or the resolver links over it. It returned
    /// It returns nothing: no caller needs to know whether a step's worth of work happened.
    ///
    /// A buffer that was closed between the edit and this step is skipped rather than replayed:
    /// `didClose` has already put the file back to what is on disk, and re-indexing the buffer
    /// over it would undo exactly that.
    fn index_pending(&mut self) {
        for uri in std::mem::take(&mut self.pending_index) {
            let Some(text) = self.open.get(&uri).map(|open| open.text.text().to_owned()) else {
                continue;
            };
            self.index_buffer(&uri, &text);
        }
    }

    fn index_buffer(&mut self, uri: &DocUri, text: &str) {
        let path = uri.to_path();
        let language = path
            .as_deref()
            .map_or(LanguageId::Ruby, indexer::language_of);
        // The same rule as on the indexing path, or opening a signature file in the editor puts
        // back the declarations that path took out — and leaves them there, because nothing
        // re-indexes the file once the buffer closes. A template is the same rule again: this
        // is the hook `didOpen`, `didChange` and the file watcher all share, and a template that
        // reached the graph raw through any of them would replace its own call sites with parse
        // errors.
        if path.as_deref().is_some_and(erb::is_template) {
            let view = erb::ruby_view(text);
            if self.index_contained(uri, &view, &LanguageId::Ruby) {
                // The view and not the buffer: `ruby_view` is what rubydex's offsets index,
                // which is `ruby_view`'s property and the reason a template needs no special
                // case.
                self.indexed_text.insert(uri.clone(), view);
            }
            self.mark_dirty_for(uri);
            return;
        }
        let edited = matches!(language, LanguageId::Rbs)
            .then(|| {
                // The third route a signature takes into the graph, and the return types have to
                // follow it for the same reason the interface rule does: a table that disagrees
                // with the graph about a method is a wrong answer rather than an absent one.
                self.types.harvest(text);
                signatures::without_interfaces(text)
            })
            .flatten();
        let text = edited.as_deref().unwrap_or(text);
        // **Only when the index actually happened**, which is the bulkhead meeting the map. A
        // contained panic costs the document its *update* and nothing else: the graph goes on
        // answering with whatever version it already held. Recording the new text here would
        // claim the graph holds it, `Rebase::between` would compare two equal strings and
        // answer `identity`, and every offset would be handed to a graph that is some unknown
        // number of edits behind — a wrong answer with no refusal to fall back from.
        //
        // Leaving the previous entry in place is not merely safer, it is *correct*: it is the
        // text the graph really holds, so the map describes the difference exactly. A document
        // whose very first index crashes has no entry, which is the identity — and harmless,
        // because a graph with no such document resolves nothing to be wrong about.
        if self.index_contained(uri, text, &language) {
            self.indexed_text.insert(uri.clone(), text.to_owned());
        }
        self.mark_dirty_for(uri);
    }

    /// How this buffer's offsets relate to the ones the graph holds for it.
    ///
    /// `Rebase::identity` wherever the two texts are equal, which is every request that is not
    /// answered between a keystroke and its index — so on a document nobody is typing in, this
    /// is a length comparison and two equal strings.
    fn rebase_for(&self, uri: &DocUri, buffer: &str) -> Rebase {
        match self.indexed_text.get(uri) {
            Some(indexed) => Rebase::between(buffer, indexed),
            // Never indexed as a buffer, so the graph holds whatever the disk walk gave it and
            // nothing here can say how that differs. Identity is the safe assumption, and a
            // deferred answer is gated on an entry existing.
            None => Rebase::identity(u32::try_from(buffer.len()).unwrap_or(u32::MAX)),
        }
    }

    // ------------------------------------------------------------------------- the bulkhead

    /// Put one document into the graph, and remember it if that crashes the indexer.
    ///
    /// The five inline routes all end here. A crash costs the document its *update* and
    /// nothing else — the panic is in the build, so the graph is never entered and whatever
    /// version it already held goes on answering, which is why a buffer that is being typed
    /// into a bad state keeps the answers it had before the edit rather than losing them.
    fn index_contained(&mut self, uri: &DocUri, source: &str, language: &LanguageId) -> bool {
        if indexer::index_source(&mut self.graph, uri.as_str(), source, language) {
            self.unskip(uri);
            return true;
        }
        self.record_skip(uri);
        false
    }

    /// Remember that indexing `uri` crashed, and say so.
    ///
    /// `warn!` every time, because a report is made of two things and the default panic hook
    /// has just printed the first: rubydex's own file and line, and the file that provoked it.
    /// `showMessage` only the first time, because the buffer route retries on every keystroke
    /// and a user editing the offending file is owed one notification rather than one per
    /// character.
    fn record_skip(&mut self, uri: &DocUri) {
        tracing::warn!("indexing {uri} crashed; leaving that file out of the index");
        if !self.skipped.insert(uri.clone()) {
            return;
        }
        let path = uri.to_path().unwrap_or_else(|| PathBuf::from(uri.as_str()));
        let message = messages::file_not_indexed(&path);
        self.show_warning(&message);
    }

    /// Forget that it ever did, because it has just worked.
    ///
    /// The whole of the "not permanent either" half: a user who fixes the file does not have to
    /// restart the server, and nothing has to decide what counts as a fix.
    fn unskip(&mut self, uri: &DocUri) {
        if self.skipped.remove(uri) {
            tracing::info!("{uri} indexes cleanly again");
        }
    }

    /// The paths in `batch` that did not crash the indexer last time.
    ///
    /// Asked by the two bulk routes and by neither of the four that index because the text may
    /// have moved. `is_empty` first because this runs over every gem batch and the set is empty
    /// in every workspace anyone has measured.
    fn without_skipped(&self, batch: Vec<PathBuf>) -> Vec<PathBuf> {
        if self.skipped.is_empty() {
            return batch;
        }
        batch
            .into_iter()
            .filter(|path| {
                let skip = DocUri::from_path(path).is_some_and(|uri| self.skipped.contains(&uri));
                if skip {
                    tracing::debug!("skipping {}; it crashed the indexer", path.display());
                }
                !skip
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // What the repository declares without writing it down
    // -----------------------------------------------------------------------

    /// Throw the graph away and build it again: the workspace, the open buffers, the gems.
    ///
    /// Shared by `ReloadConfig` — where the configuration decides what belongs in the index, so
    /// nothing computed under the old one can be trusted — and by the recovery in
    /// [`Analysis::resolve`], where the graph is in an unknown state and starting over is the
    /// only honest answer.
    fn rebuild(&mut self) {
        self.graph = Graph::new();
        // The table is keyed by declarations this graph no longer has, and every signature file
        // is about to be read again anyway.
        self.types.clear();
        // Same reason, one step further: the documents it maps are not in the new graph either,
        // and whatever generated them will be asked again as their source files are re-read.
        self.synthesized.clear();
        self.generated.clear();
        // And the pass gate has nothing to compare against: the projection it holds is of a
        // graph that no longer exists. The `mark_dirty` at the end of this would cover it, but
        // an invariant that depends on a later line is one a later edit can lose.
        self.generated_from = None;
        // The per-document half of the same sentence — the gate's evidence and the walk's memo
        // — keyed by a `UriId` of a graph that is about to be replaced, and every document in it
        // is about to be re-indexed anyway.
        self.contributions.clear();
        // Same again: every entry describes offsets into the graph being thrown away.
        self.indexed_text.clear();
        // Every module's own parse memo, which each keeps. Keyed by a URI rather than by anything
        // the graph owns and with the file's own freshness, so it would survive a rebuild
        // correctly — and a rebuild is a configuration change, which can move the workspace root
        // and so what every provenance line says. Dropped rather than reasoned about, by building
        // the registry again.
        self.knowledge = Self::registered();
        // Whatever was still queued refers to the old configuration's gem roots, and the graph
        // it was going to be indexed into no longer exists.
        if let Some(work) = self.gem_work.take()
            && let Some(progress) = work.progress
        {
            progress.end("cancelled".to_owned());
        }
        self.foreign_prefixes.clear();
        self.engine_prefixes.clear();
        self.load_prefixes.clear();
        // The deferred edits. Every open buffer is replayed below, which is a superset of whatever was
        // waiting, and the graph the deferred index was for has just been thrown away.
        self.pending_index.clear();
        // The skip list is the one thing here that is deliberately **kept**. Everything above is
        // keyed by a graph that is about to be replaced; the skip list is keyed by a file that
        // is still on disk and still crashes the indexer, and the recovery in
        // [`Analysis::resolve`] runs the walk below — so clearing it would let the file this
        // rebuild may be recovering from take the rebuild down with it.
        self.index_workspace();
        // Open buffers shadow disk, so replay them over the freshly indexed tree.
        let buffers: Vec<(DocUri, String)> = self
            .open
            .iter()
            .map(|(uri, document)| (uri.clone(), document.text.text().to_owned()))
            .collect();
        for (uri, text) in buffers {
            self.index_buffer(&uri, &text);
        }
        // Unconditionally, even with no buffers to replay: a reload may have changed which
        // rules are on, or dropped files from the index, and both change what the editor should
        // be showing. Without this a project with no open files keeps displaying the
        // diagnostics from the previous configuration forever.
        self.mark_dirty();
        self.queue_background_indexing();
    }

    /// Bring the index back in line with what is on disk, for paths a watcher named.
    ///
    /// Four rules, and each is a way to be wrong that nothing else would catch:
    ///
    /// - **A buffer beats the disk.** A rebase under an open file must not overwrite what the
    ///   editor is showing. `didClose` already implements exactly this precedence in the other
    ///   direction — fall back to disk only once the buffer is gone.
    /// - **A gem is not the user's code.** Watchers are the client's and shared, so a change
    ///   inside a vendored bundle can arrive; `is_own_code` is the test that is right when the
    ///   bundle lives inside the workspace root.
    /// - **The walk decides what belongs.** `Workspace::indexes` is the same rules the startup
    ///   walk applied, so a file the user excluded stays excluded however it is written.
    /// - **`index.max_files` still applies**, because a pathological repository exists and the
    ///   cap is the only thing standing between it and the process.
    ///
    /// **This loop has three outcomes and not two**: re-index a document, forget one, or
    /// neither. A `db/structure.sql` is the third. It is watched —
    /// `capabilities::watched_files` registers `db/*structure.sql` beside `ya-lsp.toml`, which
    /// is watched and never indexed — it is read by `synthesize`, and it must never reach
    /// rubydex, which would read
    /// SQL as Ruby and file a document full of parse errors. So the branch invalidates and
    /// indexes nothing, and it sits **above** the `Workspace::indexes` gate because that gate is
    /// `index.include`, which is the shapes Ruby is written in and will always say no.
    fn refresh(&mut self, uris: Vec<DocUri>) {
        let started = Instant::now();
        let max_files = self.workspace.config().index.max_files;
        let (mut indexed, mut forgotten, mut full) = (0_usize, 0_usize, false);
        let mut invalidated = 0_usize;

        for uri in uris {
            if self.open.contains_key(&uri) {
                // The editor's copy is newer than anything on disk by definition, and it is
                // what every answer is already computed against.
                tracing::trace!(
                    "{uri} changed on disk but is open in the editor; keeping the buffer"
                );
                continue;
            }
            if !self.is_own_code(uri.as_str()) {
                tracing::trace!("{uri} changed, but it is not this project's code to index");
                continue;
            }
            // Above the `is_file` test as well as above the index gate, so that one branch
            // covers a dump being written, edited and deleted alike: `synthesize` re-reads the
            // directory every settle, so which of the three it was is a question this loop
            // never has to ask, and a deleted one is pruned by `forget_stale` rather than by
            // anything here.
            if self.workspace.features().schema
                && uri.to_path().is_some_and(|path| rails::is_structure(&path))
            {
                tracing::trace!(
                    "{uri} changed; it is a schema dump, so the next settle re-reads it"
                );
                self.mark_dirty();
                invalidated += 1;
                continue;
            }

            // Gone, or never a file — `didClose`'s own test, for the same reason. Whether the
            // workspace *would* index it is not a question that can be asked of a path that is
            // not there, and it does not need to be: a document the graph holds is one that was
            // indexed, so it is one to drop, and one it does not hold is nothing at all.
            let Some(path) = uri.to_path().filter(|path| path.is_file()) else {
                if self.forget_indexed(&uri) {
                    forgotten += 1;
                }
                continue;
            };
            if !self.workspace.indexes(&path) {
                tracing::trace!("{uri} changed but is not a file this workspace indexes");
                continue;
            }
            // A file the walk skipped is not in the graph and was counted against the cap all
            // the same — `index_workspace` counts what discovery found, not what survived it —
            // so without this the retry below would count it twice.
            let known = self.indexed(&uri) || self.skipped.contains(&uri);
            if !known && self.workspace_files >= max_files {
                full = true;
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(
                        "could not read {} after a watched change: {error}",
                        path.display()
                    );
                    continue;
                }
            };
            if !known {
                self.workspace_files += 1;
            }
            // The same entry point an open buffer takes, so the `.rbs` interface rule is one
            // rule: a signature file that reaches the graph through the watcher must not put
            // back the `Object` members the indexing path took out.
            self.index_buffer(&uri, &text);
            indexed += 1;
        }

        if full && !self.index_full_reported {
            self.index_full_reported = true;
            let message = messages::index_full(max_files);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }
        if indexed + forgotten + invalidated > 0 {
            tracing::debug!(
                "re-indexed {indexed}, dropped {forgotten} and invalidated {invalidated} \
                 watched files in {:.2?}",
                started.elapsed()
            );
        }
    }

    /// Whether the graph currently holds a document for `uri`.
    ///
    /// One hash of the URI string, which is what makes it affordable per changed file; the
    /// alternative — asking the workspace walk — costs a `read_dir` per directory.
    fn indexed(&self, uri: &DocUri) -> bool {
        self.graph
            .documents()
            .contains_key(&UriId::from(uri.as_str()))
    }

    /// Drop `uri` from the graph if it holds it, and say whether it did.
    fn forget_indexed(&mut self, uri: &DocUri) -> bool {
        if !self.indexed(uri) {
            return false;
        }
        self.forget(uri);
        self.workspace_files = self.workspace_files.saturating_sub(1);
        true
    }

    fn forget(&mut self, uri: &DocUri) {
        self.graph.delete_document(uri.as_str());
        // The map describes offsets into a document that no longer exists.
        self.indexed_text.remove(uri);
        // Whatever this file *implied* goes with it. Left behind, a generated declaration would
        // outlive the only thing that could ever refresh it: nothing re-reads a file that is
        // gone, so the columns of a deleted `db/schema.rb` would answer forever.
        self.synthesized.forget(&mut self.graph, uri);
        self.mark_dirty();
    }

    /// Something changed and the pass cannot know what, so the next one does all of its work.
    fn mark_dirty(&mut self) {
        self.touched_all = true;
        self.dirty = true;
        self.resolve_at = Some(Instant::now() + resolve_debounce());
    }

    /// The same, for the one route that knows which document moved.
    fn mark_dirty_for(&mut self, uri: &DocUri) {
        self.touched.insert(uri.as_str().to_owned());
        self.dirty = true;
        self.resolve_at = Some(Instant::now() + resolve_debounce());
    }

    /// Run the debounced work: link the graph, then push whatever diagnostics moved.
    fn settle(&mut self) {
        // Before anything else, and before `dirty` is cleared: a buffer whose index the loop
        // deferred has to be in the graph before the pass reads it or the resolver links over
        // it. `index_buffer` marks dirty again, which is why this cannot run after the flag.
        self.index_pending();
        self.resolve_at = None;
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.regenerate();
        self.publish_diagnostics();
    }

    /// Generate, link, and then find the places only a linked graph can name.
    ///
    /// **One function because the three are one order**, and the middle step is what makes it an
    /// order rather than a list. Every generator writes the text rubydex is about to link, so
    /// while the pass is running the graph holds four declarations — `Object`, `BasicObject`,
    /// `Module` and `Class` — and a generator that needs to ask *where does Rails write `where`*
    /// cannot be answered where it stands. It states the name instead and
    /// [`Self::place_generated_members`] answers it, which is only sound after the resolve.
    ///
    /// Both callers are a bulk route: the first index of a workspace, and a settle. Neither may
    /// run two of these three.
    fn regenerate(&mut self) {
        self.synthesize();
        self.resolve();
        self.place_generated_members();
    }

    /// Link the graph — and survive rubydex panicking while it does.
    ///
    /// rubydex 0.2.5 panics inside `Resolver::resolve` after a document is deleted:
    /// `Graph::delete_document` invalidates first and untracks the document's strings second, so
    /// the work the invalidation queued can name a string that is no longer there, and
    /// `resolution.rs:748` unwraps it. Reproduced by deleting
    /// `lib/solargraph/yard_map/to_method.rb` from a solargraph v0.58.2 checkout; reachable
    /// through `didClose` on a file that is gone, and routine once the watcher is on, because a
    /// `git checkout` that removes a file is an ordinary Tuesday.
    ///
    /// There is nothing to upgrade to and the alternative is a fork, so it is contained here
    /// instead.
    /// Left alone it is the worst failure this server has: the analysis thread dies, the editor
    /// keeps sending requests, and a language server that answers nothing at all looks exactly
    /// like one that is thinking. A graph half-way through a resolve is in an unknown state, so
    /// the only honest recovery is to throw it away and index everything again.
    fn resolve(&mut self) {
        let started = Instant::now();
        // The panic message itself still reaches stderr through the default hook, which is
        // where the rubydex file and line a report needs are written.
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            crash_the_next_resolve_if_asked();
            Resolver::new(&mut self.graph).resolve();
        }))
        .is_err();
        if !crashed {
            tracing::debug!("resolved in {:.2?}", started.elapsed());
            return;
        }
        if self.recovering {
            // The rebuild crashed too, so rebuilding again would only crash again. The graph
            // keeps whatever it managed to link; every feature degrades rather than stopping.
            tracing::error!(
                "linking the graph crashed again during recovery; leaving the index as it is"
            );
            return;
        }
        let message = messages::index_rebuilt();
        tracing::warn!("{message}");
        self.show_warning(&message);
        self.recovering = true;
        self.rebuild();
        self.recovering = false;
    }

    // -----------------------------------------------------------------------
    // Diagnostics
    // -----------------------------------------------------------------------

    /// Push the diagnostics that changed since the last publish.
    ///
    /// Sending every URI every time would mean one notification per workspace file on every
    /// keystroke, so this diffs: URIs whose set is unchanged are skipped, and a URI that has
    /// dropped out gets an explicit empty publish, which is the only way to clear it.
    fn publish_diagnostics(&mut self) {
        let current = self.collect_diagnostics();

        for (uri, items) in &current {
            if self.published.get(uri) == Some(items) {
                continue;
            }
            self.send_diagnostics(uri, items.clone());
        }

        let cleared: Vec<DocUri> = self
            .published
            .keys()
            .filter(|uri| !current.contains_key(*uri))
            .cloned()
            .collect();
        for uri in cleared {
            self.send_diagnostics(&uri, Vec::new());
        }

        // `current` never holds an empty entry, so the map stays the size of "files with
        // problems" rather than the size of the workspace.
        self.published = current;
    }

    /// Every diagnostic the graph currently holds, grouped by document and converted to LSP.
    fn collect_diagnostics(&self) -> HashMap<DocUri, Vec<lsp_types::Diagnostic>> {
        let config = &self.workspace.config().diagnostics;

        // Group by the graph's own URI string first. Turning that into a `DocUri` parses a URL
        // and touches the filesystem check, and a file with a hundred parse errors should pay
        // for that once, not a hundred times.
        let mut by_document: HashMap<
            &str,
            Vec<(&rubydex::diagnostic::Diagnostic, DiagnosticSeverity)>,
        > = HashMap::new();

        for diagnostic in self.graph.all_diagnostics() {
            let rule = *diagnostic.rule();
            let configured =
                config.severity(diagnostics::name(rule), diagnostics::default_severity(rule));
            // `Off` has no LSP spelling — the diagnostic has to be dropped, not downgraded.
            let Some(severity) = diagnostics::to_lsp_severity(configured) else {
                continue;
            };
            let Some(document) = self.graph.documents().get(diagnostic.uri_id()) else {
                // The document was deleted but a declaration still carries its diagnostic.
                // There is no file to attach it to.
                continue;
            };
            // Filtered here, on the raw URI string, rather than after grouping: a Rails bundle
            // contributes tens of thousands of diagnostics nobody can act on, and parsing a URL
            // for each of them on every settle would cost more than the diagnostics do.
            if !self.is_own_code(document.uri()) {
                continue;
            }
            by_document
                .entry(document.uri())
                .or_default()
                .push((diagnostic, severity));
        }

        let mut collected = HashMap::with_capacity(by_document.len());
        for (raw_uri, entries) in by_document {
            let Some(uri) = DocUri::from_uri_str(raw_uri) else {
                // rubydex's synthetic built-in document, or anything else with no file.
                continue;
            };
            // A template's diagnostics are not about anything the user wrote. What survives a
            // correct scan is `<%= yield :subnav %>` in a layout — two of them over lobsters'
            // 121 templates, and both legal, because a compiled Rails template is a method body
            // and Prism is reading a file. A rule that fires on correct input does not earn a
            // squiggle; see `diagnostics.rs`, and `erb.rs` for the other two scanners that were
            // measured against this one.
            if erb::is_template_uri(&uri) {
                continue;
            }
            // Reading the file to place the ranges is only affordable because this runs for
            // files that *have* diagnostics, which on healthy code is none of them.
            let Some(mut items) = self.with_text(&uri, |text| {
                entries
                    .iter()
                    .map(|(diagnostic, severity)| {
                        let offset = diagnostic.offset();
                        lsp_types::Diagnostic {
                            range: text.range_at(offset.start(), offset.end()),
                            severity: Some(*severity),
                            // The rule name, so the Problems panel shows the key the user needs
                            // to put in `[diagnostics.rules]` to change or silence it.
                            code: Some(lsp_types::NumberOrString::String(
                                diagnostics::name(*diagnostic.rule()).to_owned(),
                            )),
                            source: Some(diagnostics::SOURCE.to_owned()),
                            message: diagnostic.message().to_owned(),
                            ..lsp_types::Diagnostic::default()
                        }
                    })
                    .collect::<Vec<_>>()
            }) else {
                // No readable text means no trustworthy ranges. Publishing nothing beats
                // publishing squiggles in the wrong place.
                tracing::debug!("skipping diagnostics for unreadable {uri}");
                continue;
            };

            // `all_diagnostics` walks hash maps, so its order varies run to run. Sorting makes
            // the set comparable against the last publish, which is what keeps the diff honest.
            items.sort_by(|a, b| {
                (a.range.start, a.range.end, &a.message).cmp(&(
                    b.range.start,
                    b.range.end,
                    &b.message,
                ))
            });
            collected.insert(uri, items);
        }
        collected
    }

    fn send_diagnostics(&self, uri: &DocUri, items: Vec<lsp_types::Diagnostic>) {
        let Ok(lsp_uri) = uri.to_lsp() else {
            tracing::warn!("cannot publish diagnostics for {uri}: not a valid LSP uri");
            return;
        };
        let params = lsp_types::PublishDiagnosticsParams {
            uri: lsp_uri,
            diagnostics: items,
            // Lets the client throw away diagnostics for text the user has already edited past.
            version: self.open.get(uri).and_then(|open| open.version),
        };
        let _ = self
            .outgoing
            .send(Message::Notification(lsp_server::Notification::new(
                "textDocument/publishDiagnostics".to_owned(),
                params,
            )));
    }

    /// Whether a document is the user's own code — inside the workspace, outside every gem.
    ///
    /// Three features turn on this one test, and they want it for the same reason: a result the
    /// user cannot act on is worse than no result. Nobody can fix a warning inside someone
    /// else's gem, nobody is going to edit a gem to rename their own method, and a Rails bundle
    /// would bury the answer under tens of thousands of them either way.
    ///
    /// The second check is not redundant with the workspace check. A *vendored* bundle lives at
    /// `vendor/bundle/ruby/<abi>`, inside the workspace root by construction, so the prefix test
    /// alone calls a hundred gems — and, for a project that vendors its own signatures, the
    /// whole of Ruby's core — the user's own code.
    ///
    /// Compared as a URI prefix rather than a path: both sides come from `Url::from_file_path`,
    /// so they are already canonical, and this runs once per diagnostic.
    fn is_own_code(&self, uri: &str) -> bool {
        let named = uri.starts_with(&self.workspace_prefix)
            || self
                .own_prefixes
                .iter()
                .any(|prefix| uri.starts_with(prefix));
        named
            && !self
                .foreign_prefixes
                .iter()
                .any(|prefix| uri.starts_with(prefix))
    }

    /// Whether a generator may read this document: the user's own code, or a Rails engine's.
    ///
    /// The one place the answer differs from [`Analysis::is_own_code`]. An engine ships its
    /// models under `app/`, declares `require_paths = ["lib"]`,
    /// and its `has_many :variant_records` is a member of `ActiveStorage::Blob` — a class an
    /// application names and chains off, and one nothing in the bundle's `lib/` declares.
    ///
    /// Widening `is_own_code` itself would have been one line and three regressions: a
    /// diagnostic published inside somebody's gem, a rename offered there, and the bundle's
    /// classes in `workspace/symbol`. Those three want "code the user can act on"; a generator
    /// wants "code whose declarations the user can reach", and the honest way to have both is
    /// two predicates.
    fn is_generator_source(&self, uri: &str) -> bool {
        self.is_own_code(uri)
            || self
                .engine_prefixes
                .iter()
                .any(|prefix| uri.starts_with(prefix))
    }

    /// Run `f` over the text of `uri`: the open buffer if the editor has one, otherwise the file
    /// on disk. `None` when there is no readable text.
    ///
    /// The disk read is what rubydex indexed, unless the file changed underneath us — in which
    /// case a re-index is already on its way and the ranges correct themselves.
    ///
    /// **A template is handed over blanked**, which is the other half of what makes ERB work at
    /// all. Nine of the seventeen requests parse the document themselves rather than reading the
    /// graph — the outline, the folds, the scope walk under highlight and rename, the semantic
    /// tokens, the cursor under a typed receiver — and every one of them would be handed markup
    /// to parse as Ruby. Blanking here is not a second rule: it is [`erb::ruby_view`] again, the
    /// same bytes rubydex was given, and because it preserves length and line breaks the offsets
    /// on both sides of the server are the same offsets. The buffer itself stays exactly what
    /// the editor sent, because that is what incremental edits are applied to.
    ///
    /// **It is handed over blanked and addressed unblanked**, and getting that wrong is
    /// silent. An LSP position is a count of code units in the text the *client*
    /// has, and blanking replaces a 3-byte `“` with three spaces — so a column counted against
    /// the view is displaced left by (bytes − units) of every non-ASCII character in the markup
    /// before it on the line, and every span answered from it is displaced right by the same
    /// amount. [`TextDocument::blanked`] carries both texts for that reason. The template is
    /// read twice on the disk path and cloned once on the buffer path, which is one allocation
    /// the size of a template beside a Prism parse of it.
    fn with_text<R>(&self, uri: &DocUri, f: impl FnOnce(&TextDocument) -> R) -> Option<R> {
        let template = erb::is_template_uri(uri);
        if let Some(open) = self.open.get(uri) {
            if !template {
                return Some(f(&open.text));
            }
            let source = open.text.text();
            let view = erb::ruby_view(source);
            return Some(f(&TextDocument::blanked(
                source.to_owned(),
                view,
                self.encoding,
            )));
        }
        let text = std::fs::read_to_string(uri.to_path()?).ok()?;
        if template {
            let view = erb::ruby_view(&text);
            return Some(f(&TextDocument::blanked(text, view, self.encoding)));
        }
        Some(f(&TextDocument::new(text, self.encoding)))
    }

    /// The document exactly as the editor has it, markup and all.
    ///
    /// The one question [`Self::with_text`] cannot answer, because it answers every other one by
    /// taking the markup away: whether the cursor is in Ruby. Only `completion` asks.
    fn with_source<R>(&self, uri: &DocUri, f: impl FnOnce(&str) -> R) -> Option<R> {
        match self.open.get(uri) {
            Some(open) => Some(f(open.text.text())),
            None => Some(f(&std::fs::read_to_string(uri.to_path()?).ok()?)),
        }
    }

    /// Another document's text, by the URI rubydex filed it under, and its map onto the graph.
    ///
    /// The one thing [`types`] reads that is not the graph, and it goes through
    /// [`Self::with_text`] rather than straight to disk for the reason that function exists: an
    /// open buffer is authoritative, so a controller being edited types the template it renders
    /// before it is saved. It clones, unlike every other accessor here — the text has to outlive
    /// the borrow because what reads it is a parse in another module — and it is reached only
    /// where a receiver in a *template* was nothing but a name, which is a path no ordinary Ruby
    /// file takes.
    ///
    /// **The [`Rebase`] is that same sentence finished.** Preferring the open buffer is what
    /// makes an unsaved controller answer, and it is also what puts the offsets a whole file
    /// behind the graph the moment somebody types in it. Both come from the one `with_text`
    /// call, so the text handed out and the map handed out describe the same string.
    fn read_of(&self, uri: &str) -> Option<(String, Rebase)> {
        let uri = DocUri::from_uri_str(uri)?;
        self.with_text(&uri, |text| {
            (text.text().to_owned(), self.rebase_for(&uri, text.text()))
        })
    }

    /// What the type side reads besides the graph, built per request.
    ///
    /// Per request rather than held, because the closure borrows `self` and the flag is
    /// configuration that a `workspace/didChangeConfiguration` can move underneath it.
    fn sources<'a>(
        &'a self,
        read: &'a dyn Fn(&str) -> Option<(String, Rebase)>,
    ) -> types::Sources<'a> {
        types::Sources {
            graph: &self.graph,
            types: &self.types,
            read,
            views: &self.views,
            guess: self.workspace.config().types.guess_from_names,
            features: self.workspace.features(),
            layout: self.layout(),
            // One cursor, so the one arm that places an offset of its own walks the document
            // once. `inlayHint` is the caller that cannot afford that and hands its own walk
            // down — see `Sources::bodies`.
            bodies: None,
            // Nothing has been followed yet. The one rung that raises it hands a copy down
            // rather than mutating this, so a request always starts from zero.
            constant_hops: 0,
        }
    }

    /// A `[trees]` key that **replaces** a built-in list says what it replaced.
    ///
    /// Two of the three lists replace rather than extend, because a name on either *deletes*
    /// answers when it is wrong — so somebody who set one has taken four directory names six
    /// repositories agree on out of play, and the only place that is visible is here. It is the
    /// same debt `include_is_empty` pays for `index.include`.
    ///
    /// **A log line and not a `messages::` sentence**, deliberately: setting these is a
    /// legitimate thing to do, and a `window/showMessage` every session about a setting the user
    /// meant is a nag rather than a warning. `test_support` is absent for the same reason it is
    /// additive — it replaces nothing, so there is nothing to say.
    fn say_what_the_fences_replaced(&self) {
        let trees = &self.workspace.config().trees;
        if let Some(test) = &trees.test {
            tracing::info!(
                "trees.test is {test:?}, replacing the built-in {:?}",
                environment::TEST_TREES
            );
        }
        if let Some(migration) = &trees.migration {
            tracing::info!(
                "trees.migration is {migration:?}, replacing the built-in {:?}",
                [environment::MIGRATION_PAIR]
            );
        }
    }

    /// A misspelled key in `[diagnostics.rules]` does nothing at all, silently, forever. Say so.
    fn warn_about_unknown_rules(&self) {
        for name in self.workspace.config().diagnostics.rules.keys() {
            if diagnostics::is_known_name(name) {
                continue;
            }
            let known = diagnostics::known_names().collect::<Vec<_>>().join(", ");
            let message = messages::unknown_diagnostic_rule(name, &known);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }
    }

    fn respond(&self, response: Response) {
        // A send failure means the client is gone; the main loop is already tearing down.
        let _ = self.outgoing.send(Message::Response(response));
    }

    /// Ask the client to draw the margin again.
    ///
    /// A server-initiated request, like `client/registerCapability`: the main loop reads the
    /// answer and drops it, because there is nothing to do with either one. A string id in the
    /// server's own id space, which cannot collide with anything the client sent.
    fn refresh_hints(&self) {
        if !self.client.hint_refresh {
            return;
        }
        let _ = self.outgoing.send(Message::Request(Request {
            id: RequestId::from("ya-lsp/inlay-hint-refresh".to_owned()),
            method: "workspace/inlayHint/refresh".to_owned(),
            params: serde_json::Value::Null,
        }));
    }

    /// Ask the client to claim the files this server answers about and its own selector did not.
    ///
    /// A gem's source, Ruby's stdlib and the RBS beside them live outside every workspace folder,
    /// and the server indexes all three — but a document selector is the only gate on what the
    /// client ever sends, so until this lands `definition` jumps into a gem and every request in
    /// the file it opened is dead, with nothing logged anywhere because nothing was asked.
    ///
    /// **Sent from here because this is where the answer exists.** The prefixes are the gem roots
    /// the bundle resolved to, and no part of the handshake knows them: discovery runs on this
    /// thread. `client/registerCapability` is the same channel the file watcher uses, for the same
    /// reason — a thing with no static form in the protocol — and the client's own
    /// `didOpen` registration **back-fills**, walking the documents already open and sending one
    /// for every file the new selector newly matches. So a gem file the user is already looking at
    /// starts answering at registration rather than at the next tab switch.
    ///
    /// Additive, never a replacement: a client that declines keeps exactly what it has today.
    fn register_documents(&mut self, held: &[&PathBuf]) {
        // **Not `foreign_prefixes`**, which is a superset built for a different question — see the
        // list this is handed in `queue_background_indexing`.
        //
        // The workspace's own prefix is not among them either, and the filter is what keeps it
        // out. A vendored bundle at `vendor/bundle/ruby/<abi>` and a project's own
        // `.gem_rbs_collection/` are foreign *and* inside the root, so the selector the client was
        // built with already claims them — and claiming a file twice does not add anything, it
        // puts two providers over one document, which is one server answering the same hover
        // twice.
        let prefixes: Vec<String> = held
            .iter()
            .filter_map(|path| DocUri::from_path(path))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .filter(|prefix| !prefix.starts_with(&self.workspace_prefix))
            .collect();
        self.unregister_documents();
        if prefixes.is_empty() {
            return;
        }
        let registrations = (self.documents)(&prefixes);
        if registrations.is_empty() {
            if !self.documents_declined {
                self.documents_declined = true;
                tracing::info!("{}", messages::cannot_claim_foreign_files());
            }
            return;
        }
        tracing::info!(
            "asking the client for {} methods over {} roots outside the workspace",
            registrations.len(),
            prefixes.len()
        );
        self.documents_live = registrations
            .iter()
            .map(|registration| lsp_types::Unregistration {
                id: registration.id.clone(),
                method: registration.method.clone(),
            })
            .collect();
        // A string id in the server's own id space, as `register_file_watchers` and
        // `refresh_hints` use: the main loop reads the answer and logs a refusal, and there is
        // nothing else to do with either outcome.
        let _ = self.outgoing.send(Message::Request(Request {
            id: RequestId::from("ya-lsp/document-registration".to_owned()),
            method: "client/registerCapability".to_owned(),
            params: serde_json::json!({ "registrations": registrations }),
        }));
    }

    /// Take back the live document registrations, by the names they were made under.
    ///
    /// Silent and cheap in the case that happens once per process — there is nothing to take back
    /// at startup. The case it exists for is a reload: `[gems] enabled`, an `[rbs] path` or a
    /// changed workspace root all move the set of files this server has answers about, and
    /// re-registering an id the client already holds replaces its *record* of the registration
    /// without disposing the provider behind it. The old selector would go on answering beside
    /// the new one, which is the duplicate the filter in `register_documents` exists to avoid,
    /// arriving by a different route.
    fn unregister_documents(&mut self) {
        if self.documents_live.is_empty() {
            return;
        }
        let unregisterations = std::mem::take(&mut self.documents_live);
        let _ = self.outgoing.send(Message::Request(Request {
            id: RequestId::from("ya-lsp/document-unregistration".to_owned()),
            method: "client/unregisterCapability".to_owned(),
            params: serde_json::json!({ "unregisterations": unregisterations }),
        }));
    }

    fn show_warning(&self, message: &str) {
        let params = lsp_types::ShowMessageParams {
            typ: lsp_types::MessageType::WARNING,
            message: message.to_owned(),
        };
        let _ = self
            .outgoing
            .send(Message::Notification(lsp_server::Notification::new(
                "window/showMessage".to_owned(),
                params,
            )));
    }
}

/// The run loop itself, driven over the real channel by the real thread.
#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
pub(crate) mod testing;
#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod threaded_tests;

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn indexes_the_workspace_on_startup() {
        let mut harness = Harness::new();
        harness.write("lib/person.rb", "class Person\n  def shout\n  end\nend\n");

        harness.analysis.index_workspace();

        assert!(harness.has("Person"));
        assert!(harness.has("Person#shout()"));
        assert_eq!(harness.document_count(), 1);
    }

    #[test]
    fn editing_a_buffer_replaces_declarations_without_leaking_the_old_ones() {
        // Re-indexing the same URI must not accumulate.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.analysis.index_workspace();
        assert!(harness.has("Person#shout()"));

        harness.open(&uri, "class Person\n  def shout\n  end\nend\n");

        for iteration in 0..5 {
            harness.change(
                &uri,
                &format!("class Person\n  def whisper{iteration}\n  end\nend\n"),
            );
        }

        assert!(
            !harness.has("Person#shout()"),
            "the original method should be gone"
        );
        for iteration in 0..4 {
            assert!(
                !harness.has(&format!("Person#whisper{iteration}()")),
                "intermediate edit {iteration} leaked"
            );
        }
        assert!(harness.has("Person#whisper4()"));
        assert_eq!(
            harness.document_count(),
            1,
            "editing must not fork a second document"
        );
    }

    #[test]
    fn closing_a_buffer_reverts_to_what_is_on_disk() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.analysis.index_workspace();

        harness.open(&uri, "class Person\n  def unsaved\n  end\nend\n");
        assert!(harness.has("Person#unsaved()"));

        harness.run(Task::DidClose { uri });

        assert!(
            !harness.has("Person#unsaved()"),
            "unsaved edits must not survive the close"
        );
        assert!(
            harness.has("Person#shout()"),
            "the file is still part of the project, so its disk content must be indexed"
        );
        assert_eq!(harness.document_count(), 1);
    }

    #[test]
    fn closing_a_deleted_file_drops_it_from_the_graph() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.analysis.index_workspace();

        std::fs::remove_file(uri.to_path().unwrap()).unwrap();
        harness.run(Task::DidClose { uri });

        assert!(!harness.has("Person#shout()"));
        assert_eq!(harness.document_count(), 0);
    }

    #[test]
    fn a_change_for_an_unopened_buffer_is_still_applied() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", "class Person\nend\n");
        harness.analysis.index_workspace();

        harness.change(&uri, "class Person\n  def recovered\n  end\nend\n");

        assert!(harness.has("Person#recovered()"));
    }

    #[test]
    fn an_edit_reaches_the_buffer_at_once_and_the_graph_at_the_settle() {
        // `didChange` does two things and only the second is expensive: apply
        // the edit, which is microseconds, and put the document into rubydex, which is 265-305
        // ms for `app/models/user.rb` on discourse. The four requests `needs_the_graph` exempts
        // read the buffer and never the graph — and an editor sends `semanticTokens/full` after
        // every keystroke — so they were waiting on an index they do not read. Measured on
        // discourse before and after: **281 ms a keystroke, and 3-9 ms.**
        //
        // `handle` rather than `Harness::run`, which settles whenever the task left anything
        // dirty. That is the right default for every other test in this file and it is exactly
        // what this one is about, so the task goes in unaccompanied.
        let mut harness = Harness::new();
        let source = "class Story
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.open(&uri, source);

        harness.analysis.handle(Task::DidChange {
            uri: uri.clone(),
            changes: vec![TextChange {
                range: None,
                text: "class Story\n  def zzz_typed\n  end\nend\n".to_owned(),
            }],
            version: Some(2),
        });
        // `definitions_in` and not `has`: a declaration is the resolver's and would be absent
        // here whether the document was indexed or not.
        assert_eq!(
            harness.definitions_in(&uri),
            1,
            "the index was not deferred"
        );

        // A second keystroke before anything has asked for the first: both are deferred and the
        // document is recorded once, so a burst costs one index rather than one per character.
        harness.analysis.handle(Task::DidChange {
            uri: uri.clone(),
            changes: vec![TextChange {
                range: None,
                text: "class Story\n  def zzz_typed\n  end\n\n  def zzz_again\n  end\nend\n"
                    .to_owned(),
            }],
            version: Some(3),
        });
        assert_eq!(
            harness.analysis.pending_index.len(),
            1,
            "a burst queued one index per keystroke"
        );

        // The claim, and it is about `serve` rather than about the handler: a request that
        // never asks the graph a question is answered without settling, so the index stays
        // deferred across it.
        harness.ask(
            "textDocument/foldingRange",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );
        assert_eq!(
            harness.definitions_in(&uri),
            1,
            "a request that never reads the graph settled anyway"
        );

        // And the other half, which is what makes the deferral safe rather than merely cheap:
        // anything that does read the graph settles, and `settle` indexes what is pending
        // before it links anything over it.
        harness.ask(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );
        assert_eq!(
            harness.definitions_in(&uri),
            3,
            "a request that reads the graph did not get both edits"
        );
        assert!(
            harness.has("Story#zzz_typed()"),
            "and it is linked, not merely indexed"
        );
    }

    #[test]
    fn a_buffer_closed_before_its_deferred_index_ran_keeps_what_is_on_disk() {
        // The one ordering a deferred index adds that nothing else in this file can reach: an
        // edit is deferred, and the buffer is closed before the settle that would have indexed
        // it. `didClose` has
        // already put the file back to what disk says, so replaying the buffer over it would
        // undo exactly that — and the text it would replay is a version of the file the user
        // explicitly abandoned.
        let mut harness = Harness::new();
        let source = "class Story
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.open(&uri, source);

        harness.analysis.handle(Task::DidChange {
            uri: uri.clone(),
            changes: vec![TextChange {
                range: None,
                text: "class Story\n  def zzz_abandoned\n  end\nend\n".to_owned(),
            }],
            version: Some(2),
        });
        harness.run(Task::DidClose { uri: uri.clone() });

        assert_eq!(
            harness.definitions_in(&uri),
            1,
            "an abandoned buffer was replayed over the file on disk"
        );
        assert!(
            harness.has("Story"),
            "and the file on disk is still indexed"
        );
    }

    // -----------------------------------------------------------------------
    // The index and the disk
    // -----------------------------------------------------------------------

    #[test]
    fn a_file_written_while_the_server_runs_is_indexed_without_the_editor_opening_it() {
        // `git checkout`, `git pull`, a rebase, `rails g model` — every one of them writes Ruby
        // the editor never opened. Before this the file did not exist as far as the index was
        // concerned until someone restarted the server.
        let mut harness = Harness::new();
        let source = "Place.new\n";
        let main = harness.write("app/main.rb", source);
        harness.index();
        assert!(
            harness.definition_at(&main, source, "Place").is_null(),
            "nothing declares Place yet"
        );

        let place = harness.write("app/place.rb", "class Place\n  def name\n  end\nend\n");
        harness.watch(&[&place]);

        assert!(harness.has("Place#name()"));
        assert!(
            !harness.definition_at(&main, source, "Place").is_null(),
            "navigation follows the file that appeared, with no restart and no didOpen"
        );
    }

    #[test]
    fn a_file_rewritten_on_disk_replaces_what_the_index_held() {
        // The branch-switch case: same path, different declarations. Leaving the old ones in
        // is worse than not noticing at all, because navigation lands somewhere that is gone.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();
        assert!(harness.has("Person#shout()"));

        std::fs::write(
            uri.to_path().unwrap(),
            "class Person\n  def whisper\n  end\nend\n",
        )
        .unwrap();
        harness.watch(&[&uri]);

        assert!(
            !harness.has("Person#shout()"),
            "the old method must be gone"
        );
        assert!(harness.has("Person#whisper()"));
        assert_eq!(
            harness.document_count(),
            1,
            "re-indexing must not fork a second document"
        );
    }

    #[test]
    fn a_file_deleted_on_disk_is_dropped_and_takes_its_diagnostics_with_it() {
        // A deletion is the one change no other notification can stand in for: nothing else
        // ever tells a server that a declaration has gone. And `publishDiagnostics` is stateful
        // per URI, so a file that vanishes with squiggles on it keeps them on screen forever
        // unless something sends the empty set.
        let mut harness = Harness::new();
        let uri = harness.write("app/broken.rb", "class Broken\n  def oops(\nend\n");
        harness.index();
        assert!(harness.has("Broken"));
        assert!(!harness.latest(&uri).unwrap_or_default().is_empty());

        std::fs::remove_file(uri.to_path().unwrap()).unwrap();
        harness.watch(&[&uri]);

        assert!(!harness.has("Broken"));
        assert_eq!(harness.document_count(), 0);
        assert_eq!(
            harness.latest(&uri),
            Some(Vec::new()),
            "an explicit empty publish is the only thing that clears a squiggle"
        );
    }

    #[test]
    fn a_deletion_for_something_the_index_never_held_costs_nothing() {
        // Watchers are the client's, so a delete can name a path this server never indexed —
        // and `Workspace::indexes` cannot be asked about a path that is not there. The graph is
        // the thing that knows, and it answers for both halves of the question at once.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        harness.index();
        let absent = DocUri::from_path(&harness.root.path().join("app/never.rb")).unwrap();

        harness.watch(&[&absent]);

        assert!(harness.has("Person"));
        assert_eq!(harness.document_count(), 1);
    }

    #[test]
    fn an_open_buffer_is_not_clobbered_by_a_change_to_the_same_file_on_disk() {
        // A rebase under an open file must not overwrite what the editor is showing: the buffer
        // holds edits the disk has never seen, and the editor is still the authority on them
        // until it says otherwise. `didClose` already implements this precedence in the other
        // direction, which is what makes the buffer's disappearance the moment disk takes over.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();
        harness.open(&uri, "class Person\n  def unsaved\n  end\nend\n");
        assert!(harness.has("Person#unsaved()"));

        std::fs::write(
            uri.to_path().unwrap(),
            "class Person\n  def from_disk\n  end\nend\n",
        )
        .unwrap();
        harness.watch(&[&uri]);

        assert!(
            harness.has("Person#unsaved()"),
            "the buffer the editor is showing must survive the disk change"
        );
        assert!(!harness.has("Person#from_disk()"));

        // And the moment the buffer goes, disk is the truth again — through the path that
        // already existed for it.
        harness.run(Task::DidClose { uri });
        assert!(harness.has("Person#from_disk()"));
    }

    #[test]
    fn every_notification_says_it_arrived_and_what_it_was_about() {
        // A notification changes what every later answer is made of, and none of them said so.
        // The explanation for an answer somebody is about to report as wrong is very often here
        // — a `didChange` that was dropped, a reload that threw the graph away — and until now
        // none of it was written down anywhere.
        let mut harness = Harness::new();
        let source = "class Story\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let (_, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.run(Task::DidOpen {
                uri: uri.clone(),
                text: source.to_owned(),
                version: Some(1),
            });
            harness.run(Task::DidSave { uri: uri.clone() });
            harness.run(Task::DidClose { uri: uri.clone() });
        });

        for (kind, count) in [("didOpen", 1), ("didSave", 1), ("didClose", 1)] {
            assert_eq!(
                logged
                    .lines()
                    .filter(|line| line.contains(&format!("notification kind=\"{kind}\"")))
                    .count(),
                count,
                "{kind}: {logged}"
            );
        }
        assert!(
            logged.contains("app/models/story.rb\""),
            "a notification names the document it is about: {logged}"
        );
        assert!(
            !logged.contains("notification kind=\"request\""),
            "a request writes its own pair; a third line would say it less precisely: {logged}"
        );
    }

    #[test]
    fn a_configuration_reload_says_which_tables_the_file_moved() {
        // "It is reading my ya-lsp.toml" and "it is reading my ya-lsp.toml and every key in it
        // is already the default" produce identical behaviour and identical logs, and the second
        // is what somebody who has just mistyped a table name has.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.index();

        std::fs::write(
            harness.root.path().join("ya-lsp.toml"),
            "[types]\nguess_from_names = false\n",
        )
        .unwrap();
        let (_, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            harness.run(Task::ReloadConfig);
        });
        // `rails` is on the list too, because the harness sends `rails.enabled = true` where the
        // default is `auto` — which is the line doing its job: that *is* a setting somebody set.
        assert!(logged.contains("types"), "{logged}");

        // And a file that says nothing the defaults do not already say does not appear on it.
        std::fs::write(
            harness.root.path().join("ya-lsp.toml"),
            "[types]\nguess_from_names = true\n",
        )
        .unwrap();
        let (_, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            harness.run(Task::ReloadConfig);
        });
        assert!(logged.contains("reloaded configuration"), "{logged}");
        // And the detection says which way it went **after** the log has been re-pointed, which
        // is the whole reason that line is not written where the decision is made: a reload that
        // has just turned the file on has to hold the reload that turned it on.
        assert!(logged.contains("rails knowledge"), "{logged}");
        assert!(
            !logged.contains("types"),
            "a value equal to the default is not a change: {logged}"
        );
    }

    #[test]
    fn a_watched_change_the_workspace_does_not_index_is_ignored() {
        // A client's watchers are shared across every server it runs and every registration
        // each one made, so anything can arrive here. Indexing it would put files in the graph
        // that `index.exclude` says are out — and the walk and this path disagreeing is the one
        // failure neither the user nor the log would ever show.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        harness.index();

        let excluded = harness.write("tmp/generated.rb", "class Generated\nend\n");
        let not_ruby = harness.write("app/notes.md", "class NotRuby; end\n");
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("other.rb"), "class Other\nend\n").unwrap();
        let outside = DocUri::from_path(&elsewhere.path().join("other.rb")).unwrap();

        harness.watch(&[&excluded, &not_ruby, &outside]);

        assert!(!harness.has("Generated"), "index.exclude still applies");
        assert!(!harness.has("NotRuby"), "index.include still applies");
        assert!(!harness.has("Other"), "another project is not this one");
        assert_eq!(harness.document_count(), 1);
    }

    #[test]
    fn a_file_that_is_there_but_cannot_be_read_keeps_what_the_index_already_had() {
        // The gap between `is_file` and reading it: a half-written file mid-checkout, a
        // permission, or — portably testable — bytes that are not UTF-8. Dropping the document
        // would be the worse answer of the two, since the old declarations are at least the
        // ones that were true a moment ago, and the next write brings another notification.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();

        std::fs::write(uri.to_path().unwrap(), b"class Person\n  def \xff\nend\n").unwrap();
        let (_, logged) = crate::testing::captured_logs(tracing::Level::WARN, || {
            harness.watch(&[&uri]);
        });

        assert!(harness.has("Person#shout()"), "the last good index is kept");
        assert!(logged.contains("after a watched change"), "{logged}");
    }

    #[test]
    fn a_crash_while_linking_the_graph_rebuilds_instead_of_killing_the_server() {
        // rubydex 0.2.5 panics in `Resolver::resolve` after a document is deleted, and there is
        // no published version to upgrade to. Uncaught, the analysis thread dies and the server
        // answers nothing at all forever — which looks exactly like a server that is thinking,
        // so nobody restarts it. Reproduced on a real project by deleting one file from a
        // solargraph v0.58.2 checkout; what this pins is that ya-lsp comes back from it.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();
        assert!(harness.has("Person#shout()"));

        RESOLVES_TO_CRASH.set(1);
        let uri = harness.write("app/place.rb", "class Place\nend\n");
        harness.watch(&[&uri]);
        assert_eq!(RESOLVES_TO_CRASH.get(), 0, "the crash was armed and taken");

        assert!(
            harness.has("Person#shout()") && harness.has("Place"),
            "the rebuild has to put the whole workspace back, not only what was asked for"
        );
        let said = harness.messages();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("something went wrong while linking"),
            "{said:?}"
        );
    }

    #[test]
    fn a_rebuild_that_crashes_again_stops_rather_than_recurring() {
        // The rebuild indexes the workspace and indexing resolves, so without a guard a project
        // that cannot be linked at all would rebuild itself forever and never answer anything.
        // Degrading to whatever was linked is the worse index and the better server.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        harness.index();

        RESOLVES_TO_CRASH.set(5);
        let (_, logged) = crate::testing::captured_logs(tracing::Level::ERROR, || {
            let uri = harness.write("app/place.rb", "class Place\nend\n");
            harness.watch(&[&uri]);
        });

        assert_eq!(
            RESOLVES_TO_CRASH.replace(0),
            3,
            "exactly two resolves should have been attempted: the first, and one rebuild"
        );
        assert!(logged.contains("crashed again during recovery"), "{logged}");
    }

    // -----------------------------------------------------------------------
    // The bulkhead
    // -----------------------------------------------------------------------

    /// The same bug arrives through a keystroke, and that half is inline on the analysis
    /// thread with no worker under it.
    ///
    /// It reads as a startup problem and is not one: the file need never be on disk, and this
    /// is the half a user meets while working rather than while waiting — so what is pinned is both halves of the answer: the rest of the workspace goes
    /// on answering, and *this* document keeps what it had before the edit rather than emptying.
    #[test]
    fn five_lines_typed_into_a_buffer_leave_the_workspace_answering() {
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        let source = "class Place\n  def name\n  end\nend\n";
        let uri = harness.write("app/place.rb", source);
        harness.index();
        harness.open(&uri, source);
        let _ = harness.messages();

        harness.change(&uri, indexer::CRASHES);
        harness.analysis.settle();

        assert!(harness.has("Person#shout()"), "the workspace still answers");
        assert!(
            harness.has("Place#name()"),
            "the graph is never entered, so the buffer keeps the version it had"
        );
        assert!(harness.analysis.skipped.contains(&uri));
        assert_eq!(harness.messages().len(), 1);
    }

    /// The buffer route retries on every keystroke, which is what makes the skip list
    /// self-clearing — and what would make a notification per character if `record_skip` did
    /// not say it once.
    #[test]
    fn a_buffer_that_stays_broken_is_told_about_once() {
        let mut harness = Harness::new();
        let uri = harness.write("app/place.rb", "class Place\nend\n");
        harness.index();
        harness.open(&uri, "class Place\nend\n");
        let _ = harness.messages();

        for trailing in 0..4 {
            harness.change(
                &uri,
                &format!("{}{}\n", indexer::CRASHES, "#".repeat(trailing)),
            );
        }
        harness.analysis.settle();

        assert_eq!(
            harness.messages().len(),
            1,
            "said once, not once per keystroke"
        );
    }

    /// The one thing `rebuild` does not clear, and the reason is the recovery it is part of.
    ///
    /// `resolve`'s crash recovery rebuilds, and a rebuild runs the workspace walk — so a
    /// workspace holding a file that crashes the indexer would take the recovery down with it
    /// if the skip list went the way of the eight caches above it.
    #[test]
    fn the_skip_list_survives_a_rebuild() {
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        let bad = harness.write("app/rice.rb", indexer::CRASHES);
        harness.index();
        let _ = harness.messages();

        let (_, logged) = crate::testing::captured_logs(tracing::Level::WARN, || {
            harness.run(Task::ReloadConfig);
        });

        assert!(
            !logged.contains("crashed"),
            "the walk knows not to read it again: {logged}"
        );
        assert!(harness.analysis.skipped.contains(&bad));
        assert!(harness.has("Person#shout()"), "and the rebuild still works");
        assert!(
            harness.messages().is_empty(),
            "and says nothing a second time"
        );
    }

    /// The other half of "not permanent either": nothing has to decide what counts as a fix.
    ///
    /// Every route that indexes because the text may have moved simply tries again, so the
    /// entry goes when the file works — and the user does not restart the server.
    #[test]
    fn a_file_that_is_fixed_is_indexed_again_without_a_restart() {
        let mut harness = Harness::new();
        let bad = harness.write("app/rice.rb", indexer::CRASHES);
        harness.index();
        assert!(harness.analysis.skipped.contains(&bad));

        std::fs::write(
            bad.to_path().unwrap(),
            "class Rice\n  def cook\n  end\nend\n",
        )
        .unwrap();
        harness.watch(&[&bad]);

        assert!(harness.has("Rice#cook()"));
        assert!(harness.analysis.skipped.is_empty());
    }

    /// A gem's files take the other bulk route, and the file that provokes this is in a gem
    /// rather than in anybody's application.
    #[test]
    fn a_gem_file_that_crashes_the_indexer_costs_that_file_and_not_the_bundle() {
        let (dir, gem_home, env) = project_with_gem(indexer::CRASHES);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.index();
        harness.index_gems();

        let bad = DocUri::from_path(&gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .expect("an absolute path");
        assert!(harness.analysis.skipped.contains(&bad));
        let said = harness.messages();
        assert!(
            said.iter()
                .any(|message| message.starts_with("something went wrong while reading")),
            "a gem's gap is still a gap the user can see: {said:?}"
        );
    }

    #[test]
    fn a_watched_change_inside_a_bundle_is_left_to_the_gem_index() {
        // `bundle install` rewrites tens of thousands of files at once. Answering that on the
        // analysis thread, one `index_source` at a time, is exactly the stall the background
        // gem index exists to avoid — so a gem is not the user's code even when the include
        // globs would have taken it, which is what a bundle vendored outside `vendor/` does.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("ya-lsp.toml"),
            "[index]\nexclude = []\n\n[gems]\npaths = [\"bundle\"]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Gemfile.lock"),
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    shouty (1.2.3)\n",
        )
        .unwrap();
        let gem = root.join("bundle/gems/shouty-1.2.3/lib/shouty.rb");
        std::fs::create_dir_all(gem.parent().unwrap()).unwrap();
        std::fs::write(&gem, "module Shouty\nend\n").unwrap();

        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, gems::Env::default());
        harness.write("app/main.rb", "class Mine\nend\n");
        harness.index();
        harness.index_gems();
        let before = harness.document_count();

        let gem_uri = DocUri::from_path(&gem).unwrap();
        assert!(
            harness.analysis.workspace.indexes(&gem),
            "the globs do take it — otherwise this proves nothing about is_own_code"
        );
        std::fs::write(&gem, "module Shouty\n  class Rewritten\n  end\nend\n").unwrap();
        harness.watch(&[&gem_uri]);

        assert!(
            !harness.has("Shouty::Rewritten"),
            "a change under a gem root is the gem index's business, not the watcher's"
        );
        assert_eq!(harness.document_count(), before);
    }

    #[test]
    fn the_index_cap_still_applies_to_a_file_created_after_the_walk() {
        // `index.max_files` exists because a pathological repository exists, and a watcher can
        // add the files the walk stopped before. Said once rather than per branch switch: the
        // condition does not change between them, and a notification that repeats forever is
        // one people learn to dismiss without reading.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[index]\nmax_files = 1\n\n[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/person.rb", "class Person\nend\n");
        harness.index();
        assert!(harness.messages().is_empty(), "the walk itself fitted");

        let second = harness.write("app/place.rb", "class Place\nend\n");
        harness.watch(&[&second]);
        assert!(!harness.has("Place"));
        let said = harness.messages();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("index.max_files (1)"), "{said:?}");

        let third = harness.write("app/thing.rb", "class Thing\nend\n");
        harness.watch(&[&third]);
        assert!(!harness.has("Thing"));
        assert!(
            harness.messages().is_empty(),
            "said once, not once per file"
        );

        // And the budget is a count, not a high-water mark: deleting the file that filled it
        // makes room for the next one.
        std::fs::remove_file(harness.root.path().join("app/person.rb")).unwrap();
        let person = DocUri::from_path(&harness.root.path().join("app/person.rb")).unwrap();
        harness.watch(&[&person, &second]);
        assert!(!harness.has("Person"));
        assert!(harness.has("Place"), "the freed slot is usable");
    }

    #[test]
    fn a_settings_change_takes_effect_without_a_restart_and_the_file_still_wins() {
        // The editor's settings are a layer, not the truth: `ya-lsp.toml` is committed so a
        // whole team gets the same behaviour whatever editor they use, and it has to keep
        // outranking whatever one person has in their own preferences.
        let mut harness = Harness::new();
        harness.write("lib/person.rb", "class Person\nend\n");
        harness.write("spec/person_spec.rb", "class PersonSpec\nend\n");
        harness.analysis.index_workspace();
        assert!(harness.has("PersonSpec"), "indexed by default");

        harness.run(Task::ChangeConfig {
            options: Some(serde_json::json!({ "index": { "exclude": ["spec/**/*"] } })),
        });
        assert!(!harness.has("PersonSpec"), "the client's settings applied");

        // A project file that says something different wins, and keeps winning across a later
        // settings change that does not mention the same key.
        std::fs::write(
            harness
                .root
                .path()
                .join(crate::workspace::config::CONFIG_FILE_NAME),
            "[index]\nexclude = []\n",
        )
        .unwrap();
        harness.run(Task::ChangeConfig {
            options: Some(serde_json::json!({ "gems": { "enabled": false } })),
        });
        assert!(harness.has("PersonSpec"), "ya-lsp.toml outranks the editor");

        // And dropping the layer altogether goes back to the server's own defaults.
        harness.run(Task::ChangeConfig { options: None });
        assert!(harness.has("PersonSpec"));
    }

    #[test]
    fn config_reload_reindexes_and_replays_open_buffers() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", "class Person\nend\n");
        harness.write("spec/person_spec.rb", "class PersonSpec\nend\n");
        harness.analysis.index_workspace();
        assert!(harness.has("PersonSpec"), "indexed by default");

        harness.open(&uri, "class Person\n  def in_buffer\n  end\nend\n");

        std::fs::write(
            harness
                .root
                .path()
                .join(crate::workspace::config::CONFIG_FILE_NAME),
            "[index]\nexclude = [\"spec/**/*\"]\n",
        )
        .unwrap();
        harness.run(Task::ReloadConfig);

        assert!(!harness.has("PersonSpec"), "newly excluded");
        assert!(
            harness.has("Person#in_buffer()"),
            "the open buffer must be replayed over the fresh index"
        );
    }

    // -----------------------------------------------------------------------
    // Diagnostics
    // -----------------------------------------------------------------------

    #[test]
    fn a_syntax_error_is_published_as_an_error() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/broken.rb", UNTERMINATED);

        harness.index();

        let items = harness
            .latest(&uri)
            .expect("diagnostics for the broken file");
        let errors: Vec<_> = items
            .iter()
            .filter(|item| item.code == code("parse-error"))
            .collect();
        assert!(!errors.is_empty(), "{items:?}");
        assert!(
            errors
                .iter()
                .all(|item| item.severity == Some(DiagnosticSeverity::ERROR)),
            "{items:?}"
        );
        assert!(
            items
                .iter()
                .all(|item| item.source.as_deref() == Some("ya-lsp")),
            "{items:?}"
        );
        // Prism also emits an indentation warning for this fixture, and it must arrive under its
        // own rule so `[diagnostics.rules]` can silence it independently.
        assert!(
            items.iter().any(|item| item.code == code("parse-warning")
                && item.severity == Some(DiagnosticSeverity::WARNING)),
            "{items:?}"
        );
        // Sorted by position, so the first diagnostic is the one at the top of the file.
        assert_eq!(items[0].range.start, lsp_types::Position::new(0, 0));
    }

    #[test]
    fn fixing_the_file_publishes_an_empty_set_rather_than_going_quiet() {
        // The failure this guards is the classic one: diagnostics that never clear. LSP keeps
        // whatever was last sent for a URI on screen forever, so silence is not a retraction.
        let mut harness = Harness::new();
        let uri = harness.write("lib/broken.rb", UNTERMINATED);
        harness.index();
        assert!(!harness.latest(&uri).expect("published").is_empty());

        harness.open(&uri, "class Foo\n  def bar\n  end\nend\n");

        let items = harness
            .latest(&uri)
            .expect("an explicit empty publish, not silence");
        assert!(items.is_empty(), "{items:?}");
    }

    #[test]
    fn an_unchanged_set_is_not_republished() {
        // Otherwise every keystroke anywhere in the project re-sends diagnostics for every file
        // that has any, which is the whole reason the publisher diffs.
        let mut harness = Harness::new();
        let broken = harness.write("lib/broken.rb", UNTERMINATED);
        let other = harness.write("lib/fine.rb", "class Fine\nend\n");
        harness.index();
        assert!(harness.latest(&broken).is_some());

        harness.open(&other, "class Fine\n  def added\n  end\nend\n");

        assert!(
            harness.published().is_empty(),
            "editing an unrelated file must not re-send the broken file's diagnostics"
        );
    }

    #[test]
    fn disabling_diagnostics_clears_what_was_already_published() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/broken.rb", UNTERMINATED);
        harness.index();
        assert!(!harness.latest(&uri).expect("published").is_empty());

        std::fs::write(
            harness
                .root
                .path()
                .join(crate::workspace::config::CONFIG_FILE_NAME),
            "[diagnostics]\nenabled = false\n",
        )
        .unwrap();
        harness.run(Task::ReloadConfig);

        let items = harness.latest(&uri).expect("an empty publish, not silence");
        assert!(items.is_empty(), "{items:?}");
    }

    #[test]
    fn a_resolution_diagnostic_reaches_the_document_its_declaration_lives_in() {
        // Resolution diagnostics hang off declarations rather than documents, so the uri_id
        // lookup is the only thing that puts them in the right file. The rule ships off (it
        // fires on correct Ruby), so this turns it on — which exercises the config path too.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join(crate::workspace::config::CONFIG_FILE_NAME),
            "[diagnostics.rules]\nundefined-constant-visibility-target = \"information\"\n",
        )
        .unwrap();
        let mut harness = Harness::at(root, PositionEncoding::Utf16);
        let uri = harness.write(
            "lib/thing.rb",
            "class Thing\n  private_constant :MISSING\nend\n",
        );
        harness.index();

        let items = harness.latest(&uri).expect("published");
        let item = items
            .iter()
            .find(|item| item.code == code("undefined-constant-visibility-target"))
            .unwrap_or_else(|| panic!("{items:?}"));
        assert_eq!(item.severity, Some(DiagnosticSeverity::INFORMATION));
        assert_eq!(item.range.start.line, 1, "second line");
    }

    #[test]
    fn ranges_are_in_the_negotiated_encoding() {
        // rubydex hands us UTF-8 byte offsets and `Offset::to_location` only ever returns UTF-8
        // columns, so a diagnostic sitting *after* a wide character is where a naive mapping
        // silently lands in the wrong place. Two emoji are 8 UTF-8 bytes but 4 UTF-16 units.
        let source = "def thing\n  \"\u{1f600}\u{1f600}\"; unused = 1\nend\n";

        let mut utf16 = Harness::with_encoding(PositionEncoding::Utf16);
        let uri = utf16.write("lib/thing.rb", source);
        utf16.index();
        let wide = utf16.latest(&uri).expect("published");

        let mut utf8 = Harness::with_encoding(PositionEncoding::Utf8);
        let uri8 = utf8.write("lib/thing.rb", source);
        utf8.index();
        let narrow = utf8.latest(&uri8).expect("published");

        let unused = |items: &[lsp_types::Diagnostic]| {
            items
                .iter()
                .find(|item| item.message.contains("unused"))
                .unwrap_or_else(|| panic!("{items:?}"))
                .range
                .start
        };
        let wide = unused(&wide);
        let narrow = unused(&narrow);

        assert_eq!(wide.line, 1);
        assert_eq!(narrow.line, 1);
        // `  "` is 3 units, then the emoji: 4 UTF-16 vs 8 UTF-8, then `"; ` is 3 more.
        assert_eq!(wide.character, 10, "utf-16 column");
        assert_eq!(narrow.character, 14, "utf-8 column");
    }

    #[test]
    fn files_outside_the_workspace_root_are_not_published() {
        // Nobody can fix a warning inside somebody else's gem, and a Rails app would
        // bury the user's own problems under thousands of them.
        let mut harness = Harness::new();
        harness.index();

        let outside = tempfile::tempdir().expect("tempdir");
        let path = outside.path().join("vendored.rb");
        std::fs::write(&path, UNTERMINATED).unwrap();
        let uri = DocUri::from_path(&path).unwrap();

        harness.open(&uri, UNTERMINATED);

        assert!(
            harness.latest(&uri).is_none(),
            "a file outside the workspace must not be published"
        );
    }

    #[test]
    fn an_unknown_rule_name_in_config_is_reported() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join(crate::workspace::config::CONFIG_FILE_NAME),
            "[diagnostics.rules]\nparse_error = \"off\"\n",
        )
        .unwrap();
        let mut harness = Harness::at(root, PositionEncoding::Utf16);

        harness.index();

        let warnings: Vec<String> = harness
            .outgoing
            .try_iter()
            .filter_map(|message| match message {
                Message::Notification(notification)
                    if notification.method == "window/showMessage" =>
                {
                    serde_json::from_value::<lsp_types::ShowMessageParams>(notification.params)
                        .ok()
                        .map(|params| params.message)
                }
                _ => None,
            })
            .collect();
        assert!(
            warnings
                .iter()
                .any(|message| message.contains("parse_error") && message.contains("parse-error")),
            "a typo must name both the mistake and the right spelling: {warnings:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Gem indexing
    // -----------------------------------------------------------------------

    #[test]
    fn a_change_to_a_buffer_that_was_never_opened_is_recovered_only_when_it_is_safe() {
        // Clients do occasionally get this wrong. An incremental range only means anything
        // against the exact text it was computed from, so applying one to an empty buffer is
        // worse than dropping it — but a whole-buffer change carries its own base and can be
        // treated as the `didOpen` that never arrived.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\nend\n");
        harness.index();

        harness.run(Task::DidChange {
            uri: uri.clone(),
            changes: vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 6,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 12,
                    },
                }),
                text: "Ghost".to_owned(),
            }],
            version: Some(2),
        });
        assert!(
            harness.has("Person"),
            "an incremental edit against no buffer must be dropped"
        );
        assert!(!harness.has("Ghost"));

        harness.run(Task::DidChange {
            uri: uri.clone(),
            changes: vec![TextChange {
                range: None,
                text: "class Ghost\nend\n".to_owned(),
            }],
            version: Some(3),
        });
        assert!(
            harness.has("Ghost"),
            "a whole buffer can stand in for an open"
        );
    }

    #[test]
    fn closing_a_buffer_falls_back_to_disk_and_forgets_a_file_that_is_gone() {
        // Closing an editor tab does not remove the file from the project. The graph has to
        // return to what is on disk — and only drop the document when there is no disk copy
        // left, which is what a rename or a delete looks like from here.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\nend\n");
        harness.index();

        harness.open(&uri, "class Person\n  def shout\n  end\nend\n");
        assert!(harness.has("Person#shout()"), "the buffer shadows disk");

        harness.run(Task::DidClose { uri: uri.clone() });
        assert!(harness.has("Person"), "the file is still in the project");
        assert!(
            !harness.has("Person#shout()"),
            "the unsaved method is not on disk and must not survive the close"
        );

        harness.open(&uri, "class Person\nend\n");
        std::fs::remove_file(uri.to_path().expect("a path")).unwrap();
        harness.run(Task::DidClose { uri: uri.clone() });
        assert!(
            !harness.has("Person"),
            "a file that is gone from disk is gone from the graph"
        );
    }

    #[test]
    fn saving_changes_nothing_because_the_buffer_was_already_indexed() {
        // `didSave` arrives after every `didChange` for the same text. Re-indexing here would
        // double the work of typing for no new information at all.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\nend\n");
        harness.index();
        harness.open(&uri, "class Person\n  def shout\n  end\nend\n");
        let before = harness.latest(&uri);

        harness.run(Task::DidSave { uri: uri.clone() });

        assert!(harness.has("Person#shout()"), "the buffer is still indexed");
        assert_eq!(harness.latest(&uri), before, "nothing was re-published");
    }

    #[test]
    fn a_reload_that_breaks_the_config_warns_and_keeps_serving() {
        // `ya-lsp.toml` is edited by hand and saved half-written. The server has to say so and
        // carry on with the defaults; going quiet is indistinguishable from a crash.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\nend\n");
        harness.index();
        let _ = harness.messages();

        std::fs::write(
            harness.root.path().join("ya-lsp.toml"),
            "[index]\ninclude = [\n",
        )
        .unwrap();
        harness.run(Task::ReloadConfig);

        let messages = harness.messages();
        assert!(
            messages.iter().any(|shown| shown.contains("ya-lsp.toml")),
            "{messages:?}"
        );
        assert!(
            harness.has("Person"),
            "the workspace is re-indexed with the defaults"
        );
        assert!(
            uri.as_str().ends_with("person.rb"),
            "the document keeps its identity across a reload"
        );
    }

    #[test]
    fn changed_editor_settings_reload_the_config_without_a_file() {
        // The editor's layer never touches the filesystem, so it arrives carried rather than
        // re-read — and it still has to rebuild the graph, because it can change what is
        // indexed at all.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        harness.write("spec/person_spec.rb", "class PersonSpec\nend\n");
        harness.index();
        assert!(harness.has("PersonSpec"));

        harness.run(Task::ChangeConfig {
            options: Some(serde_json::json!({ "index": { "exclude": ["spec/**/*"] } })),
        });

        assert!(harness.has("Person"), "the project is still indexed");
        assert!(
            !harness.has("PersonSpec"),
            "the new exclude glob is in effect"
        );
    }

    #[test]
    fn gem_indexing_stops_at_max_files_and_says_so() {
        // Silence here is the worst outcome: half a bundle indexed looks exactly like a bundle
        // where the gem you wanted was never installed.
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\nmax_files = 0\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        let _ = harness.messages();

        harness.index_gems();

        assert_eq!(
            harness.messages(),
            vec![messages::gem_index_truncated(0)],
            "the message names the setting to raise, in the spelling ya-lsp.toml takes"
        );
        assert!(
            !harness.has("Shouty"),
            "nothing was indexed, which is what the warning is about"
        );
    }

    /// A gem's source lives outside every workspace folder, so the server has to claim it itself.
    ///
    /// The client's document selector is the only gate on what it ever sends, and it cannot name a
    /// gem root without a second copy of `workspace::gems` in TypeScript. So the server says where
    /// its answers are, over the same channel the file watcher is registered on. Before this,
    /// `definition` jumped into `activerecord-8.1.3.1/lib/active_record.rb` and every request in
    /// the file it had just opened was dead, with nothing logged, because nothing was asked.
    #[test]
    fn the_client_is_asked_to_claim_the_gem_roots_and_not_the_workspace() {
        let (dir, gem_home, env) = project_with_gem("module Shouty\nend\n");
        let workspace = DocUri::from_path(dir.path())
            .expect("a workspace URI")
            .as_str()
            .to_owned();
        let gems = DocUri::from_path(gem_home.path())
            .expect("a gem home URI")
            .as_str()
            .to_owned();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();

        harness.index_gems();

        let sent = harness.requests("client/registerCapability");
        assert_eq!(
            sent.len(),
            1,
            "one batch: a client rejects the whole array on the first method it cannot place"
        );
        let registrations = sent[0].params["registrations"]
            .as_array()
            .expect("a list of registrations");
        let selector = &registrations[0]["registerOptions"]["documentSelector"];
        assert!(
            registrations.iter().all(
                |registration| &registration["registerOptions"]["documentSelector"] == selector
            ),
            "one selector, or a request answers over a different set of files than its neighbour"
        );
        let bases: Vec<String> = selector
            .as_array()
            .expect("every registration carries its own selector")
            .iter()
            .map(|filter| filter["pattern"]["baseUri"].as_str().unwrap().to_owned())
            .collect();

        // **The gem's own directory, not the directory it was found in.** `Gems::roots` holds
        // every gem path that exists on this machine — every Ruby installed here, and the system's
        // own — and asking the editor to claim all of them would claim every Ruby file under every
        // bundle on the machine. It is also what makes the arbitration in the extension mean
        // something: two folders on one Ruby with *different* bundles claim different gems, and a
        // gem only one of them locked stays with that one.
        assert_eq!(
            bases
                .iter()
                .filter(|base| base.ends_with("gems/shouty-1.2.3"))
                .count(),
            crate::server::capabilities::LANGUAGE_IDS.len(),
            "one filter per language, naming the gem the lockfile resolved: {bases:?}"
        );
        assert!(
            bases.iter().all(|base| base != &format!("{gems}/gems")),
            "the search root holds gems this bundle never locked: {bases:?}"
        );
        assert!(
            bases.iter().all(|base| !base.starts_with(&workspace)),
            "the client already claimed the workspace, and claiming it twice puts two providers \
             over one document: {bases:?}"
        );
        assert!(
            registrations
                .iter()
                .any(|registration| registration["method"] == "textDocument/didOpen"),
            "without didOpen the client never tells the server the file exists at all"
        );
    }

    /// A gem file the client opens answers like any other file, which is the claim's other half.
    ///
    /// The server was never the part that was broken — the bundle is already in the graph, so the
    /// only thing missing was ever being asked. This is what makes the registration worth sending:
    /// `didOpen` a gem's source and all three of the requests a user reaches for inside one answer.
    #[test]
    fn a_gem_file_the_client_opens_answers_like_any_other() {
        const SOURCE: &str =
            "module Shouty\n  class Megaphone\n    def shout\n      Shouty\n    end\n  end\nend\n";
        let (dir, gem_home, env) = project_with_gem(SOURCE);
        let gem_file = DocUri::from_path(&gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .expect("a gem file URI");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        harness.index_gems();

        harness.open(&gem_file, SOURCE);

        let outline = harness.outline(&gem_file);
        assert_eq!(
            all_symbols(&outline)
                .iter()
                .filter_map(|symbol| symbol["name"].as_str())
                .collect::<Vec<_>>(),
            vec!["Shouty", "Megaphone", "shout"],
            "the outline of a file that used to be dead the moment `definition` landed in it"
        );
        assert!(
            !harness.hover_at(&gem_file, SOURCE, "Megaphone").is_null(),
            "hover inside a gem"
        );
        // A `LocationLink`, because the harness's client takes them; the point is the URI.
        assert_eq!(
            harness.definition_at(&gem_file, SOURCE, "Shouty\n    end")[0]["targetUri"],
            serde_json::json!(gem_file.as_str()),
            "and a jump that lands in the gem it started in"
        );
    }

    /// A vendored bundle is already claimed, and asking for it again answers every hover twice.
    ///
    /// `vendor/bundle/ruby/<abi>` is inside the workspace root by construction, so it is *foreign*
    /// — nobody can fix a warning in it — and *claimed*, by the selector the client was built with.
    /// The two questions have different answers and only one of them decides this.
    #[test]
    fn a_bundle_vendored_inside_the_project_is_not_claimed_a_second_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    shouty (1.2.3)\n",
        )
        .unwrap();
        let gem_home = dir.path().join("vendor/bundle/ruby/4.0.0");
        let file = gem_home.join("gems/shouty-1.2.3/lib/shouty.rb");
        std::fs::create_dir_all(file.parent().expect("a parent")).unwrap();
        std::fs::write(&file, "module Shouty\nend\n").unwrap();
        std::fs::create_dir_all(gem_home.join("specifications")).unwrap();
        std::fs::write(
            gem_home.join("specifications/shouty-1.2.3.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = [\"lib\".freeze]\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let env = gems::Env {
            gem_home: Some(gem_home),
            ..gems::Env::default()
        };
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();

        harness.index_gems();

        assert!(
            harness.has("Shouty"),
            "the vendored gem is indexed, which is what makes the question worth asking"
        );
        assert!(
            harness.requests("client/registerCapability").is_empty(),
            "every root is inside the workspace, so there is nothing the client has not claimed"
        );
    }

    /// A reload takes the old registrations back by name before making new ones.
    ///
    /// Re-registering an id the client already holds replaces its *record* of the registration
    /// without disposing the provider behind it, so the old selector goes on answering beside the
    /// new one — the same duplicate the workspace filter avoids, arriving by another route. The ids
    /// are fixed strings for exactly this: something has to be able to name them again.
    #[test]
    fn a_reload_takes_the_document_registrations_back_before_remaking_them() {
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        harness.index_gems();
        let first = harness.requests("client/registerCapability");
        assert_eq!(first.len(), 1);
        let ids: Vec<String> = first[0].params["registrations"]
            .as_array()
            .expect("registrations")
            .iter()
            .map(|registration| registration["id"].as_str().unwrap().to_owned())
            .collect();
        assert!(harness.requests("client/unregisterCapability").is_empty());

        harness.run(Task::ReloadConfig);
        while harness.analysis.step_gem_indexing() {}

        let taken_back = harness.requests("client/unregisterCapability");
        assert_eq!(
            taken_back.len(),
            1,
            "once, and before the second registration"
        );
        let released: Vec<String> = taken_back[0].params["unregisterations"]
            .as_array()
            .expect("the protocol's own spelling, typo included")
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(released, ids, "by the names they were made under");
        assert_eq!(
            harness.requests("client/registerCapability").len(),
            1,
            "and then asked for again, because a reload can move every root"
        );
    }

    /// A client that takes no dynamic registration keeps exactly what it has, and is told once.
    ///
    /// Silence is the expensive outcome here: the symptom is one file answering nothing while every
    /// file beside it answers, which reads as the server being wrong rather than the server never
    /// having been asked. Once per process and not once per reload — a `ya-lsp.toml` saved five
    /// times would otherwise repeat the sentence five times.
    #[test]
    fn a_client_that_takes_no_registration_is_told_once_and_keeps_what_it_has() {
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.takes_no_registration();
        harness.write("app/main.rb", "Shouty\n");
        harness.index();

        let (_, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            harness.index_gems();
            harness.run(Task::ReloadConfig);
            harness.index_gems();
        });

        assert!(
            harness.requests("client/registerCapability").is_empty(),
            "nothing is asked of a client that cannot answer the question"
        );
        assert_eq!(
            logged.matches("cannot be asked to claim files").count(),
            1,
            "said once for the session, across two passes over the bundle: {logged}"
        );
        assert!(
            harness.has("Shouty"),
            "the gem is still indexed; what is lost is the client ever asking about it"
        );
    }

    #[test]
    fn diagnostics_are_skipped_for_a_file_that_has_gone_from_disk() {
        // The declaration still carries its diagnostic after the file is deleted. Placing a
        // range needs the text, and squiggles in guessed positions are worse than none.
        let mut harness = Harness::new();
        let uri = harness.write("app/broken.rb", "class Broken\n  def oops(\nend\n");
        harness.index();
        assert!(
            harness.latest(&uri).is_some_and(|items| !items.is_empty()),
            "the syntax error is reported while the file is there"
        );

        std::fs::remove_file(uri.to_path().expect("a path")).unwrap();
        harness.analysis.publish_diagnostics();

        // Not silence: `publishDiagnostics` is stateful per URI, so the squiggles that are no
        // longer placeable have to be cleared with an explicit empty array. Sending nothing
        // would leave them on screen for as long as the editor is open.
        assert_eq!(
            harness.latest(&uri),
            Some(Vec::new()),
            "the stale diagnostics have to be cleared, not merely stopped"
        );
    }

    #[test]
    fn closing_an_unsaved_buffer_clears_the_squiggles_it_had() {
        // A different loss from `..._gone_from_disk`: there the document stays in the graph and
        // only its text goes, so the diagnostics survive with nowhere to be placed. Here the
        // document is deleted outright — `didClose` on a buffer with no file behind it — and
        // its diagnostics go with it.
        //
        // Which makes the *clearing* the whole assertion: `publishDiagnostics` is stateful per
        // URI, so a document that no longer exists still needs an explicit empty array sent
        // for it. Publishing nothing would leave the squiggles on screen with no buffer under
        // them and no way to ever remove them.
        let mut harness = Harness::new();
        let uri = DocUri::from_path(&harness.root.path().join("untitled.rb")).expect("a uri");

        harness.open(
            &uri,
            "class Broken
  def oops(
end
",
        );
        assert!(
            harness.latest(&uri).is_some_and(|items| !items.is_empty()),
            "the syntax error is reported while the buffer is open"
        );

        harness.run(Task::DidClose { uri: uri.clone() });
        harness.analysis.publish_diagnostics();

        assert_eq!(
            harness.latest(&uri),
            Some(Vec::new()),
            "cleared, and nothing published for a document that is not there"
        );
    }

    #[cfg(unix)]
    #[test]
    fn closing_a_buffer_whose_file_cannot_be_read_forgets_it_rather_than_keeping_stale_text() {
        // On close the buffer stops shadowing disk and the file is re-read, so the graph holds
        // what is really there. A file that exists and cannot be read is the one case where
        // neither answer is available: keeping the buffer's text would leave the graph
        // asserting the contents of a file nobody can open.
        use std::os::unix::fs::PermissionsExt;

        let mut harness = Harness::new();
        let source = "class Person\n  def shout\n  end\nend\n";
        let uri = harness.write("app/person.rb", source);
        harness.index();
        harness.open(&uri, source);
        assert!(
            !harness.symbol_names("shout").is_empty(),
            "indexed to start"
        );

        let path = uri.to_path().expect("a path");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        harness.run(Task::DidClose { uri: uri.clone() });
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            harness.symbol_names("shout").is_empty(),
            "the document is dropped rather than left holding the closed buffer's text"
        );
    }

    #[test]
    fn a_gems_own_problems_are_never_published() {
        // Measured on a real Rails app: without this filter, opening it publishes 208
        // diagnostics inside other people's gems. Nobody can act on any of them, and they bury
        // whatever the user actually broke.
        let (dir, _gem_home, env) = project_with_gem("class Broken\n  def oops(\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let mine = harness.write("app/main.rb", "class Mine\n  def oops(\nend\n");
        harness.index();
        harness.index_gems();

        let published: Vec<String> = harness
            .published()
            .into_iter()
            .map(|(uri, _)| uri)
            .collect();
        assert!(
            published.iter().any(|uri| uri == mine.as_str()),
            "the user's own syntax error still has to be reported: {published:?}"
        );
        assert!(
            !published.iter().any(|uri| uri.contains("shouty-1.2.3")),
            "a gem's problems must not reach the editor: {published:?}"
        );
    }

    #[test]
    fn a_vendored_bundles_problems_are_not_published_either() {
        // Regression: a vendored bundle sits at `vendor/bundle/ruby/<abi>` — inside the
        // workspace root by construction — so a workspace-prefix test alone lets every gem in
        // it publish diagnostics. Found by a fixture, not by reasoning.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("Gemfile.lock"),
            "GEM\n  specs:\n    shouty (1.2.3)\n",
        )
        .unwrap();

        let gem_home = root.join("vendor/bundle/ruby/3.4.0");
        std::fs::create_dir_all(gem_home.join("gems/shouty-1.2.3/lib")).unwrap();
        std::fs::write(
            gem_home.join("gems/shouty-1.2.3/lib/shouty.rb"),
            "class Broken\n  def oops(\nend\n",
        )
        .unwrap();

        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, gems::Env::default());
        harness.write("app/main.rb", "class Mine; end\n");
        harness.index();
        harness.index_gems();

        let published: Vec<String> = harness
            .published()
            .into_iter()
            .map(|(uri, _)| uri)
            .collect();
        assert!(
            !published.iter().any(|uri| uri.contains("shouty")),
            "{published:?}"
        );
    }
    #[test]
    fn reloading_the_config_does_not_lose_the_gems() {
        // The graph is thrown away and rebuilt on reload. If the gems are not re-queued they are
        // gone for the rest of the session, and the only symptom is navigation quietly getting
        // worse after someone edits ya-lsp.toml.
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty::Megaphone.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();
        assert!(!harness.definition_at(&uri, source, "Megaphone").is_null());

        harness.run(Task::ReloadConfig);
        // The reload queues the gems again but indexes none of them; that is background work.
        while harness.analysis.step_gem_indexing() {}
        harness.analysis.settle();

        assert!(
            !harness.definition_at(&uri, source, "Megaphone").is_null(),
            "gem intelligence must survive a config reload"
        );
    }

    // -----------------------------------------------------------------------
    // Whose code is it
    // -----------------------------------------------------------------------

    #[test]
    fn an_engines_models_are_indexed_though_they_are_not_on_its_load_path() {
        // The engine walk, and the measurement it came from written as a test: against a
        // real bundle `ActiveStorage::Service` answered and `ActiveStorage::Blob` did not,
        // because the first is in `lib/` and the second is in `app/models/` and an engine
        // declares `require_paths = ["lib"]`. Both halves are asserted, because a fix that
        // reached `app/` by making it a load path would pass the first assertion and break
        // `require`.
        let (dir, root, env) = project_with_engine(&[(
            "models/shouty/message.rb",
            "class Shouty::Message\n  def shout\n  end\nend\n",
        )]);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "Shouty::Message.new.shout\nShouty::Horn.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        assert!(
            harness.has("Shouty::Message#shout()"),
            "the engine's app/ was not indexed"
        );
        assert!(harness.has("Shouty::Horn"), "and lib/ still is");

        let found = harness.definition_at(&uri, source, "Message");
        let target = found.to_string();
        assert!(
            target.contains("app/models/shouty/message.rb"),
            "the jump lands in the engine's own app/: {target}"
        );

        // The engine is indexed and is still not the user's code. Three features turn on that
        // test for a reason that has not changed — nobody fixes a warning inside somebody's
        // engine — so gate 2 asked a differently-named question instead of widening it.
        let engine =
            DocUri::from_path(&root.join("gems/shouty-1.2.3/app/models/shouty/message.rb"))
                .expect("an absolute path");
        assert!(!harness.analysis.is_own_code(engine.as_str()));
        assert!(harness.analysis.is_generator_source(engine.as_str()));
        assert!(harness.analysis.is_own_code(uri.as_str()));
        assert!(
            !harness
                .symbol_names("Message")
                .contains(&"Message".to_owned()),
            "workspace/symbol still answers with the user's own code only"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_curated_collection_is_indexed_and_is_not_the_users_own_code() {
        // `.gem_rbs_collection/` is hidden, so the workspace walk prunes it at the
        // directory — it arrives as a signature path of its own instead, on the same background
        // pass as a gem's `sig/`.
        //
        // The second assertion is the one that would have bitten. The collection lives *inside*
        // the workspace root, which is exactly the shape that made a vendored bundle publish 208
        // unfixable squiggles: `is_own_code` has to exclude it explicitly, because "under the
        // root" is not the question.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "GEM\n  remote: https://rubygems.org/\n  specs:\n\nBUNDLED WITH\n   2.6.2\n",
        )
        .unwrap();
        let collection = dir.path().join(".gem_rbs_collection/blanket/1.0");
        std::fs::create_dir_all(&collection).unwrap();
        let signature = collection.join("blanket.rbs");
        std::fs::write(&signature, "class Blanket\n  def tuck: () -> String\nend\n").unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write("app/main.rb", "Blanket.new\n");
        harness.index();
        harness.index_gems();

        assert!(
            harness.has("Blanket#tuck()"),
            "the curated collection was not indexed"
        );
        let curated = DocUri::from_path(&signature).expect("an absolute path");
        assert!(
            !harness.analysis.is_own_code(curated.as_str()),
            "the collection is under the workspace root and is still not the user's code"
        );
        // And the filter is a filter rather than a mute button: the file beside it is.
        assert!(harness.analysis.is_own_code(uri.as_str()));

        let found = harness.declarations_at(&uri, "Blanket.new.~\n");
        assert_eq!(found, vec!["tuck".to_owned()], "{found:?}");
    }
    /// A reload that changes `[index] load_paths` changes what counts as the user's own code.
    ///
    /// `own_prefixes` is the one prefix list that is not derived from the bundle, so it has no
    /// other reason to be rebuilt — and it was first written where `workspace_prefix` is, which is
    /// read once at construction. `rebuild` re-runs the walk and clears every neighbouring list;
    /// a stale one here would keep answering about the *previous* configuration's directories for
    /// the rest of the session, which is the shape every bug in this family has.
    #[test]
    fn a_reload_that_changes_the_load_paths_changes_what_is_owned() {
        let shared = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(shared.path().join("models")).unwrap();
        std::fs::write(shared.path().join("models/user.rb"), "class User; end\n").unwrap();
        let real = shared.path().canonicalize().unwrap();
        let user = DocUri::from_path(&real.join("models/user.rb")).unwrap();

        let base = "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n";
        let mut harness = Harness::configured(base);
        harness.index();
        assert!(
            !harness.analysis.is_own_code(user.as_str()),
            "nothing names this tree yet"
        );

        // The project names it, and reloads.
        std::fs::write(
            harness.root.path().join("ya-lsp.toml"),
            format!(
                "{base}\n[index]\nload_paths = [{}]\n",
                serde_json::to_string(&shared.path().to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
        harness.run(Task::ReloadConfig);
        harness.analysis.settle();
        assert!(
            harness.analysis.is_own_code(user.as_str()),
            "a reload has to re-read the list, or it describes the configuration before it"
        );
    }

    /// A tree outside the workspace root, named by `[index] load_paths`, is indexed, is the
    /// user's own code, and is a root the client is asked to claim.
    ///
    /// The monorepo case, and every one of the three had its own way of failing quietly. The
    /// setting documented itself as "extra roots to index" and indexed nothing — it reached
    /// `require` resolution and stopped — so a shared model was a name the graph did not hold.
    /// Once indexed it would have been *foreign*, because `is_own_code` is a prefix test against
    /// the root: no diagnostics, no rename, ranked below the bundle in search, in a directory
    /// the project wrote down by hand. And a file outside every workspace folder is one no
    /// client's selector claims, so nothing would ever have asked about it.
    #[test]
    fn a_load_path_outside_the_root_is_indexed_owned_and_claimed() {
        let shared = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(shared.path().join("models")).unwrap();
        std::fs::write(
            shared.path().join("models/user.rb"),
            "class User\n  def display_name = 'x'\nend\n",
        )
        .unwrap();

        let mut harness = Harness::configured(&format!(
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n[index]\nload_paths = [{}]\n",
            serde_json::to_string(&shared.path().to_string_lossy()).unwrap()
        ));
        let source = "class Story\n  def who = User.new\nend\n";
        let story = harness.write("app/models/story.rb", source);
        harness.index();

        // Indexed: the declaration is in the graph, reachable from a file in the root.
        let answer = harness.definition_at(&story, source, "User");
        let landed = serde_json::to_string(&answer).unwrap();
        assert!(
            landed.contains("models/user.rb"),
            "a load path outside the root has to be walked, or `require` resolves to nothing: {landed}"
        );

        // The user's own code: the prefix test has a second way to say yes now.
        //
        // Canonicalized, because that is the spelling the index holds: `resolve_load_path`
        // resolves the path once and everything downstream — the walk, `own_prefixes`, the
        // registration — is a function of that one answer. A temp directory on macOS reaches
        // disk through `/var -> /private/var`, so the two spellings genuinely differ here and
        // asserting the wrong one is how this test first failed.
        let real = shared.path().canonicalize().unwrap();
        let user_uri = DocUri::from_path(&real.join("models/user.rb")).unwrap();
        assert!(
            harness.analysis.is_own_code(user_uri.as_str()),
            "a tree the project named by hand is not somebody else's gem"
        );
        // The guard, in the direction that widening a prefix test always threatens: a real gem
        // root must still be foreign, or diagnostics start appearing inside the bundle.
        assert!(
            !harness
                .analysis
                .is_own_code("file:///gems/activerecord-8.1.3.1/lib/active_record.rb"),
            "widening `is_own_code` must not have swallowed the gems it exists to exclude"
        );
    }
}
