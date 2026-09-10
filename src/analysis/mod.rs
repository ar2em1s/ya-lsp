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

pub mod annotations;
pub mod code_actions;
pub mod completion;
pub mod cursor;
pub mod diagnostics;
pub mod erb;
pub mod hierarchy;
pub mod highlight;
pub mod hover;
pub mod indexer;
pub mod locator;
pub mod position;
pub mod progress;
pub mod ranges;
pub mod references;
pub mod rename;
pub mod render;
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
use lsp_server::{ErrorCode, Message, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CodeAction, CodeActionKind, CodeActionOrCommand, CompletionItem,
    CompletionItemKind, CompletionItemTag, CompletionList, CompletionResponse, CompletionTextEdit,
    DiagnosticSeverity, DocumentHighlight, DocumentSymbolResponse, Documentation, FoldingRange,
    GotoDefinitionResponse, Hover, HoverContents, Location, LocationLink, MarkupContent,
    MarkupKind, OneOf, OptionalVersionedTextDocumentIdentifier, PrepareRenameResponse,
    SelectionRange, SemanticToken, SemanticTokens, SignatureHelp, SymbolInformation,
    TextDocumentEdit, TextEdit, TypeHierarchyItem, WorkspaceEdit, WorkspaceSymbolResponse,
};
use rubydex::{
    indexing::LanguageId,
    model::{
        graph::Graph,
        ids::{DeclarationId, UriId},
    },
    resolution::Resolver,
};

use crate::messages;
use crate::workspace::{DocUri, Workspace, gems, rails};
use locator::Site;
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

/// One content change from `textDocument/didChange`.
#[derive(Debug)]
pub struct TextChange {
    /// `None` when the client sent the whole buffer instead of a range.
    pub range: Option<lsp_types::Range>,
    pub text: String,
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
    outgoing: Sender<Message>,
    cancellations: Cancellations,
) -> AnalysisHandle {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let thread = std::thread::Builder::new()
        .name("ya-lsp-analysis".to_owned())
        .spawn(move || {
            let mut analysis = Analysis::new(workspace, encoding, client, outgoing, cancellations);
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
    generated_from: Option<synthesize::Context>,
    /// Eight bytes per document walked, of **what that document contributed** to
    /// `generated_from`.
    ///
    /// Comparing the whole `Context` can only be asked after the walk, because the walk is the
    /// only thing that answers it. This is the same question asked of one document: a
    /// keystroke re-derives the contribution of the file it touched and compares eight bytes,
    /// and the other twenty-four thousand are known not to have moved because nothing else was
    /// indexed. A document the walk does not visit has **no entry**, which is not the same as
    /// an entry for an empty contribution — see `Analysis::contribution`.
    contributions: HashMap<rubydex::model::ids::UriId, u64>,
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
    /// Every file the generators read, as their readers last parsed it.
    ///
    /// Keyed by document URI, and each entry carries the evidence that the file has not changed
    /// since: the disk stamp the pass gate already takes, or a hash of the buffer where an
    /// editor holds one. See `synthesize::Cached`, which is also where the argument for
    /// memoising the **parse** rather than the `Facts` is written down.
    sources: HashMap<String, synthesize::Cached>,
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
    /// How many source files the pass has read off the disk or out of a buffer and parsed.
    ///
    /// The third of these counters for the same reason as the other two: the memo behind it
    /// changes no answer, so the only way to see it working is to count the reads it did not
    /// make. It counts **files and not parses** — one file on three lists is one read and up to
    /// three readers, which is itself part of the saving.
    reads: u64,
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
    /// The last non-empty diagnostic set we sent per URI.
    ///
    /// `publishDiagnostics` is stateful: whatever was last sent for a URI stays on screen until
    /// something else is sent for it. Keeping the last publish lets us send only what changed —
    /// and, just as importantly, an explicit empty set for a URI whose problems went away.
    published: HashMap<DocUri, Vec<lsp_types::Diagnostic>>,
}

impl Analysis {
    fn new(
        workspace: Workspace,
        encoding: PositionEncoding,
        client: ClientSupport,
        outgoing: Sender<Message>,
        cancellations: Cancellations,
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
            reads: 0,
            contributions: HashMap::new(),
            indexed_text: HashMap::new(),
            sources: HashMap::new(),
            touched: HashSet::new(),
            passes: 0,
            stamps: Vec::new(),
            touched_all: true,
            open: HashMap::new(),
            encoding,
            client,
            workspace,
            outgoing,
            cancellations,
            dirty: false,
            foreign_prefixes: Vec::new(),
            engine_prefixes: Vec::new(),
            resolve_at: None,
            pending_index: Vec::new(),
            gem_work: None,
            workspace_files: 0,
            skipped: HashSet::new(),
            recovering: false,
            index_full_reported: false,
            workspace_prefix,
            published: HashMap::new(),
        }
    }

    /// Index everything in the workspace, once, at startup.
    fn index_workspace(&mut self) {
        let started = Instant::now();
        self.warn_about_unknown_rules();
        let discovery = self.workspace.discover();

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

        self.synthesize();
        self.resolve();
        tracing::info!(
            "indexed {count} files in {indexed:.2?}, resolved in {:.2?} (total {:.2?})",
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
            Task::WatchedFiles { uris } => self.refresh(uris),
            Task::ChangeConfig { options } => {
                self.workspace.set_options(options);
                self.handle(Task::ReloadConfig);
            }
            Task::ReloadConfig => {
                for problem in self.workspace.reload() {
                    tracing::warn!("{problem}");
                    self.show_warning(&problem);
                }
                tracing::info!("reloaded configuration; re-indexing workspace");
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
        let (problems, load_paths, signature_paths, engine_paths, gem_count, roots, signatures) = {
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
        // The per-document half of the same sentence, keyed by a `UriId` of a graph that is
        // about to be replaced — every document in it is about to be re-indexed anyway.
        self.contributions.clear();
        // Same again: every entry describes offsets into the graph being thrown away.
        self.indexed_text.clear();
        // The parse memo. This one is keyed by a URI rather than by anything the graph owns and its
        // freshness is the file's own, so it would survive a rebuild correctly — and a rebuild
        // is a configuration change, which can move the workspace root and so what every
        // `name` in it says. Dropped rather than reasoned about.
        self.sources.clear();
        // Whatever was still queued refers to the old configuration's gem roots, and the graph
        // it was going to be indexed into no longer exists.
        if let Some(work) = self.gem_work.take()
            && let Some(progress) = work.progress
        {
            progress.end("cancelled".to_owned());
        }
        self.foreign_prefixes.clear();
        self.engine_prefixes.clear();
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
            if uri.to_path().is_some_and(|path| rails::is_structure(&path)) {
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
        self.synthesize();
        self.resolve();
        self.publish_diagnostics();
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
        uri.starts_with(&self.workspace_prefix)
            && !self
                .foreign_prefixes
                .iter()
                .any(|prefix| uri.starts_with(prefix))
    }

    /// Whose routes file this is, for [`rails::read_routes`].
    ///
    /// One line, and it is here rather than in the generator for the reason every other question
    /// about *which* documents exist is: `workspace::rails` is pure text in, text out, and never
    /// sees a URI.
    fn whose(&self, uri: &DocUri) -> rails::Whose {
        if self.is_own_code(uri.as_str()) {
            rails::Whose::Own
        } else {
            rails::Whose::Gem
        }
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

    /// Another document's text, by the URI rubydex filed it under.
    ///
    /// The one thing [`types`] reads that is not the graph, and it goes through
    /// [`Self::with_text`] rather than straight to disk for the reason that function exists: an
    /// open buffer is authoritative, so a controller being edited types the template it renders
    /// before it is saved. It clones, unlike every other accessor here — the text has to outlive
    /// the borrow because what reads it is a parse in another module — and it is reached only
    /// where a receiver in a *template* was nothing but a name, which is a path no ordinary Ruby
    /// file takes.
    fn text_of(&self, uri: &str) -> Option<String> {
        let uri = DocUri::from_uri_str(uri)?;
        self.with_text(&uri, |text| text.text().to_owned())
    }

    /// What the type side reads besides the graph, built per request.
    ///
    /// Per request rather than held, because the closure borrows `self` and the flag is
    /// configuration that a `workspace/didChangeConfiguration` can move underneath it.
    fn sources<'a>(&'a self, read: &'a dyn Fn(&str) -> Option<String>) -> types::Sources<'a> {
        types::Sources {
            graph: &self.graph,
            types: &self.types,
            read,
            views: &self.views,
            guess: self.workspace.config().types.guess_from_names,
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

    fn serve(&mut self, request: Request) {
        if self.cancellations.take(&request.id) {
            tracing::debug!("skipping cancelled request {}", request.id);
            self.respond(Response::new_err(
                request.id,
                ErrorCode::RequestCanceled as i32,
                "request cancelled by the client".to_owned(),
            ));
            return;
        }

        let id = request.id.clone();
        let method = request.method.clone();
        // The bulkhead's third seam, on the *request* path: eight lines of ordinary Ruby — a
        // class reopened under a constant that aliases it — reach an unwrap in
        // `find_self_receiver_declaration` from `textDocument/definition`, and neither the
        // indexing bulkhead nor `resolve`'s guard covers that path.
        //
        // The failure unit here is the request: it answers nothing and says so. `settle` is
        // inside the guard rather than in front of it because everything it does is reachable
        // the same way — a request that arrives dirty pays for the pass and the link, and only
        // one of the three has a guard of its own.
        //
        // A settle that crashes has already cleared `dirty`, so the next request answers against
        // the graph as it stands rather than settling again. That is deliberate and it is the
        // policy `recovering` lands on one level down: retrying per request would settle, crash
        // and re-arm on every keystroke. Something that changes the workspace arms it again.
        let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Answer against a settled graph: a stale answer is worse than a slightly slower
            // one. This is `dirty`, not `resolve_at`, because background gem indexing
            // deliberately does not arm the timer — but its files are still unlinked until
            // something resolves them.
            //
            // Two requests are exempt, because their answer does not come from the graph at
            // all; waiting for a resolve they never read is a cost paid on every keystroke, and
            // on a large file it is two orders of magnitude.
            //
            // The third is **deferred rather than exempt**, and the retry below is the whole
            // difference between the two words.
            let deferred = self.dirty && defers(&method);
            if self.dirty && needs_the_graph(&method) && !defers(&method) {
                self.settle();
            }
            let again = deferred.then(|| request.params.clone());
            let response = self.dispatch(&id, request);
            // **A `Rebase` is an optimization and not a filter.** It refuses an offset that
            // lands inside text the graph has not been given — a constant just typed is very
            // nearly the whole of that set, because `self`, a local and an instance variable
            // are all resolved against the buffer — and from here a refusal cannot be told
            // apart from a cursor with nothing to complete. Both are repaired the same way:
            // settle, and ask again with the two coordinate systems back in step.
            //
            // So the deferred path answers *sooner* than the eager one and never answers
            // *less*, which is the property that makes it safe to leave on. Without this it
            // trades a 45 ms answer for a 1 ms empty list, which is not a trade anybody wants.
            match again {
                Some(params) if answered_nothing(&response) => {
                    self.settle();
                    self.dispatch(
                        &id,
                        Request {
                            id: id.clone(),
                            method: method.clone(),
                            params,
                        },
                    )
                }
                _ => response,
            }
        }));

        let response = match served {
            Ok(response) => response,
            Err(_) => {
                // Not a `messages::` sentence: this is addressed to the client and answers one
                // request, which is the boundary `messages.md` draws. The panic hook has
                // already put rubydex's file and line on stderr.
                tracing::error!("answering {method} crashed; that request answers nothing");
                Response::new_err(
                    id.clone(),
                    ErrorCode::InternalError as i32,
                    format!("ya-lsp crashed while answering {method}"),
                )
            }
        };

        self.cancellations.forget(&id);
        self.respond(response);
    }

    /// Which handler answers `request`. Split out of [`Analysis::serve`] only so that the
    /// bulkhead there is one expression rather than a closure wrapped around a `match` this
    /// long.
    fn dispatch(&mut self, id: &RequestId, request: Request) -> Response {
        #[cfg(test)]
        crash_the_next_request_if_asked();
        match request.method.as_str() {
            "textDocument/documentSymbol" => reply(id, self.document_symbols(request.params)),
            "textDocument/hover" => reply(id, self.hover(request.params)),
            "textDocument/definition" => reply(id, self.goto_definition(request.params)),
            "textDocument/references" => reply(id, self.references(request.params)),
            "textDocument/documentHighlight" => reply(id, self.document_highlights(request.params)),
            "textDocument/selectionRange" => reply(id, self.selection_ranges(request.params)),
            "textDocument/foldingRange" => reply(id, self.folding_ranges(request.params)),
            "textDocument/semanticTokens/full" => reply(id, self.semantic_tokens(request.params)),
            "workspace/symbol" => reply(id, self.workspace_symbols(request.params)),
            "textDocument/prepareTypeHierarchy" => {
                reply(id, self.prepare_type_hierarchy(request.params))
            }
            "typeHierarchy/supertypes" => reply(id, self.supertypes(request.params)),
            "typeHierarchy/subtypes" => reply(id, self.subtypes(request.params)),
            "textDocument/signatureHelp" => reply(id, self.signature_help(request.params)),
            "textDocument/codeAction" => reply(id, self.code_actions(request.params)),
            "textDocument/prepareRename" => reply(id, self.prepare_rename(request.params)),
            "textDocument/rename" => reply(id, self.rename(request.params)),
            "textDocument/completion" => reply(id, self.completion(request.params)),
            "completionItem/resolve" => reply(id, self.resolve_completion(request.params)),
            method => Response::new_err(
                id.clone(),
                ErrorCode::MethodNotFound as i32,
                format!("ya-lsp does not handle {method} yet"),
            ),
        }
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    /// `textDocument/documentSymbol`.
    fn document_symbols(&self, params: serde_json::Value) -> Option<DocumentSymbolResponse> {
        let params: lsp_types::DocumentSymbolParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        let symbols = self.with_text(&uri, |text| {
            symbols::document_symbols(&self.graph, UriId::from(uri.as_str()), text)
        })?;

        if symbols.is_empty() {
            return None;
        }
        Some(if self.client.hierarchical_symbols {
            DocumentSymbolResponse::Nested(symbols)
        } else {
            DocumentSymbolResponse::Flat(symbols::flatten(&symbols, &params.text_document.uri))
        })
    }

    /// `textDocument/hover`.
    fn hover(&self, params: serde_json::Value) -> Option<Hover> {
        let params: lsp_types::HoverParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.text_of(uri);
        let sources = self.sources(&read);
        self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // The graph may be a keystroke behind the buffer, so the cursor goes *in*
            // through the map and every span comes back *out* through it.
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;
            // More than one target can share the narrowest span; take the first that has
            // something to say rather than the first that exists.
            let uri_id = UriId::from(uri.as_str());
            locator::locate(&self.graph, uri_id, at)
                .into_iter()
                .find_map(|located| {
                    let (start, end) = (
                        rebase.to_buffer(located.start)?,
                        rebase.to_buffer(located.end)?,
                    );
                    let resolution =
                        locator::resolve_typed(&sources, uri_id, text.text(), &located, start);
                    // The card names a line and this is where the text is; see
                    // `hover::markdown`. The offset is the buffer's already — it is provenance
                    // `cursor` read out of the buffer, not a graph key — so it is not mapped.
                    let line = resolution
                        .derivation
                        .assignment
                        .map(|at| text.position_at(at).line + 1);
                    let markdown =
                        hover::markdown(&self.graph, &self.synthesized, &resolution, line)?;
                    Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: markdown,
                        }),
                        range: Some(text.range_at(start, end)),
                    })
                })
        })?
    }

    /// `textDocument/signatureHelp`.
    ///
    /// Answered from the buffer rather than from the graph's copy of it, like completion and
    /// for the same reason: the call under the cursor is half-written by definition, and the
    /// argument the user is on is a fact about the text as it stands this keystroke.
    fn signature_help(&self, params: serde_json::Value) -> Option<SignatureHelp> {
        let params: lsp_types::SignatureHelpParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        self.with_text(&uri, |text| {
            let call = cursor::call_at(text.text(), text.offset_at(position))?;
            let method = locator::precise_call(&self.graph, UriId::from(uri.as_str()), call.name)?;
            signature_help::help(&self.graph, method, &call.active)
        })?
    }

    /// `textDocument/documentHighlight`.
    ///
    /// Answered from the buffer rather than from the graph's copy of it, like completion and
    /// signature help: the half of the answer that comes from `scopes` is a fact about the text
    /// as it stands this keystroke, and a highlight drawn over stale offsets lands on the wrong
    /// words rather than on none.
    fn document_highlights(&self, params: serde_json::Value) -> Option<Vec<DocumentHighlight>> {
        let params: lsp_types::DocumentHighlightParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let found = self.with_text(&uri, |text| {
            let found = highlight::find(
                &self.graph,
                &self.synthesized,
                UriId::from(uri.as_str()),
                text.text(),
                text.offset_at(position),
            );
            found
                .into_iter()
                .map(|at| DocumentHighlight {
                    range: text.range_at(at.start, at.end),
                    kind: Some(at.kind),
                })
                .collect::<Vec<_>>()
        })?;

        // `null` rather than `[]`, for the same reason completion answers one inside a comment:
        // it is what tells the client nothing was known here, so it may fall back to matching
        // words itself.
        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/selectionRange`.
    ///
    /// One chain per position asked about, in the order asked: the protocol pairs the two arrays
    /// by index and has no spelling for "not this one", so every position answers — with the
    /// buffer itself where there was nothing else to say.
    fn selection_ranges(&self, params: serde_json::Value) -> Option<Vec<SelectionRange>> {
        let params: lsp_types::SelectionRangeParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        let found = self.with_text(&uri, |text| {
            params
                .positions
                .iter()
                .map(|&position| ranges::selection_range(text, text.offset_at(position)))
                .collect::<Vec<_>>()
        })?;

        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/foldingRange`.
    ///
    /// `null` rather than `[]`, and here it matters more than anywhere else: a client that has a
    /// folding provider stops guessing from indentation, so an empty array would take away the
    /// fallback *and* put nothing in its place. A `null` can only give it back.
    fn folding_ranges(&self, params: serde_json::Value) -> Option<Vec<FoldingRange>> {
        let params: lsp_types::FoldingRangeParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        // Declined in a template, and the `null` above is exactly why it can be. The walk sees
        // the Ruby and nothing else: over a template with a five-line `<div>` in it, it offers
        // two folds for the `<% %>` blocks and none for the markup. Handing the editor's
        // indentation guess back gets the whole file folded, including those two.
        if erb::is_template_uri(&uri) {
            return None;
        }
        let found = self.with_text(&uri, ranges::folds)?;

        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/semanticTokens/full`.
    ///
    /// The whole document, every time, and no delta — see [`tokens`] for why. The relative
    /// encoding is the protocol's, not a choice: each token is a delta from the one before it,
    /// which is why [`tokens::of`] sorts.
    fn semantic_tokens(&self, params: serde_json::Value) -> Option<SemanticTokens> {
        let params: lsp_types::SemanticTokensParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        self.with_text(&uri, |text| {
            let mut data: Vec<SemanticToken> = Vec::new();
            let (mut line, mut start) = (0, 0);
            for token in tokens::of(text.text()) {
                let at = text.position_at(token.start);
                // A token never spans a line — every one of them is an identifier — so the
                // length is the difference between two characters on the same line, in whatever
                // unit the client negotiated. Taking `end - start` in bytes would be wrong for
                // every non-ASCII name, and `имя` is a legal local.
                let length = text
                    .position_at(token.end)
                    .character
                    .saturating_sub(at.character);
                // Saturating, all three of them. The list is sorted, so none of these can go
                // backwards — and a subtraction that could underflow sits on the analysis
                // thread, where a panic is not a wrong colour but a server that stops
                // answering anything at all.
                data.push(SemanticToken {
                    delta_line: at.line.saturating_sub(line),
                    delta_start: if at.line == line {
                        at.character.saturating_sub(start)
                    } else {
                        at.character
                    },
                    length,
                    token_type: token.kind as u32,
                    token_modifiers_bitset: 0,
                });
                line = at.line;
                start = at.character;
            }
            SemanticTokens {
                result_id: None,
                data,
            }
        })
    }

    /// `textDocument/definition`.
    fn goto_definition(&self, params: serde_json::Value) -> Option<GotoDefinitionResponse> {
        let params: lsp_types::GotoDefinitionParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.text_of(uri);
        let sources = self.sources(&read);
        let (origin, sites) = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;

            let uri_id = UriId::from(uri.as_str());
            for located in locator::locate(&self.graph, uri_id, at) {
                let (start, end) = (
                    rebase.to_buffer(located.start)?,
                    rebase.to_buffer(located.end)?,
                );
                // The same rung hover reads. A jump and a card that disagreed about what
                // `person.` is would be worse than either being absent.
                let sites: Vec<Site> =
                    locator::resolve_typed(&sources, uri_id, text.text(), &located, start)
                        .declarations
                        .into_iter()
                        .flat_map(|id| locator::sites(&self.graph, &self.synthesized, id))
                        .collect();
                if !sites.is_empty() {
                    return Some((text.range_at(start, end), sites));
                }
            }

            // Nothing in the graph covers the cursor. The one thing the graph never indexes is
            // the *argument* of a `require`, which is exactly the part people click on.
            let require = requires::at(text.text(), offset)?;
            let site = self.require_site(&uri, &require)?;
            Some((text.range_at(require.start, require.end), vec![site]))
        })??;

        let links: Vec<LocationLink> = sites
            .into_iter()
            .filter_map(|site| self.link(origin, &site))
            .collect();
        if links.is_empty() {
            return None;
        }

        Some(if self.client.definition_links {
            GotoDefinitionResponse::Link(links)
        } else {
            GotoDefinitionResponse::Array(
                links
                    .into_iter()
                    .map(|link| Location {
                        uri: link.target_uri,
                        // The name, not the whole body: it is where the editor parks the
                        // cursor, and landing on `class` is landing in the right place.
                        range: link.target_selection_range,
                    })
                    .collect(),
            )
        })
    }

    // -----------------------------------------------------------------------
    // Project-wide search
    // -----------------------------------------------------------------------

    /// `textDocument/references`.
    ///
    /// Answers only with the user's own code — see `analysis::references` for why, and for the
    /// difference in precision between a constant and a method.
    fn references(&self, params: serde_json::Value) -> Option<Vec<Location>> {
        let params: lsp_types::ReferenceParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::from_lsp(&params.text_document_position.text_document.uri)?;
        let include_declaration = params.context.include_declaration;
        let scope = self.own_documents();

        let mut found = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // As in goto-definition: several targets can share the narrowest span, so take the
            // first that has something to say rather than the first that exists.
            locator::locate(&self.graph, UriId::from(uri.as_str()), offset)
                .into_iter()
                .find_map(|located| {
                    let resolution = locator::resolve(&self.graph, &located);
                    let found = references::find(
                        &self.graph,
                        &self.synthesized,
                        &located,
                        &resolution,
                        &scope,
                        include_declaration,
                    );
                    (!found.is_empty()).then_some(found)
                })
        })??;

        if found.len() > MAX_REFERENCES {
            // Said out loud, not just logged. A truncated "find all references" is a wrong
            // answer wearing the shape of a right one, and the user is the only one who can
            // decide what to do about it. It takes a workspace of tens of thousands of files to
            // reach — measured: `.new` across 17,557 files finds 35,733 — so this is rare
            // enough that a message is information rather than noise.
            let message = messages::references_truncated(found.len(), MAX_REFERENCES);
            tracing::warn!("{message}");
            self.show_warning(&message);
            found.truncate(MAX_REFERENCES);
        }

        let mut ranges = Ranges::new(self);
        let locations: Vec<Location> = found
            .into_iter()
            .filter_map(|reference| {
                let uri = DocUri::from_uri_str(&reference.uri)?;
                Some(Location {
                    range: ranges.at(&uri, reference.start, reference.end)?,
                    uri: uri.to_lsp().ok()?,
                })
            })
            .collect();
        (!locations.is_empty()).then_some(locations)
    }

    /// `workspace/symbol`.
    fn workspace_symbols(&self, params: serde_json::Value) -> Option<WorkspaceSymbolResponse> {
        let params: lsp_types::WorkspaceSymbolParams = parse_params(params)?;
        let started = Instant::now();
        let hits = search::search(
            &self.graph,
            &self.synthesized,
            params.query.trim(),
            MAX_WORKSPACE_SYMBOLS,
            &self.own_documents(),
        );
        tracing::debug!(
            "workspace/symbol {:?} -> {} hits in {:.2?}",
            params.query,
            hits.len(),
            started.elapsed()
        );

        let mut ranges = Ranges::new(self);
        let symbols: Vec<SymbolInformation> = hits
            .into_iter()
            .filter_map(|hit| {
                let uri = DocUri::from_uri_str(&hit.site.uri)?;
                // The name span, not the whole construct: a client reveals `location.range`
                // selected, and selecting a 400-line class body to show where it starts is
                // not what anyone asked for.
                let range = ranges.at(&uri, hit.site.selection.0, hit.site.selection.1)?;
                #[allow(deprecated)] // Required field, superseded by `tags`.
                Some(SymbolInformation {
                    name: hit.name,
                    kind: hit.kind,
                    tags: hit.tags,
                    deprecated: None,
                    location: Location {
                        uri: uri.to_lsp().ok()?,
                        range,
                    },
                    container_name: hit.container,
                })
            })
            .collect();

        // `null` rather than `[]` for nothing found, the same as every other handler: an empty
        // array is a claim that the project has no such symbol, which is only true by accident.
        (!symbols.is_empty()).then_some(WorkspaceSymbolResponse::Flat(symbols))
    }

    // -----------------------------------------------------------------------
    // Type hierarchy
    // -----------------------------------------------------------------------

    /// `textDocument/prepareTypeHierarchy`.
    ///
    /// The item this hands back is what the two follow-ups arrive holding, so it carries the
    /// declaration in `data` — as a decimal string, for `completionItem/resolve`'s reason: a
    /// `DeclarationId` is a 64-bit hash and JSON numbers are doubles. Unlike a completion item
    /// this one survives a config reload, because the hash is of the *name*: the graph the client
    /// was looking at can be gone and the id still finds the class.
    fn prepare_type_hierarchy(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchyPrepareParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let items = self.with_text(&uri, |text| {
            hierarchy::prepare(
                &self.graph,
                &self.synthesized,
                UriId::from(uri.as_str()),
                text.offset_at(position),
                &self.own_documents(),
            )
        })?;
        self.hierarchy_items(items)
    }

    /// `typeHierarchy/supertypes`.
    fn supertypes(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchySupertypesParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let items = hierarchy::supertypes(
            &self.graph,
            &self.synthesized,
            declaration,
            &self.own_documents(),
        );
        self.hierarchy_items(items)
    }

    /// `typeHierarchy/subtypes`.
    fn subtypes(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchySubtypesParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let found = hierarchy::subtypes(
            &self.graph,
            &self.synthesized,
            declaration,
            MAX_SUBTYPES,
            &self.own_documents(),
        );

        if found.found > MAX_SUBTYPES {
            // Said out loud, as a truncated `references` is: a short list of subtypes is
            // indistinguishable from a complete one, and the user is the only one who can decide
            // what to do about it. It takes asking about something near the root of the object
            // model to reach, which is a deliberate click rather than something that happens
            // while typing, so a message here is information rather than noise.
            let message = messages::subtypes_truncated(found.found, MAX_SUBTYPES);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }
        self.hierarchy_items(found.items)
    }

    /// Turn hierarchy rows into the wire shape, reading each file at most once.
    ///
    /// A row whose file cannot be read is dropped rather than sent with a made-up range —
    /// rubydex's synthetic built-in document is the one that reaches here, and `DocUri` rejects
    /// it for every request alike. `null` for an empty result, never `[]`: an empty array is a
    /// claim that a class has no ancestors, which is not true of anything in Ruby.
    fn hierarchy_items(&self, items: Vec<hierarchy::Item>) -> Option<Vec<TypeHierarchyItem>> {
        let mut ranges = Ranges::new(self);
        let items: Vec<TypeHierarchyItem> = items
            .into_iter()
            .filter_map(|item| {
                let uri = DocUri::from_uri_str(&item.site.uri)?;
                Some(TypeHierarchyItem {
                    name: item.name,
                    kind: item.kind,
                    tags: None,
                    detail: Some(item.detail),
                    range: ranges.at(&uri, item.site.full.0, item.site.full.1)?,
                    selection_range: ranges.at(
                        &uri,
                        item.site.selection.0,
                        item.site.selection.1,
                    )?,
                    uri: uri.to_lsp().ok()?,
                    data: item
                        .declaration
                        .map(|id| serde_json::Value::String(id.get().to_string())),
                })
            })
            .collect();
        (!items.is_empty()).then_some(items)
    }

    // -----------------------------------------------------------------------
    // Rename
    // -----------------------------------------------------------------------

    /// `textDocument/prepareRename`.
    ///
    /// `textDocument/codeAction`.
    ///
    /// The second request that writes, and it goes through the same two gates the first one
    /// does: nothing is proposed for a file that is not the user's own, and nothing at all is
    /// proposed inside a gem. It is a *silent* refusal, unlike a rename's — the user pressed no
    /// key asking for this one, so an action absent from a menu is the whole of what needs
    /// saying.
    ///
    /// Declined in a template, and for a reason no other request has: every action here writes a
    /// **line**, and in a template a line belongs to the markup. `erb::ruby_view` keeps offsets
    /// so that everything which reads answers unchanged, and there is nothing it can do about a
    /// line that starts with `<td>`.
    fn code_actions(&self, params: serde_json::Value) -> Option<Vec<CodeActionOrCommand>> {
        let params: lsp_types::CodeActionParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        if !self.is_own_code(uri.as_str()) || erb::is_template_uri(&uri) {
            return None;
        }
        let actions = self.with_text(&uri, |text| {
            let start = text.offset_at(params.range.start);
            let end = text.offset_at(params.range.end);
            code_actions::at(text.text(), start, end)
                .into_iter()
                .map(|action| {
                    // Converted here, once, from the same read that produced the offsets — the
                    // reason `rename::Replacement` carries both, one layer down.
                    let edits = action
                        .edits
                        .iter()
                        .map(|edit| TextEdit {
                            range: text.range_at(edit.start, edit.end),
                            new_text: edit.text.clone(),
                        })
                        .collect();
                    CodeActionOrCommand::CodeAction(CodeAction {
                        title: action.title,
                        kind: Some(match action.kind {
                            code_actions::Kind::Extract => CodeActionKind::REFACTOR_EXTRACT,
                            code_actions::Kind::Rewrite => CodeActionKind::REFACTOR_REWRITE,
                        }),
                        edit: Some(self.workspace_edit(vec![(uri.clone(), edits)])),
                        ..CodeAction::default()
                    })
                })
                .collect::<Vec<_>>()
        })?;

        (!actions.is_empty()).then_some(actions)
    }

    /// Answering with a range is a *promise* that the rename will go through, so this runs the
    /// whole plan and reads every span back before it says yes. That costs a file read per file
    /// the name is written in, once, on a key the user pressed deliberately — and it is the
    /// only way the promise is honest. A prepare that said yes and a rename that then refused
    /// would put the refusal after the user had typed the new name.
    fn prepare_rename(&self, params: serde_json::Value) -> Option<PrepareRenameResponse> {
        let params: lsp_types::TextDocumentPositionParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        let offset = self.with_text(&uri, |text| text.offset_at(params.position))?;
        let renaming = self.renaming(&uri, offset)?;

        // The one replacement the cursor is actually in, out of everything the rename would
        // make: the editor puts its rename box over exactly this range and pre-fills it with
        // the text inside. It is not always the span the cursor was *located* in — the name of
        // `Failure = Class.new(StandardError)` is located as the whole assignment and narrowed
        // to the word — so a cursor on the `=` of that finds nothing and answers `null`.
        let here = renaming
            .files
            .iter()
            .filter(|(at, _)| *at == uri)
            .flat_map(|(_, replacements)| replacements)
            // A range rather than a pair of comparisons, which is the same test without the
            // short-circuit arm a `&&` would put in a file this one is measured with.
            .find(|at| (at.start..=at.end).contains(&offset))?;
        Some(PrepareRenameResponse::Range(here.range))
    }

    /// `textDocument/rename`.
    fn rename(&self, params: serde_json::Value) -> Option<WorkspaceEdit> {
        let params: lsp_types::RenameParams = parse_params(params)?;
        let position = params.text_document_position;
        let uri = DocUri::from_lsp(&position.text_document.uri)?;
        let offset = self.with_text(&uri, |text| text.offset_at(position.position))?;
        // The plan is made again rather than remembered from the prepare: `prepareSupport` is a
        // client capability, and a client without it sends this request on its own, so every
        // refusal has to be reachable from here too.
        let renaming = self.renaming(&uri, offset)?;

        if !rename::is_name(&params.new_name, renaming.constant) {
            let message = messages::rename_needs_a_ruby_name(&params.new_name, renaming.constant);
            tracing::info!("{message}");
            self.show_warning(&message);
            return None;
        }

        // Nothing left to decide: every range was converted when the plan was confirmed, from
        // the same read that checked the bytes under it, so there is no second conversion here
        // to disagree with the first one or to fail on its own.
        let files: Vec<(DocUri, Vec<TextEdit>)> = renaming
            .files
            .iter()
            .map(|(at, replacements)| {
                let edits = replacements
                    .iter()
                    .map(|at| TextEdit {
                        range: at.range,
                        new_text: params.new_name.clone(),
                    })
                    .collect();
                (at.clone(), edits)
            })
            .collect();
        // Counted into locals first: an argument on its own line inside a `tracing::debug!` is
        // evaluated only when that level is on, so it reads as a line no test ever ran.
        let edited: usize = files.iter().map(|(_, edits)| edits.len()).sum();
        let (touched, from, to) = (files.len(), &renaming.name, &params.new_name);
        tracing::debug!("rename {from:?} -> {to:?}: {edited} edits across {touched} files");
        Some(self.workspace_edit(files))
    }

    /// The rename at a position, with every span read back and checked against the name it is
    /// about to replace.
    ///
    /// Both requests go through here, and a refusal is said out loud rather than merely
    /// answered with `null`: the user pressed a key asking for this one, and an editor's own
    /// "this cannot be renamed" does not say which of the reasons applies or what to do next.
    /// That is the one place ya-lsp raises a `window/showMessage` for a single request rather
    /// than for the state of the workspace, and pressing the key is what earns it.
    fn renaming(&self, uri: &DocUri, offset: u32) -> Option<Renaming> {
        // ya-lsp never proposes an edit to a file that is not the user's own. Silently, as
        // every other request inside a bundle is: a gem is opened to be read, and nobody
        // pressing rename in one is expecting it to work.
        if !self.is_own_code(uri.as_str()) {
            return None;
        }
        let own = self.own_documents();
        let plan = self.with_text(&uri.clone(), |text| {
            rename::plan(
                &self.graph,
                &self.synthesized,
                uri.as_str(),
                text.text(),
                offset,
                &own,
            )
        })?;
        let (name, constant, edits) = match plan {
            rename::Plan::Nothing => return None,
            rename::Plan::Refused(message) => {
                tracing::info!("{message}");
                self.show_warning(&message);
                return None;
            }
            rename::Plan::Edits {
                name,
                constant,
                edits,
            } => (name, constant, edits),
        };

        // Every span, read back from the text as it stands and confirmed to hold only the name.
        // This is what stops `Error = Class.new(StandardError)` — whose name span rubydex
        // records as the entire assignment — from being replaced wholesale, and it is why a
        // refusal here is whole: a rename that changed most of the places a name is written
        // would leave code that no longer runs.
        let mut ranges = Ranges::new(self);
        let mut files: HashMap<DocUri, Vec<Replacement>> = HashMap::new();
        for edit in edits {
            let at = DocUri::from_uri_str(&edit.uri)?;
            let confirmed = ranges
                .text_at(&at, edit.start, edit.end)
                .and_then(|written| rename::narrow(&written, &name));
            let Some((from, to)) = confirmed else {
                let message = messages::rename_could_not_confirm(&name, &file_name(&at));
                tracing::warn!("{message}");
                self.show_warning(&message);
                return None;
            };
            let (start, end) = (edit.start + from, edit.start + to);
            files.entry(at.clone()).or_default().push(Replacement {
                start,
                end,
                // Converted here, from the read that just confirmed the bytes: the offsets and
                // the range are two views of one span, and deriving them apart is how they come
                // to disagree.
                range: ranges.at(&at, start, end)?,
            });
        }

        // Sorted by URI so that the same rename produces the same edit twice running; the spans
        // inside a file arrive in order already, from `references` and from the scope walk
        // alike.
        let mut files: Vec<(DocUri, Vec<Replacement>)> = files.into_iter().collect();
        files.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        Some(Renaming {
            name,
            constant,
            files,
        })
    }

    /// The edit in whichever of the two shapes the client said it takes.
    ///
    /// `documentChanges` is worth negotiating for rather than always sending the older `changes`
    /// map, because it carries the version each file's edit was computed against — so a client
    /// can reject a rename the user has typed past instead of applying it to text that has
    /// moved. The version is the one from `didOpen`/`didChange` where there is a buffer, and
    /// `null` for a file only on disk, which is what the protocol's *optional* version means.
    fn workspace_edit(&self, files: Vec<(DocUri, Vec<TextEdit>)>) -> WorkspaceEdit {
        if !self.client.versioned_edits {
            return WorkspaceEdit {
                changes: Some(
                    files
                        .into_iter()
                        .filter_map(|(uri, edits)| Some((uri.to_lsp().ok()?, edits)))
                        .collect(),
                ),
                ..WorkspaceEdit::default()
            };
        }
        WorkspaceEdit {
            document_changes: Some(lsp_types::DocumentChanges::Edits(
                files
                    .into_iter()
                    .filter_map(|(uri, edits)| {
                        Some(TextDocumentEdit {
                            text_document: OptionalVersionedTextDocumentIdentifier {
                                version: self.open.get(&uri).and_then(|open| open.version),
                                uri: uri.to_lsp().ok()?,
                            },
                            edits: edits.into_iter().map(OneOf::Left).collect(),
                        })
                    })
                    .collect(),
            )),
            ..WorkspaceEdit::default()
        }
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    /// `textDocument/completion`.
    ///
    /// The list is `isIncomplete` when the cap dropped rows, which is the only way it can fail
    /// to hold something a longer prefix would reach — every filter behind it is a subsequence
    /// match. Below the cap the client narrows what it already has and sends nothing, which is
    /// a whole request per keystroke. See `analysis::completion` for that argument in full, and
    /// for what is exact here and what is a guess.
    fn completion(&self, params: serde_json::Value) -> Option<CompletionResponse> {
        let params: lsp_types::CompletionParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::from_lsp(&params.text_document_position.text_document.uri)?;
        let started = Instant::now();

        // The one request that has to know a template from a Ruby file, and it has to ask the
        // *source* rather than the blanked view, because the view is where the markup went. A
        // half-typed word in an `<h1>` is spaces by the time completion sees it, so most of the
        // time this changes nothing — but a caret in markup that happens to sit after a run of
        // Ruby-looking bytes is a list of the workspace's constants offered to someone writing
        // prose, and this is the only request that can produce one without a token under it.
        if erb::is_template_uri(&uri)
            && !self
                .with_source(&uri, |source| {
                    let offset =
                        TextDocument::new(source.to_owned(), self.encoding).offset_at(position);
                    erb::in_ruby(source, offset as usize)
                })
                .unwrap_or(false)
        {
            return None;
        }

        let read = |uri: &str| self.text_of(uri);
        let sources = self.sources(&read);
        let (completion, range) = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // How this buffer's offsets relate to the graph's. The identity unless the index
            // was deferred, in which case it is the whole reason this answer can be trusted.
            let rebase = self.rebase_for(&uri, text.text());
            if !rebase.is_identity() {
                tracing::debug!("answering completion from a graph {rebase:?} behind the buffer");
            }
            let completion = completion::complete(
                &sources,
                UriId::from(uri.as_str()),
                text.text(),
                offset,
                MAX_COMPLETION_ITEMS,
                &self.own_documents(),
                &rebase,
            )?;
            let range = text.range_at(completion.start, completion.end);
            Some((completion, range))
        })??;

        tracing::debug!(
            "completion -> {} items in {:.2?}",
            completion.items.len(),
            started.elapsed()
        );

        let items: Vec<CompletionItem> = completion
            .items
            .into_iter()
            .enumerate()
            .map(|(index, item)| CompletionItem {
                kind: Some(completion_kind(item.kind)),
                detail: item.detail,
                documentation: item.documentation.map(|value| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    })
                }),
                tags: item.deprecated.then(|| vec![CompletionItemTag::DEPRECATED]),
                // Clients sort by their own fuzzy score first and fall back to this, so the
                // ranking survives as the tiebreak. Zero-padded because it is compared as text.
                sort_text: Some(format!("{index:05}")),
                // The span the cursor's half-typed word occupies, so that accepting `empty?`
                // over `emp` replaces it rather than appending to it.
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: item.label.clone(),
                })),
                // Only what `completionItem/resolve` needs to find the declaration again. A
                // string, not a number: a `DeclarationId` is a 64-bit hash and JSON numbers are
                // doubles, so the round trip through a client would corrupt it.
                data: item
                    .declaration
                    .map(|id| completion_data(id, completion.precise, completion.guess.as_deref())),
                label: item.label,
                ..CompletionItem::default()
            })
            .collect();

        Some(CompletionResponse::List(CompletionList {
            is_incomplete: completion.incomplete,
            items,
        }))
    }

    /// `completionItem/resolve` — the documentation for the one row the user is looking at.
    ///
    /// Reading a declaration's comments means reaching into its definitions, and a list of five
    /// hundred rows is five hundred of those for a single one anybody reads. LSP exists to avoid
    /// exactly that, so the list ships without documentation and this fills it in.
    fn resolve_completion(&self, params: serde_json::Value) -> Option<CompletionItem> {
        let mut item: CompletionItem = parse_params(params)?;

        let markdown =
            completion_target(item.data.as_ref()).and_then(|(declaration, precise, guess)| {
                hover::markdown(
                    &self.graph,
                    &self.synthesized,
                    &locator::Resolution {
                        declarations: vec![declaration],
                        // What the list was built from, carried back on the item: a row off the
                        // name-based list is a guess and its card has to say so. It said `true`
                        // here for two releases, which made every guessed row's card read as
                        // certain.
                        precise,
                        redirected: false,
                        // The other guess, and a different one: the rows are a real class's
                        // members and the class itself was read off the receiver's name.
                        derivation: types::Derivation {
                            guess,
                            ..types::Derivation::default()
                        },
                    },
                    None,
                )
            });

        if let Some(value) = markdown {
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }));
        }
        // Always the item, never null: the client sent one and the protocol says it gets one
        // back, enriched or not.
        Some(item)
    }

    /// The documents that are the user's own code, as a set the reference scan can test.
    fn own_documents(&self) -> std::collections::HashSet<UriId> {
        self.graph
            .documents()
            .iter()
            .filter(|(_, document)| self.is_own_code(document.uri()))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Turn a graph site into a link, reading the target file to place its ranges.
    fn link(&self, origin: lsp_types::Range, site: &Site) -> Option<LocationLink> {
        let target = DocUri::from_uri_str(&site.uri)?;
        let target_uri = target.to_lsp().ok()?;
        // **The target's map and not the requester's.** A jump can land in any document, and any
        // open buffer may be a keystroke ahead of the graph — so the span is moved into *that*
        // file's coordinates. A document nobody is typing in has the identity, which is every
        // document nobody has typed in since the last settle.
        let (target_range, target_selection_range) = self.with_text(&target, |text| {
            let rebase = self.rebase_for(&target, text.text());
            Some((
                text.range_at(
                    rebase.to_buffer(site.full.0)?,
                    rebase.to_buffer(site.full.1)?,
                ),
                text.range_at(
                    rebase.to_buffer(site.selection.0)?,
                    rebase.to_buffer(site.selection.1)?,
                ),
            ))
        })??;
        Some(LocationLink {
            origin_selection_range: Some(origin),
            target_uri,
            target_range,
            target_selection_range,
        })
    }

    /// Where a `require` points, if the graph has that file.
    ///
    /// `require_relative` resolves against the requiring file's own directory; a plain
    /// `require` resolves against the configured load paths, exactly as Ruby walks
    /// `$LOAD_PATH`.
    fn require_site(&self, from: &DocUri, require: &requires::Require) -> Option<Site> {
        let load_paths = if require.relative {
            vec![from.to_path()?.parent()?.to_path_buf()]
        } else {
            self.workspace.load_paths()
        };
        locator::require_site(&self.graph, &require.path, &load_paths)
    }

    fn respond(&self, response: Response) {
        // A send failure means the client is gone; the main loop is already tearing down.
        let _ = self.outgoing.send(Message::Response(response));
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

/// A rename that has been checked against the bytes it would replace.
///
/// Byte spans rather than ranges, because the two things done with them are a containment test
/// against the cursor and a conversion — and the first is arithmetic on offsets where it is a
/// two-field comparison on positions.
#[derive(Debug)]
struct Renaming {
    /// The name every one of these spans currently holds.
    name: String,
    /// Whether the replacement has to be a constant name rather than a variable's.
    constant: bool,
    /// What to replace, by document, each file's own in order and the files by URI.
    files: Vec<(DocUri, Vec<Replacement>)>,
}

/// One confirmed replacement: where it is in bytes, and the range the client is sent.
///
/// Both, out of the one read. The offsets are what a cursor position is compared against and
/// the range is what goes on the wire, and converting the second from the first a second time —
/// in the other handler, from a file possibly read again — is two chances for them to disagree
/// about the same span.
#[derive(Debug)]
struct Replacement {
    start: u32,
    end: u32,
    range: lsp_types::Range,
}

/// The last segment of a URI's path, which is what a message about a file names.
fn file_name(uri: &DocUri) -> String {
    uri.as_str()
        .rsplit_once('/')
        .map_or(uri.as_str(), |(_, name)| name)
        .to_owned()
}

/// Byte spans to LSP ranges, reading each document at most once.
///
/// A project-wide answer names hundreds of spans across a handful of files, and converting one
/// span means reading and line-indexing the whole file it is in. Doing that per span reads the
/// same file once per hit in it — which on a file with fifty references to a method is fifty
/// reads of the same bytes.
struct Ranges<'a> {
    analysis: &'a Analysis,
    /// `None` for a document that could not be read, so a missing file is not re-attempted per
    /// span either.
    read: HashMap<DocUri, Option<TextDocument>>,
}

impl<'a> Ranges<'a> {
    fn new(analysis: &'a Analysis) -> Self {
        Self {
            analysis,
            read: HashMap::new(),
        }
    }

    fn at(&mut self, uri: &DocUri, start: u32, end: u32) -> Option<lsp_types::Range> {
        self.with(uri, |text| text.range_at(start, end))
    }

    /// The bytes a span covers, for a caller that has to know what it is about to replace.
    ///
    /// `None` for a span that runs past the end of the text as it is *now*, which is what a
    /// plan made against a file that has since been edited looks like — and one more reason
    /// `rename` reads every span back rather than trusting the offsets it was given.
    fn text_at(&mut self, uri: &DocUri, start: u32, end: u32) -> Option<String> {
        self.with(uri, |text| {
            Some(text.text().get(start as usize..end as usize)?.to_owned())
        })?
    }

    fn with<R>(&mut self, uri: &DocUri, read: impl FnOnce(&TextDocument) -> R) -> Option<R> {
        // An open buffer shadows disk and is already indexed; never cached, because the copy
        // would go stale the moment the user types.
        if let Some(open) = self.analysis.open.get(uri) {
            return Some(read(&open.text));
        }
        let encoding = self.analysis.encoding;
        self.read
            .entry(uri.clone())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(uri.to_path()?).ok()?;
                Some(TextDocument::new(text, encoding))
            })
            .as_ref()
            .map(read)
    }
}

/// Wrap a handler's answer as a successful response.
///
/// `None` becomes JSON `null`, which is how LSP spells "there is nothing here". An error
/// response would be wrong: editors surface those to the user, and "no definition found" is
/// not something to complain about.
/// ya-lsp's own suggestion kinds, in LSP's vocabulary.
/// The declaration an item's `data` field names, as three requests round-trip one.
///
/// A decimal string rather than a JSON number, because a `DeclarationId` is a 64-bit hash and
/// JSON numbers are doubles — the round trip through a client would corrupt it. Everything about
/// the value is the client's word: it echoes back whatever the list it is looking at carried, and
/// a config reload drops the graph that list was built from, so nothing here may assume the id
/// still names anything.
/// What `completionItem/resolve` is given to find the row again.
///
/// An object rather than the bare id string the type hierarchy sends, because the card also has
/// to say which tier the row came from — and the row is the only thing that still knows by the
/// time the client asks. Both halves are strings: a `DeclarationId` is a 64-bit hash and JSON
/// numbers are doubles, so the round trip through a client would corrupt a number.
fn completion_data(
    declaration: DeclarationId,
    precise: bool,
    guess: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "declaration": declaration.get().to_string(),
        "precise": precise,
        // Absent rather than null when the receiver was not guessed, so that the common row
        // carries two fields and not three.
        "guess": guess,
    })
}

/// The declaration a resolve request names, and which tier its list came from.
///
/// Anything else — a stale id, an older client's bare string, nothing at all — answers `None`,
/// and the item comes back unenriched. The protocol says it comes back either way. The guess is
/// optional where `precise` is not: a client holding a list from an older server still
/// resolves, it just cannot say a thing it was never told.
fn completion_target(
    data: Option<&serde_json::Value>,
) -> Option<(DeclarationId, bool, Option<String>)> {
    let data = data?;
    let declaration = declaration_in(data.get("declaration"))?;
    let guess = data
        .get("guess")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some((declaration, data.get("precise")?.as_bool()?, guess))
}

fn declaration_in(data: Option<&serde_json::Value>) -> Option<DeclarationId> {
    data?
        .as_str()?
        .parse::<u64>()
        .ok()
        .filter(|raw| *raw != 0)
        .map(DeclarationId::new)
}

/// Whether a request's answer is drawn from the graph, and so has to wait for it to be linked.
///
/// `foldingRange`, `selectionRange` and `semanticTokens/full` are pure functions of one buffer —
/// neither `analysis::ranges` nor `analysis::tokens` ever sees a `Graph` — so they are the three
/// that do not. Semantic tokens fire on the first keystroke in a newly opened file, which is
/// exactly when a cold index is still being built, and waiting would leave the file uncoloured
/// for as long as that takes.
/// Whether this request may be answered without indexing what has just been typed.
///
/// **The three a caret asks**, which is the set whose whole answer is a function of the
/// cursor and the graph. Each needs `Rebase::to_graph` on the way in; `hover` and
/// `definition` also need `to_buffer` on the way out, because they answer with a span
/// `locate` found in the graph and `definition`'s span can be in a different document
/// again — see `Analysis::link`.
///
/// The rest are not excluded on principle but on demand: `references`, `documentHighlight`
/// and the hierarchies answer about the *workspace* rather than about the caret, nobody
/// asks them between two keystrokes, and each would need every span it returns mapped
/// through the map of whichever document it came from. `signatureHelp` reads the buffer and
/// never the graph, and the four in `needs_the_graph`'s exemption list do not settle at
/// all.
fn defers(method: &str) -> bool {
    matches!(
        method,
        "textDocument/completion" | "textDocument/hover" | "textDocument/definition"
    )
}

fn needs_the_graph(method: &str) -> bool {
    !matches!(
        method,
        "textDocument/foldingRange"
            | "textDocument/selectionRange"
            | "textDocument/semanticTokens/full"
            | "textDocument/codeAction"
    )
}

fn completion_kind(kind: completion::Kind) -> CompletionItemKind {
    match kind {
        completion::Kind::Class => CompletionItemKind::CLASS,
        completion::Kind::Module => CompletionItemKind::MODULE,
        completion::Kind::Constant => CompletionItemKind::CONSTANT,
        completion::Kind::Method => CompletionItemKind::METHOD,
        completion::Kind::Variable => CompletionItemKind::VARIABLE,
        completion::Kind::Field => CompletionItemKind::FIELD,
        completion::Kind::Keyword => CompletionItemKind::KEYWORD,
    }
}

/// Whether a response carries no answer at all, in either of the two shapes one can take.
///
/// `reply(id, None)` serialises to `null`, and a `CompletionList` that matched nothing is an
/// empty `items`. A deferred request retries on both: the refusal that motivates the retry
/// produces the first, and an empty list is cheap enough to re-derive that telling them apart
/// would be a distinction with no consequence.
fn answered_nothing(response: &Response) -> bool {
    match &response.response_result {
        Ok(serde_json::Value::Null) => true,
        Ok(value) => value
            .get("items")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty),
        Err(_) => false,
    }
}

fn reply<T: serde::Serialize>(id: &RequestId, value: Option<T>) -> Response {
    match serde_json::to_value(value) {
        Ok(result) => Response {
            id: id.clone(),
            response_result: Ok(result),
        },
        Err(error) => Response::new_err(
            id.clone(),
            ErrorCode::InternalError as i32,
            format!("could not serialise the response: {error}"),
        ),
    }
}

/// Request params are client input: malformed ones get logged and answered with `null`, never
/// a panic.
fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Option<T> {
    match serde_json::from_value(params) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            tracing::warn!("malformed request params: {error}");
            None
        }
    }
}

/// The run loop itself, driven over the real channel by the real thread.
#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod threaded_tests;

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// The LSP position of the first occurrence of `needle`.
    ///
    /// Every fixture here is ASCII, so counting characters is counting bytes; `position.rs`
    /// owns the cases where that is not true.
    fn position_of(source: &str, needle: &str) -> serde_json::Value {
        let offset = source
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in fixture"));
        let line = source[..offset].matches('\n').count();
        let column = offset - source[..offset].rfind('\n').map_or(0, |index| index + 1);
        serde_json::json!({ "line": line, "character": column })
    }

    /// The LSP position of the `~` in a fixture, which is removed before it is sent.
    fn marked_position(marked: &str) -> serde_json::Value {
        let offset = marked.find('~').expect("a ~ marking the cursor");
        let line = marked[..offset].matches('\n').count();
        let character = offset - marked[..offset].rfind('\n').map_or(0, |index| index + 1);
        serde_json::json!({ "line": line, "character": character })
    }

    /// The selection a fixture marks with two `~`, as the protocol spells one — or an empty
    /// range at a single `~`, which is what an editor sends when nothing is selected.
    fn marked_range(marked: &str) -> serde_json::Value {
        let start = marked.find('~').expect("a ~ marking the selection");
        let rest = marked.replacen('~', "", 1);
        let end = rest[start..]
            .find('~')
            .map_or(start, |offset| start + offset);
        let at = |offset: usize| {
            serde_json::json!({
                "line": rest[..offset].matches('\n').count(),
                "character": offset - rest[..offset].rfind('\n').map_or(0, |index| index + 1),
            })
        };
        serde_json::json!({ "start": at(start), "end": at(end) })
    }

    /// A `textDocument/signatureHelp` response drawn the way an editor draws it: every
    /// signature on its own line, the active parameter of the active one underlined beneath it,
    /// and the documentation last.
    ///
    /// Rendering the offsets rather than asserting on them is the point. A span that is off by
    /// one draws under the wrong text, which is visible at a glance and reads as the bug it is;
    /// a pair of numbers in an `assert_eq!` shows nobody anything. The underline counts
    /// characters where the protocol counts UTF-16 code units, which every fixture here is
    /// ASCII enough for — `render`'s own tests are where the two are made to differ.
    fn drawn(help: &serde_json::Value) -> String {
        let Some(signatures) = help["signatures"].as_array() else {
            return "null".to_owned();
        };
        let chosen = help["activeSignature"].as_u64().unwrap_or_default();

        let mut lines: Vec<String> = Vec::new();
        for (index, signature) in signatures.iter().enumerate() {
            lines.push(signature["label"].as_str().unwrap_or_default().to_owned());
            if index as u64 != chosen {
                continue;
            }
            let span = signature["activeParameter"]
                .as_u64()
                .and_then(|active| signature["parameters"].as_array()?.get(active as usize))
                .and_then(|parameter| parameter["label"].as_array());
            if let Some(span) = span {
                let start = span[0].as_u64().unwrap_or_default() as usize;
                let end = span[1].as_u64().unwrap_or_default() as usize;
                lines.push(format!(
                    "{}{}",
                    " ".repeat(start),
                    "~".repeat(end.saturating_sub(start))
                ));
            }
        }
        if let Some(documentation) = signatures
            .first()
            .and_then(|signature| signature["documentation"]["value"].as_str())
        {
            lines.push(documentation.to_owned());
        }
        lines.join("\n")
    }

    /// A type hierarchy answer, one row a line: the Ruby keyword for the kind, the name, and
    /// the detail column.
    fn drawn_hierarchy(answer: &serde_json::Value) -> String {
        let Some(items) = answer.as_array() else {
            return "null".to_owned();
        };
        items
            .iter()
            .map(|item| {
                // LSP numbers `Module` 2 and `Class` 5. Anything else is printed rather than
                // panicked over, so a wrong kind reads as a wrong row instead of a lost test.
                let keyword = match item["kind"].as_u64() {
                    Some(2) => "module".to_owned(),
                    Some(5) => "class".to_owned(),
                    other => format!("kind {other:?}"),
                };
                format!(
                    "{keyword} {} — {}",
                    item["name"].as_str().unwrap_or_default(),
                    item["detail"].as_str().unwrap_or("(no detail)"),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The edits in a `WorkspaceEdit`, by file, in whichever of the two shapes it arrived in.
    ///
    /// Both are read here because ya-lsp sends both: `documentChanges` to a client that
    /// advertised it and the older `changes` map to one that did not, and a test that could only
    /// read one of them would be blind to half of what ships.
    fn edits_in(answer: &serde_json::Value) -> Vec<(String, Vec<lsp_types::TextEdit>)> {
        if let Some(changes) = answer["documentChanges"].as_array() {
            return changes
                .iter()
                .map(|change| {
                    (
                        change["textDocument"]["uri"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                        serde_json::from_value(change["edits"].clone()).expect("well-formed edits"),
                    )
                })
                .collect();
        }
        let mut files: Vec<(String, Vec<lsp_types::TextEdit>)> = answer["changes"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(uri, edits)| {
                (
                    uri.clone(),
                    serde_json::from_value(edits.clone()).expect("well-formed edits"),
                )
            })
            .collect();
        // A JSON object has no order of its own, and the older shape is one.
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    /// Every symbol in an outline, parents and children alike, in no particular order.
    fn all_symbols(outline: &serde_json::Value) -> Vec<&serde_json::Value> {
        let mut queue: Vec<&serde_json::Value> = outline
            .as_array()
            .map(|list| list.iter().collect())
            .unwrap_or_default();
        let mut flat = Vec::new();
        while let Some(symbol) = queue.pop() {
            if let Some(children) = symbol["children"].as_array() {
                queue.extend(children);
            }
            flat.push(symbol);
        }
        flat
    }

    /// The symbols whose `selectionRange` their own `range` does not contain.
    ///
    /// The protocol requires containment, and VS Code enforces it by throwing — which discards
    /// the whole outline, not the one bad symbol. "None" is the assertion; naming the offenders
    /// is what makes a failure readable.
    fn uncontained(outline: &serde_json::Value) -> Vec<String> {
        fn point(value: &serde_json::Value) -> (u64, u64) {
            (
                value["line"].as_u64().unwrap_or_default(),
                value["character"].as_u64().unwrap_or_default(),
            )
        }
        all_symbols(outline)
            .into_iter()
            .filter(|symbol| {
                let (range, selection) = (&symbol["range"], &symbol["selectionRange"]);
                point(&selection["start"]) < point(&range["start"])
                    || point(&selection["end"]) > point(&range["end"])
            })
            .map(|symbol| {
                format!(
                    "{}: range {:?}..{:?}, selection {:?}..{:?}",
                    symbol["name"],
                    point(&symbol["range"]["start"]),
                    point(&symbol["range"]["end"]),
                    point(&symbol["selectionRange"]["start"]),
                    point(&symbol["selectionRange"]["end"]),
                )
            })
            .collect()
    }

    /// An `Analysis` wired to a discarded output channel, over a throwaway workspace.
    struct Harness {
        analysis: Analysis,
        outgoing: Receiver<Message>,
        /// Notifications `ask` stepped over on its way to a response. Without this, asking a
        /// question silently throws away every `showMessage` and `publishDiagnostics` the
        /// handler sent first, and a test that looks for one quietly cannot find it.
        stashed: std::cell::RefCell<Vec<Message>>,
        root: tempfile::TempDir,
        version: i32,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_encoding(PositionEncoding::Utf16)
        }

        fn with_encoding(encoding: PositionEncoding) -> Self {
            Self::at(tempfile::tempdir().expect("tempdir"), encoding)
        }

        fn at(root: tempfile::TempDir, encoding: PositionEncoding) -> Self {
            // An empty environment, so gem discovery cannot wander off into whatever Ruby the
            // machine running the tests happens to have installed.
            Self::at_with_env(root, encoding, gems::Env::default())
        }

        fn at_with_env(
            root: tempfile::TempDir,
            encoding: PositionEncoding,
            env: gems::Env,
        ) -> Self {
            // `Env::default()` carries no gem roots at all, system ones included, so nothing
            // here can reach the machine's Ruby. This still turns the two off: extracting and
            // indexing the vendored signatures is ~800 files of work that no test in this
            // module is asking about. A fixture that wrote its own configuration keeps it.
            let config = root.path().join("ya-lsp.toml");
            if !config.exists() {
                std::fs::write(
                    &config,
                    "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
                )
                .unwrap();
            }

            let (sender, receiver) = crossbeam_channel::unbounded();
            let (workspace, problems) =
                Workspace::load_with_env(root.path().to_path_buf(), None, env);
            assert!(problems.is_empty(), "{problems:?}");

            Self {
                analysis: Analysis::new(
                    workspace,
                    encoding,
                    // Tests exercise the richer shapes; the flat ones get their own test.
                    ClientSupport {
                        hierarchical_symbols: true,
                        definition_links: true,
                        work_done_progress: true,
                        versioned_edits: true,
                    },
                    sender,
                    Cancellations::default(),
                ),
                outgoing: receiver,
                stashed: std::cell::RefCell::new(Vec::new()),
                root,
                version: 1,
            }
        }

        fn write(&self, relative: &str, source: &str) -> DocUri {
            let path = self.root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, source).unwrap();
            DocUri::from_path(&path).unwrap()
        }

        /// Index the workspace and push the first round of diagnostics, as `spawn` does.
        fn index(&mut self) {
            self.analysis.index_workspace();
            self.analysis.publish_diagnostics();
        }

        /// Queue the gems and run the background index to completion, as the run loop would
        /// during a stretch with no editor traffic.
        fn index_gems(&mut self) {
            self.analysis.queue_background_indexing();
            while self.analysis.step_gem_indexing() {}
            self.analysis.settle();
        }

        /// Every notification sent since the last drain, in order, including the ones `ask`
        /// stepped over.
        fn notifications(&self, method: &str) -> Vec<lsp_server::Notification> {
            let mut sent: Vec<Message> = self.stashed.borrow_mut().drain(..).collect();
            while let Ok(message) = self.outgoing.try_recv() {
                sent.push(message);
            }
            sent.into_iter()
                .filter_map(|message| match message {
                    Message::Notification(notification) if notification.method == method => {
                        Some(notification)
                    }
                    _ => None,
                })
                .collect()
        }

        /// Every `window/showMessage` body sent since the last drain.
        fn messages(&self) -> Vec<String> {
            self.notifications("window/showMessage")
                .into_iter()
                .map(|notification| {
                    notification.params["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        }

        /// Every `$/progress` payload sent so far, as `(kind, message)`.
        fn progress(&self) -> Vec<(String, String)> {
            self.notifications("$/progress")
                .into_iter()
                .map(|notification| {
                    let value = &notification.params["value"];
                    (
                        value["kind"].as_str().unwrap_or_default().to_owned(),
                        value["message"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        }

        /// Every `publishDiagnostics` sent since the last drain, in order.
        fn published(&self) -> Vec<(String, Vec<lsp_types::Diagnostic>)> {
            self.notifications("textDocument/publishDiagnostics")
                .into_iter()
                .map(|notification| {
                    let params: lsp_types::PublishDiagnosticsParams =
                        serde_json::from_value(notification.params).expect("well-formed params");
                    (params.uri.as_str().to_owned(), params.diagnostics)
                })
                .collect()
        }

        /// The diagnostics most recently published for `uri`, or `None` if none ever were.
        fn latest(&self, uri: &DocUri) -> Option<Vec<lsp_types::Diagnostic>> {
            self.published()
                .into_iter()
                .rfind(|(sent, _)| sent == uri.as_str())
                .map(|(_, items)| items)
        }

        /// A `workspace/didChangeWatchedFiles`, as a client sends it: the file system changed
        /// and nothing else did — no `didOpen`, no `didSave`, no buffer anywhere.
        fn watch(&mut self, uris: &[&DocUri]) {
            self.run(Task::WatchedFiles {
                uris: uris.iter().map(|uri| (*uri).clone()).collect(),
            });
        }

        fn open(&mut self, uri: &DocUri, text: &str) {
            self.run(Task::DidOpen {
                uri: uri.clone(),
                text: text.to_owned(),
                version: Some(1),
            });
        }

        /// A whole-buffer change, which is what a client sends for a paste or a revert.
        fn change(&mut self, uri: &DocUri, text: &str) {
            self.edit(
                uri,
                vec![TextChange {
                    range: None,
                    text: text.to_owned(),
                }],
            );
        }

        /// A `didChange` with **no settle behind it**, which is the only way to reach the
        /// state a deferred index creates.
        ///
        /// `Harness::run` settles whenever the analysis is dirty, so an ordinary `edit` leaves
        /// the graph current and every rebase the identity — which made the first draft of the
        /// three tests below pass without exercising a single line of `Rebase`. The deferred
        /// server never settles here either: it answers, and the index catches up on the
        /// debounce.
        fn edit_without_indexing(&mut self, uri: &DocUri, changes: Vec<TextChange>) {
            self.version += 1;
            let version = self.version;
            self.analysis.handle(Task::DidChange {
                uri: uri.clone(),
                changes,
                version: Some(version),
            });
        }

        fn edit(&mut self, uri: &DocUri, changes: Vec<TextChange>) {
            self.version += 1;
            let version = self.version;
            self.run(Task::DidChange {
                uri: uri.clone(),
                changes,
                version: Some(version),
            });
        }

        /// Send a request through the real dispatch path and take its result.
        ///
        /// Going through `serve` rather than calling the handler keeps the tests honest about
        /// settling, cancellation, and the `null`-versus-error distinction.
        fn ask(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            let id = RequestId::from(1);
            self.analysis.serve(Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            });
            while let Ok(message) = self.outgoing.try_recv() {
                match message {
                    Message::Response(response) if response.id == id => {
                        return response.response_result.expect("handlers never error");
                    }
                    other => self.stashed.borrow_mut().push(other),
                }
            }
            panic!("{method} was never answered");
        }

        /// The same, for a request whose answer may be an error rather than a result.
        ///
        /// `ask` unwraps, which is right for the eighteen handlers — none of them can fail —
        /// and wrong for the one thing that can now answer an error without anybody's handler
        /// deciding to: a request that crashed.
        fn ask_raw(&mut self, method: &str, params: serde_json::Value) -> Response {
            let id = RequestId::from(1);
            self.analysis.serve(Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            });
            while let Ok(message) = self.outgoing.try_recv() {
                match message {
                    Message::Response(response) if response.id == id => return response,
                    other => self.stashed.borrow_mut().push(other),
                }
            }
            panic!("{method} was never answered");
        }

        /// Index RBS as though something had read `source` and generated it.
        ///
        /// A stand-in producer for the side table, deliberately in the tests rather than in
        /// the server: the mapping is built *before* the first
        /// generator, because a version that types `@story.title` and then jumps to a file the
        /// user does not have is worse than one that does not type it.
        fn synthesize(
            &mut self,
            source: &DocUri,
            rbs: &str,
            mappings: Vec<synthesized::Mapping>,
        ) -> String {
            let uri = self.analysis.synthesized.record(
                &mut self.analysis.graph,
                &mut self.analysis.types,
                source,
                rbs,
                mappings,
            );
            // What every other indexing entry point leaves to its caller, for the same reason:
            // a batch marks itself dirty once rather than per file.
            self.analysis.mark_dirty();
            self.analysis.settle();
            uri
        }

        fn hover_at(&mut self, uri: &DocUri, source: &str, needle: &str) -> serde_json::Value {
            self.ask(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, needle),
                }),
            )
        }

        fn definition_at(&mut self, uri: &DocUri, source: &str, needle: &str) -> serde_json::Value {
            self.ask(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, needle),
                }),
            )
        }

        fn references_at(
            &mut self,
            uri: &DocUri,
            source: &str,
            needle: &str,
            include_declaration: bool,
        ) -> serde_json::Value {
            self.ask(
                "textDocument/references",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, needle),
                    "context": { "includeDeclaration": include_declaration },
                }),
            )
        }

        /// References as `file.rb:line:character`, which is short enough to assert on whole.
        fn reference_list(
            &mut self,
            uri: &DocUri,
            source: &str,
            needle: &str,
            include_declaration: bool,
        ) -> Vec<String> {
            let found = self.references_at(uri, source, needle, include_declaration);
            let Some(locations) = found.as_array() else {
                return Vec::new();
            };
            locations
                .iter()
                .map(|location| {
                    let file = location["uri"]
                        .as_str()
                        .unwrap_or_default()
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    let start = &location["range"]["start"];
                    format!("{file}:{}:{}", start["line"], start["character"])
                })
                .collect()
        }

        fn symbol_search(&mut self, query: &str) -> serde_json::Value {
            self.ask("workspace/symbol", serde_json::json!({ "query": query }))
        }

        /// Search results as `name` or `Container#name`, in the order they were ranked.
        fn symbol_names(&mut self, query: &str) -> Vec<String> {
            let found = self.symbol_search(query);
            let Some(symbols) = found.as_array() else {
                return Vec::new();
            };
            symbols
                .iter()
                .map(|symbol| {
                    let name = symbol["name"].as_str().unwrap_or_default();
                    match symbol["containerName"].as_str() {
                        Some(container) => format!("{container}#{name}"),
                        None => name.to_owned(),
                    }
                })
                .collect()
        }

        /// Open a buffer written with a `~` where the cursor is, and ask what completes there.
        fn complete(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
            let position = marked_position(marked);
            self.open(uri, &marked.replace('~', ""));
            self.ask(
                "textDocument/completion",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position,
                }),
            )
        }

        /// Open a buffer written with a `~` where the cursor is, and ask what call it is inside.
        fn signature(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
            let position = marked_position(marked);
            self.open(uri, &marked.replace('~', ""));
            self.ask(
                "textDocument/signatureHelp",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position,
                }),
            )
        }

        /// The signature card at the `~`, drawn the way an editor does it.
        fn signature_card(&mut self, uri: &DocUri, marked: &str) -> String {
            drawn(&self.signature(uri, marked))
        }

        /// Open a buffer written with a `~` where the cursor is, and ask what expanding the
        /// selection from it reaches.
        fn selection(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
            let position = marked_position(marked);
            self.open(uri, &marked.replace('~', ""));
            self.ask(
                "textDocument/selectionRange",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "positions": [position],
                }),
            )
        }

        /// Open a buffer and ask what folds in it.
        fn folding(&mut self, uri: &DocUri, source: &str) -> serde_json::Value {
            self.open(uri, source);
            self.ask(
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            )
        }

        /// The code actions offered over a buffer marked with one or two `~`, drawn as each
        /// title followed by the file the action leaves behind.
        fn actions(&mut self, uri: &DocUri, marked: &str) -> String {
            let source = marked.replace('~', "");
            self.open(uri, &source);
            let answer = self.ask(
                "textDocument/codeAction",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "range": marked_range(marked),
                    "context": { "diagnostics": [] },
                }),
            );
            let Some(actions) = answer.as_array() else {
                return "null".to_owned();
            };
            let mut drawn = Vec::new();
            for action in actions {
                let mut text = TextDocument::new(source.clone(), self.analysis.encoding);
                let (_, edits) = edits_in(&action["edit"])
                    .pop()
                    .expect("every action edits exactly one file");
                // Back to front, as the client applies them: every range was computed against
                // the text as it stands, so applying one must not move the next.
                let mut edits = edits;
                edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
                for edit in &edits {
                    text.apply(Some(edit.range), &edit.new_text);
                }
                drawn.push(format!(
                    "--- {} [{}] ---\n{}",
                    action["title"].as_str().unwrap_or_default(),
                    action["kind"].as_str().unwrap_or_default(),
                    text.text()
                ));
            }
            drawn.join("")
        }

        /// Open a buffer written with a `~` where the cursor is, and ask what it highlights.
        fn highlight(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
            let position = marked_position(marked);
            self.open(uri, &marked.replace('~', ""));
            self.ask(
                "textDocument/documentHighlight",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position,
                }),
            )
        }

        /// What the editor would paint at the `~`: the file back, with a `w` under every byte
        /// of a write and an `r` under every byte of a read.
        ///
        /// Drawn rather than asserted as ranges for the reason the signature card is: a
        /// highlight one line or one column out is a bug anybody can see at a glance here, and
        /// a list of `{line, character}` pairs is a bug nobody can see at all. Lines with
        /// nothing on them are dropped so the assertion is about what lit up, but the ones that
        /// remain carry their own text — which is what makes "and not the one in the comment"
        /// something the fixture *shows* rather than something a test name claims.
        fn highlight_map(&mut self, uri: &DocUri, marked: &str) -> String {
            let found = self.highlight(uri, marked);
            let source = marked.replace('~', "");
            let Some(spans) = found.as_array() else {
                return "null".to_owned();
            };

            let mut masks: Vec<Vec<char>> = source
                .lines()
                .map(|line| vec![' '; line.chars().count()])
                .collect();
            for span in spans {
                let line = span["range"]["start"]["line"].as_u64().unwrap_or_default() as usize;
                let start = span["range"]["start"]["character"]
                    .as_u64()
                    .unwrap_or_default() as usize;
                let end = span["range"]["end"]["character"]
                    .as_u64()
                    .unwrap_or_default() as usize;
                // LSP numbers them `Text` 1, `Read` 2, `Write` 3; `Text` is never answered.
                let mark = if span["kind"].as_u64() == Some(3) {
                    'w'
                } else {
                    'r'
                };
                let Some(mask) = masks.get_mut(line) else {
                    continue;
                };
                for column in start..end {
                    if let Some(cell) = mask.get_mut(column) {
                        *cell = mark;
                    }
                }
            }

            let mut drawn = Vec::new();
            for (line, mask) in source.lines().zip(&masks) {
                if mask.iter().all(|cell| *cell == ' ') {
                    continue;
                }
                drawn.push(line.to_owned());
                drawn.push(mask.iter().collect::<String>().trim_end().to_owned());
            }
            drawn.join("\n")
        }

        /// Ask for the type hierarchy at the first occurrence of `needle`.
        fn prepare_hierarchy(
            &mut self,
            uri: &DocUri,
            source: &str,
            needle: &str,
        ) -> serde_json::Value {
            self.ask(
                "textDocument/prepareTypeHierarchy",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, needle),
                }),
            )
        }

        /// Prepare at `needle`, then expand the first item the way an editor does.
        ///
        /// The item is echoed back verbatim, `data` and all — which is the only way the two
        /// follow-ups are ever reached, and therefore the only honest way to test them.
        fn expand(
            &mut self,
            method: &str,
            uri: &DocUri,
            source: &str,
            needle: &str,
        ) -> serde_json::Value {
            let prepared = self.prepare_hierarchy(uri, source, needle);
            let item = prepared[0].clone();
            assert!(item.is_object(), "nothing to expand at {needle:?}");
            self.ask(method, serde_json::json!({ "item": item }))
        }

        /// The rows of a hierarchy answer, drawn the way Ruby writes what they are.
        ///
        /// The keyword makes the kind visible — `module Comparable` in a list of supertypes is
        /// the answer's most surprising claim and also its most correct one — and the detail
        /// column is what separates the project's two rows from the gems' eight. Drawn rather
        /// than asserted field by field, as the signature card and the highlight map are: a row
        /// in the wrong place, with the wrong kind, or pointing at the wrong file is one thing
        /// to read here and three assertions to write otherwise.
        fn hierarchy_rows(
            &mut self,
            method: &str,
            uri: &DocUri,
            source: &str,
            needle: &str,
        ) -> String {
            drawn_hierarchy(&self.expand(method, uri, source, needle))
        }

        fn prepare_rename(
            &mut self,
            uri: &DocUri,
            source: &str,
            needle: &str,
        ) -> serde_json::Value {
            self.ask(
                "textDocument/prepareRename",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, needle),
                }),
            )
        }

        /// Rename at `needle`, then draw every file the answer would change.
        ///
        /// **The assertion is the renamed Ruby**, which is the whole point of drawing it. A span
        /// one byte out writes code that is visibly broken — a name run into the one beside it,
        /// a hash key changed along with its value, an `end` eaten — where a list of
        /// `{line, character}` pairs shows nobody anything. The files no edit touched are not
        /// drawn, so what an expected block holds is exactly what the rename claims to change.
        ///
        /// The edits are applied through `TextDocument::apply`, the same code incremental sync
        /// uses, and in reverse so that each one lands before anything ahead of it has moved.
        fn renamed(&mut self, uri: &DocUri, source: &str, needle: &str, to: &str) -> String {
            let answer = self.ask(
                "textDocument/rename",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, needle),
                    "newName": to,
                }),
            );
            if answer.is_null() {
                return "null".to_owned();
            }
            let mut drawn = Vec::new();
            for (uri, edits) in edits_in(&answer) {
                let at = DocUri::from_uri_str(&uri).expect("a document URI");
                let mut text = TextDocument::new(
                    self.analysis
                        .with_text(&at, |text| text.text().to_owned())
                        .expect("readable text"),
                    self.analysis.encoding,
                );
                for edit in edits.iter().rev() {
                    text.apply(Some(edit.range), &edit.new_text);
                }
                drawn.push(format!("--- {} ---\n{}", file_name(&at), text.text()));
            }
            drawn.join("")
        }

        /// The labels offered at the `~`, in the order they were ranked.
        fn suggestions(&mut self, uri: &DocUri, marked: &str) -> Vec<String> {
            let found = self.complete(uri, marked);
            let Some(items) = found["items"].as_array() else {
                return Vec::new();
            };
            items
                .iter()
                .map(|item| item["label"].as_str().unwrap_or_default().to_owned())
                .collect()
        }

        /// The first `count` rows offered at the `~`, spelled the way the editor draws them.
        ///
        /// The detail is the owner, and including it is what makes an assertion here readable
        /// as a *ranking* rather than as a list of names — the owner is what the order is
        /// supposed to be about.
        fn first_rows(&mut self, uri: &DocUri, marked: &str, count: usize) -> Vec<String> {
            let found = self.complete(uri, marked);
            let Some(items) = found["items"].as_array() else {
                return Vec::new();
            };
            items
                .iter()
                .take(count)
                .map(|item| {
                    let label = item["label"].as_str().unwrap_or_default();
                    match item["detail"].as_str() {
                        Some(detail) => format!("{label}  {detail}"),
                        None => label.to_owned(),
                    }
                })
                .collect()
        }

        /// The labels offered at the `~`, with Ruby's keywords dropped.
        ///
        /// Keywords are in every expression list and are not what any of these tests are about;
        /// one test asserts they are there and the rest would be unreadable with them.
        fn declarations_at(&mut self, uri: &DocUri, marked: &str) -> Vec<String> {
            let found = self.complete(uri, marked);
            let Some(items) = found["items"].as_array() else {
                return Vec::new();
            };
            items
                .iter()
                .filter(|item| item["kind"].as_u64() != Some(14))
                .map(|item| item["label"].as_str().unwrap_or_default().to_owned())
                .collect()
        }

        fn outline(&mut self, uri: &DocUri) -> serde_json::Value {
            self.ask(
                "textDocument/documentSymbol",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            )
        }

        /// Run a task and let the debounced resolution finish, as the real loop would.
        fn run(&mut self, task: Task) {
            self.analysis.handle(task);
            if self.analysis.dirty {
                self.analysis.settle();
            }
        }

        /// rubydex spells method declarations with parentheses: `Person#shout()`, not
        /// `Person#shout`. Looking one up without them silently returns `None`.
        /// The RBS the generators wrote from one of the workspace's files.
        ///
        /// The text rather than its effects, for the handful of properties that are about
        /// what was written — optionality, arity, which of two annotations won — and would
        /// otherwise be asserted through three layers of lookup.
        fn generated_rbs(&self, relative: &str) -> String {
            let uri = DocUri::from_path(&self.root.path().join(relative)).expect("a file uri");
            self.analysis
                .synthesized
                .text(&uri)
                .unwrap_or_default()
                .to_owned()
        }

        /// Every document the generators wrote, text and all, in a stable order.
        fn every_generated_document(
            &self,
        ) -> std::collections::BTreeMap<rubydex::model::ids::UriId, String> {
            self.analysis
                .synthesized
                .every_document()
                .into_iter()
                .map(|(uri, rbs)| (uri, rbs.to_owned()))
                .collect()
        }

        fn has(&self, name: &str) -> bool {
            self.analysis.graph.get(name).is_some()
        }

        /// How many definitions the graph holds for one document.
        ///
        /// [`Harness::has`] cannot answer "was this indexed": a *declaration* is built by
        /// `Resolver::resolve` and a document that was indexed and not yet resolved has none,
        /// so it reads the same as one that was never indexed at all. A **definition** is put
        /// there by the indexing itself, which is what the deferral's tests ask.
        fn definitions_in(&self, uri: &DocUri) -> usize {
            self.analysis
                .graph
                .documents()
                .values()
                .find(|document| document.uri() == uri.as_str())
                .map_or(0, |document| document.definitions().len())
        }

        /// Documents rubydex knows about, minus the synthetic `rubydex:built-in` one.
        fn document_count(&self) -> usize {
            self.analysis
                .graph
                .documents()
                .values()
                .filter(|document| !document.uri().starts_with("rubydex:"))
                .count()
        }
    }

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

    /// The workspace walk, which is a *worker-thread* panic.
    ///
    /// Driven through `index_workspace` rather than by calling the indexer inline, because the
    /// two propagate differently: uncaught, this one reaches the analysis thread through
    /// rubydex's `handle.join().expect("Worker thread panicked")` and an inline call would
    /// never show that.
    #[test]
    fn a_file_that_crashes_the_indexer_costs_that_file_and_not_the_session() {
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        let bad = harness.write("app/rice.rb", indexer::CRASHES);
        harness.index();

        assert!(
            harness.has("Person#shout()"),
            "one bad file costs its own answers and nobody else's"
        );
        assert!(!harness.analysis.indexed(&bad));
        assert!(harness.analysis.skipped.contains(&bad));

        let said = harness.messages();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("something went wrong while reading")
                && said[0].contains("rice.rb"),
            "a gap somebody can see is a gap somebody can report: {said:?}"
        );
    }

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

    /// The request seam, which neither the indexing bulkhead nor `resolve`'s guard covers: an
    /// unguarded handler that panics takes the analysis thread with it, and a cursor on a `def`
    /// can reach one.
    ///
    /// Armed rather than provoked. The pinned rev fixes the unwraps that used to reach it — the
    /// test below asserts that Ruby *answers* — so the seam is exercised deliberately. It is kept
    /// because `create_declaration`'s unwraps survive upstream unchanged, and every handler under
    /// `dispatch` reads the graph.
    #[test]
    fn a_request_that_crashes_costs_its_own_answer_and_not_the_session() {
        let mut harness = Harness::new();
        let source = "class Bar; end\nAliased = Bar\nclass Aliased\n  def self.foo; end\nend\n";
        let uri = harness.write("app/bar.rb", source);
        let person = "class Person\n  def shout\n  end\nend\n";
        let elsewhere = harness.write("app/person.rb", person);
        harness.index();

        REQUESTS_TO_CRASH.set(1);
        let (response, logged) = crate::testing::captured_logs(tracing::Level::ERROR, || {
            harness.ask_raw(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, "foo"),
                }),
            )
        });
        assert_eq!(REQUESTS_TO_CRASH.get(), 0, "the crash was armed and taken");

        let error = response.response_result.expect_err("the request crashed");
        assert_eq!(error.code, ErrorCode::InternalError as i32);
        assert!(
            error.message.contains("textDocument/definition"),
            "{error:?}"
        );
        assert!(logged.contains("answers nothing"), "{logged}");

        assert!(
            !harness.definition_at(&elsewhere, person, "shout").is_null(),
            "the thread is alive and every other request still works"
        );
    }

    /// A constant alias reopened under its alias, which the pinned rev answers.
    ///
    /// `Graph::find_self_receiver_declaration` unwraps twice on 0.2.5 and upstream's `ab88ef1`
    /// fixes it, so this pins the fix: a pin moved back to 0.2.5 fails here rather than killing
    /// the test thread quietly under the guard above.
    #[test]
    fn a_constant_alias_reopened_under_its_alias_answers_rather_than_crashing() {
        let mut harness = Harness::new();
        let source = "class Bar; end\nAliased = Bar\nclass Aliased\n  def self.foo; end\nend\n";
        let uri = harness.write("app/bar.rb", source);
        harness.index();

        let call = "Bar.foo\nAliased.foo\n";
        let call_uri = harness.write("app/call.rb", call);
        harness.index();

        // The cursor on the `def` is what the report reached the unwraps through, and it
        // answers nothing — recorded rather than asserted away, because upstream's fix stops
        // the panic and does not make the alias resolve a definition to itself.
        assert!(harness.definition_at(&uri, source, "foo").is_null());
        // What a user actually writes does resolve, under either spelling of the constant.
        assert!(
            !harness.definition_at(&call_uri, call, "Bar.foo").is_null(),
            "the class's own name"
        );
        assert!(
            !harness
                .definition_at(&call_uri, call, "Aliased.foo")
                .is_null(),
            "and the alias it was reopened under"
        );
    }

    /// The other one: `extend self` inside a `Module.new` block.
    ///
    /// `ruby_indexer.rs:985` unwraps a lexical scope its own guard does not test for, which on
    /// 0.2.5 ends the analysis thread. What is asserted is not merely that it survives: the panic
    /// is the top-level face of a *wrong answer*, so the `def` inside the block has to be found
    /// where it really is.
    #[test]
    fn extend_self_in_an_anonymous_module_indexes_like_any_other_file() {
        let mut harness = Harness::new();
        let source = "Foo = Module.new do\n  extend self\n  def hello = \"hi\"\nend\n";
        let uri = harness.write("app/rice.rb", source);
        harness.index();

        assert!(harness.analysis.skipped.is_empty(), "nothing was skipped");
        assert!(harness.analysis.indexed(&uri));
        assert!(harness.has("Foo"), "the constant the block is assigned to");
        assert!(harness.messages().is_empty(), "and nobody had to be told");
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

    #[test]
    fn a_cancelled_request_is_answered_with_request_cancelled() {
        let harness = Harness::new();
        let id = RequestId::from(7);
        harness.analysis.cancellations.cancel(id.clone());

        let mut analysis = harness.analysis;
        analysis.serve(Request {
            id: id.clone(),
            method: "textDocument/hover".to_owned(),
            params: serde_json::Value::Null,
        });

        let Message::Response(response) = harness.outgoing.try_recv().expect("a response") else {
            panic!("expected a response");
        };
        assert_eq!(response.id, id);
        let error = response.response_result.expect_err("cancelled");
        assert_eq!(error.code, ErrorCode::RequestCanceled as i32);
    }

    // -----------------------------------------------------------------------
    // Diagnostics
    // -----------------------------------------------------------------------

    /// `class Foo` with no `end`: Prism reports it, and it is unambiguously the user's problem.
    const UNTERMINATED: &str = "class Foo\n  def bar\n";

    fn code(name: &str) -> Option<lsp_types::NumberOrString> {
        Some(lsp_types::NumberOrString::String(name.to_owned()))
    }

    #[test]
    fn parse_errors_read_the_way_prism_wrote_them() {
        // ya-lsp owns the severity and the `code` of a diagnostic and **not one word of the
        // text**: `diagnostic.message()` is forwarded verbatim. That is the decision, and it is
        // the right one — rewriting a parser's diagnostics is a real cost and a real risk of
        // saying something false about code the rewriter did not parse.
        //
        // What was wrong is that it was assumed rather than pinned. The only assertion anywhere
        // was that the message is non-empty, which is the same gap as an unranked completion
        // list: the mechanism tested, the content not. So the actual sentences are here. If
        // Prism rewrites one, this fails and someone reads the new wording and decides whether
        // users are better off — which is the entire point of a pass-through being deliberate.
        let mut harness = Harness::new();
        let uri = harness.write("lib/broken.rb", UNTERMINATED);
        harness.index();

        let items = harness.latest(&uri).expect("diagnostics");
        let said: Vec<(Option<String>, &str)> = items
            .iter()
            .map(|item| {
                (
                    match &item.code {
                        Some(lsp_types::NumberOrString::String(name)) => Some(name.clone()),
                        _ => None,
                    },
                    item.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            said,
            vec![
                (
                    Some("parse-error".to_owned()),
                    "expected an `end` to close the `class` statement",
                ),
                (
                    Some("parse-error".to_owned()),
                    "expected an `end` to close the `def` statement",
                ),
                (
                    Some("parse-warning".to_owned()),
                    "mismatched indentations at '\n' with 'def' at 2",
                ),
                (
                    Some("parse-error".to_owned()),
                    "unexpected end-of-input, assuming it is closing the parent top level \
                     context",
                ),
            ],
            "{items:?}"
        );
        // Two of these are worth reading twice. The indentation warning names the character it
        // mismatched against and that character is a newline, so a user sees a message with a
        // line break in the middle of it. The last says "assuming it is closing the parent top
        // level context", which is Prism explaining its own error recovery to someone who did
        // not ask. Neither is ya-lsp's to fix — but neither was anyone's to notice either,
        // until they were written down.
    }

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
    fn rules_that_fire_on_correct_ruby_are_off_until_asked_for() {
        // `class Child < base` is legal Ruby that rubydex cannot resolve statically. Squiggling
        // it by default would put a permanent warning on working code.
        let source = "base = Object\nclass Child < base\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/dynamic.rb", source);
        harness.index();

        assert!(
            harness.latest(&uri).is_none_or(|items| items
                .iter()
                .all(|item| item.code != code("dynamic-ancestor"))),
            "dynamic-ancestor must be silent by default"
        );

        // ... but turning it on in config must actually work.
        std::fs::write(
            harness
                .root
                .path()
                .join(crate::workspace::config::CONFIG_FILE_NAME),
            "[diagnostics.rules]\ndynamic-ancestor = \"warning\"\n",
        )
        .unwrap();
        harness.run(Task::ReloadConfig);

        let items = harness.latest(&uri).expect("now reported");
        let dynamic: Vec<_> = items
            .iter()
            .filter(|item| item.code == code("dynamic-ancestor"))
            .collect();
        assert!(!dynamic.is_empty(), "{items:?}");
        assert!(
            dynamic
                .iter()
                .all(|item| item.severity == Some(DiagnosticSeverity::WARNING))
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
    // Navigation
    // -----------------------------------------------------------------------

    const LIBRARY: &str = "\
# Someone with a name.
#
# Reopened below.
class Person
  MAX_AGE = 100

  attr_reader :name

  # Build one.
  def self.build(name)
    new(name)
  end

  # Shout it.
  def shout(volume = 1, *rest, sep:, &block)
    name.upcase
  end

  private

  def secret; end
end

class Person
  def extra; end
end
";

    fn library() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", LIBRARY);
        harness.index();
        (harness, uri)
    }

    #[test]
    fn the_outline_nests_the_way_the_file_nests() {
        let (mut harness, uri) = library();
        let outline = harness.outline(&uri);

        let names: Vec<&str> = outline
            .as_array()
            .expect("nested symbols")
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["Person", "Person"],
            "reopening is two top-level symbols"
        );

        let members: Vec<&str> = outline[0]["children"]
            .as_array()
            .expect("children")
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        // `self.build` keeps its receiver, and `private` is a statement rather than a symbol.
        assert_eq!(
            members,
            vec!["MAX_AGE", "name", "self.build", "shout", "secret"]
        );

        let shout = &outline[0]["children"][3];
        assert_eq!(shout["kind"], 6, "SymbolKind::METHOD");
        assert_eq!(shout["detail"], "(volume = ..., *rest, sep:, &block)");
        assert_eq!(outline[0]["children"][4]["detail"], "private");
        assert_eq!(outline[0]["children"][1]["detail"], "attr_reader");

        // The selection range has to sit inside the range, or clients reject the symbol.
        assert_eq!(shout["selectionRange"]["start"]["line"], 14);
        assert_eq!(shout["range"]["start"]["line"], 14);
        assert_eq!(shout["range"]["end"]["line"], 16);
    }

    /// Every construct the outline spells differently from its bare name.
    ///
    /// `class << self` has no name of its own, a method can carry a receiver, and six kinds
    /// have a `detail` that is a keyword rather than a parameter list. Each was reachable only
    /// through a fixture nothing had written.
    const SHAPES: &str = "\
module Outer
  class Widget
    attr_writer :width
    attr_accessor :height
    attr_reader :depth

    LIMIT = 10
    CAP = LIMIT

    class << self
      def registry
      end
    end

    def self.build
    end

    def resize
    end
    alias grow resize
    alias_method :enlarge, :resize
  end
end

def Outer.configure
end
";

    #[test]
    fn the_outline_spells_each_construct_the_way_a_reader_would() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/shapes.rb", SHAPES);
        harness.index();
        harness.open(&uri, SHAPES);
        let outline = harness.outline(&uri);

        /// `name | kind | detail` for every symbol, depth first, indented by nesting.
        fn rows(symbols: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
            for symbol in symbols.as_array().into_iter().flatten() {
                out.push(format!(
                    "{:indent$}{} | {} | {}",
                    "",
                    symbol["name"].as_str().unwrap_or("?"),
                    symbol["kind"],
                    symbol["detail"].as_str().unwrap_or("-"),
                    indent = depth * 2,
                ));
                rows(&symbol["children"], depth + 1, out);
            }
        }

        let mut listed = Vec::new();
        rows(&outline, 0, &mut listed);
        assert_eq!(
            listed,
            vec![
                "Outer | 2 | -",
                "  Widget | 5 | -",
                // The three `attr_*` kinds are PROPERTY, and their detail is the keyword: there
                // is no parameter list to show and "width" alone says nothing.
                "    width | 7 | attr_writer",
                "    height | 7 | attr_accessor",
                "    depth | 7 | attr_reader",
                "    LIMIT | 14 | -",
                // `CAP = LIMIT` is a constant *alias*, not a second constant.
                "    CAP | 14 | alias",
                // `class << self` has no name of its own; rubydex calls it `<Widget>`.
                "    << Widget | 5 | -",
                "      registry | 6 | -",
                "    self.build | 6 | -",
                "    resize | 6 | -",
                "    grow | 6 | alias",
                "    enlarge | 6 | alias",
                // A method written on a constant receiver keeps it, which is the only thing
                // telling `def Outer.configure` apart from a top-level `def configure`.
                "Outer.configure | 6 | -",
            ],
        );
    }

    #[test]
    fn hover_shows_the_signature_and_the_comment_above_it() {
        let (mut harness, uri) = library();
        let hover = harness.hover_at(&uri, LIBRARY, "shout(volume");
        let markdown = hover["contents"]["value"].as_str().expect("markdown");

        assert!(
            markdown.contains("Person#shout(volume = ..., *rest, sep:, &block)"),
            "{markdown}"
        );
        assert!(markdown.contains("Shout it."), "{markdown}");
        assert_eq!(hover["contents"]["kind"], "markdown");
    }

    #[test]
    fn an_anonymous_rest_parameter_hovers_as_ruby_wrote_it() {
        // rubydex records an anonymous `*`, `**` or `&` under the sigil itself rather than
        // under an empty name, so prepending a second sigil in `render` would spell `**` as
        // `****`. The pure test in `render` pins the spelling; this pins the convention it is
        // written against, which is rubydex's to change.
        let mut harness = Harness::new();
        let source = "class Relay\n  def send_on(one, *, k:, **, &)\n  end\nend\n";
        let uri = harness.write("lib/relay.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "send_on")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Relay#send_on(one, *, k:, **, &)"),
            "{markdown}"
        );
    }

    #[test]
    fn every_kind_of_parameter_ruby_has_is_spelled_the_way_it_was_written() {
        // `render::parameter_list` has an arm per `Parameter` variant and two had never been
        // asked for — an optional keyword and a forwarding `...`. `def call(retries: 3)` is
        // ordinary Ruby, and its hover is the only place a reader learns the argument is
        // optional at all: `retries:` and `retries: ...` say different things.
        let mut harness = Harness::new();
        let source = "class Job\n  def call(one, two = 1, *rest, key:, opt: 2, **kw, &blk)\n                        end\n\n  def forward(...)\n  end\nend\n";
        let uri = harness.write("lib/job.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "call(one")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Job#call(one, two = ..., *rest, key:, opt: ..., **kw, &blk)"),
            "{markdown}"
        );

        let forwarding = harness.hover_at(&uri, source, "forward(")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(forwarding.contains("Job#forward(...)"), "{forwarding}");
    }

    #[test]
    fn a_singleton_method_hovers_as_ruby_spells_it() {
        // rubydex calls this `Person::<Person>#build()`. Showing that to a user would be
        // showing them the index's internals.
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "build(name)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person.build(name)"), "{markdown}");
    }

    #[test]
    fn hover_names_every_construct_the_way_ruby_writes_it() {
        // `hover::signature` has an arm per kind of declaration and only two of them — a class
        // and a public method — had ever been asked for. The rest were reachable, rendered, and
        // asserted nowhere: a module hovering as `class`, or a private method hovering without
        // its visibility, would have gone out under a green suite.
        let mut harness = Harness::new();
        let source = "\
# A place to keep things.
module Storage
  LIMIT = 10

  class << self
    # Wipe it.
    def reset
    end
  end

  # Stash a thing.
  private def stash(thing)
  end

  protected def peek
  end
end
";
        let uri = harness.write("app/storage.rb", source);
        harness.index();

        let markdown = |harness: &mut Harness, needle: &str| -> String {
            harness.hover_at(&uri, source, needle)["contents"]["value"]
                .as_str()
                .unwrap_or_else(|| panic!("no hover on {needle:?}"))
                .to_owned()
        };

        let module = markdown(&mut harness, "Storage\n");
        assert!(module.contains("module Storage"), "{module}");
        assert!(module.contains("A place to keep things."), "{module}");

        // rubydex spells this `Storage::<Storage>`, which is not what the file says.
        // On `self`, not on the keyword: a definition matches its *name* span, which for
        // `class << self` is the receiver, so hover does not fire over the `class` either.
        assert!(harness.hover_at(&uri, source, "class << self").is_null());
        let singleton = markdown(&mut harness, "self");
        assert!(singleton.contains("class << Storage"), "{singleton}");

        // The visibility prefix, which is the whole reason a reader hovers a method they did
        // not write: `stash` is callable from inside `Storage` and nowhere else.
        let private = markdown(&mut harness, "stash(thing)");
        assert!(private.contains("private "), "{private}");
        assert!(private.contains("Storage#stash(thing)"), "{private}");
        assert!(private.contains("Stash a thing."), "{private}");

        let protected = markdown(&mut harness, "peek\n");
        assert!(protected.contains("protected "), "{protected}");

        // A constant is neither a namespace nor a method, and has no signature to render.
        let constant = markdown(&mut harness, "LIMIT");
        assert!(constant.contains("Storage::LIMIT"), "{constant}");
    }

    const GALLERY: &str = "\
# Everything on a shelf.
module Shelf
  LIMIT = 10
  CAP = LIMIT

  # A thing on it.
  class Book < Object
    include Comparable

    @@printed = 0

    def initialize(title)
      @title = title
    end

    # What it is called.
    def title(upcase: false, &block)
    end

    alias name title

    def self.open(*paths)
    end

    class << self
      def shut
      end
    end

    private def hide
    end

    protected def peek
    end
  end
end

$shelf = nil
";

    /// Every construct in [`GALLERY`], in source order, with its card drawn under it.
    fn gallery_cards(harness: &mut Harness, uri: &DocUri) -> String {
        [
            "Shelf\n",
            "LIMIT = 10",
            "CAP",
            "Book < Object",
            "@@printed",
            "@title = title",
            "title(upcase:",
            "name title",
            "open(*paths)",
            "self\n",
            "shut",
            "hide",
            "peek",
            "$shelf",
        ]
        .into_iter()
        .map(|needle| {
            let found = harness.hover_at(uri, GALLERY, needle);
            let card = found["contents"]["value"].as_str().unwrap_or("null");
            let drawn: String = card
                .lines()
                .map(|line| {
                    if line.is_empty() {
                        "\n".to_owned()
                    } else {
                        format!("  {line}\n")
                    }
                })
                .collect();
            format!("{}\n{drawn}", needle.trim_end())
        })
        .collect()
    }

    #[test]
    fn every_hover_card_in_one_file_drawn_side_by_side() {
        // The first-ten treatment, for an answer that is not a list. `ANCESTRY` pins ten rows
        // because a ranking is composition rather than a feature; a hover card is the same kind
        // of object, and until this existed every construct was checked by a `contains`
        // somewhere and no two were ever read next to each other. Which is how the singleton
        // card came to be the only one on this page that drops its namespace — `class << Book`
        // above a `private Shelf::Book#hide` — through a test that covered the construct, on a
        // top-level module where the two spellings are the same string.
        //
        // Pinned whole, and pinned *together*: the failure this shape catches is one card
        // drifting away from the others, which every card asserted on its own is blind to.
        let mut harness = Harness::new();
        let uri = harness.write("app/shelf.rb", GALLERY);
        harness.index();

        assert_eq!(
            gallery_cards(&mut harness, &uri),
            "\
Shelf
  ```ruby
  module Shelf
  ```

  ---

  Everything on a shelf.
LIMIT = 10
  ```ruby
  Shelf::LIMIT
  ```
CAP
  ```ruby
  Shelf::CAP
  ```
Book < Object
  ```ruby
  class Shelf::Book
  ```

  ---

  A thing on it.
@@printed
  ```ruby
  Shelf::Book#@@printed
  ```
@title = title
  ```ruby
  Shelf::Book#@title
  ```
title(upcase:
  ```ruby
  Shelf::Book#title(upcase: ..., &block)
  ```

  ---

  What it is called.
name title
  ```ruby
  Shelf::Book#name
  ```
open(*paths)
  ```ruby
  Shelf::Book.open(*paths)
  ```
self
  ```ruby
  class << Shelf::Book
  ```
shut
  ```ruby
  Shelf::Book.shut
  ```
hide
  ```ruby
  private Shelf::Book#hide
  ```
peek
  ```ruby
  protected Shelf::Book#peek
  ```
$shelf
  ```ruby
  $shelf
  ```
"
        );
    }

    #[test]
    fn hover_on_a_reopened_class_says_there_is_more_of_it() {
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "Person\n  MAX_AGE")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("class Person"), "{markdown}");
        assert!(markdown.contains("Someone with a name."), "{markdown}");
        // The whole point: the rest of the class is in a place this hover cannot show.
        assert!(markdown.contains("Defined in 2 places"), "{markdown}");
    }

    /// An rbs root shaped the way Ruby's own is, with RDoc's markup in it.
    ///
    /// Synthetic rather than the vendored copy, deliberately: what the tests below pin is the
    /// *card*, and pinning a card against 800 files of upstream prose would break on every rbs
    /// release for a reason that has nothing to do with ya-lsp. Every shape that matters is
    /// here — the call-seq header, a `<code>` span, an indented example, a dead `rdoc-ref:`
    /// link — and each was copied from the real `String#upcase` comment.
    const CORE_RBS: &str = "\
class String
  # <!--
  #   rdoc-file=string.c
  #   - upcase(mapping = :ascii) -> new_string
  # -->
  # Returns a new string containing <code>self</code>'s upcased characters:
  #
  #     'hello'.upcase # => \"HELLO\"
  #
  # See [Case Mapping](rdoc-ref:case_mapping.rdoc).
  #
  def upcase: (?Symbol mapping) -> String
end
";

    /// A class with an overloaded constructor, which is how RBS spells a method that can be
    /// called more than one way — and, since only a constant receiver resolves exactly, the
    /// shape of overload a signature card can actually be asked for.
    const OVERLOAD_RBS: &str = "\
class Coordinate
  # A point, from a pair or from text.
  def initialize: (String text) -> void
                | (Integer x, Integer y) -> void
end
";

    const STDLIB_RBS: &str = "\
class OptionParser
  # <!--
  #   rdoc-file=optparse.rb
  #   - parse!(argv = default_argv) -> argv
  # -->
  # Parses <tt>argv</tt> in place and returns what is left of it.
  def parse!: (?Array[String] argv) -> Array[String]
end
";

    /// A workspace with the signatures above indexed, plus the project's own `lib/person.rb`.
    fn with_signatures(source: &str) -> (Harness, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::create_dir_all(signatures.join("stdlib/optparse/0")).unwrap();
        std::fs::write(signatures.join("core/string.rbs"), CORE_RBS).unwrap();
        std::fs::write(signatures.join("core/coordinate.rbs"), OVERLOAD_RBS).unwrap();
        std::fs::write(
            signatures.join("stdlib/optparse/0/optparse.rbs"),
            STDLIB_RBS,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("lib/person.rb", LIBRARY);
        let uri = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();
        (harness, uri)
    }

    /// Signatures with return types in them, which is what the return-type table is built
    /// from.
    ///
    /// Small, and deliberately real in shape. `upcase` returns a `String` so a chain composes;
    /// `length` returns an `Integer` so a link can change class; `join` is a generic whose head
    /// is the answer; `tap` is declared on `Kernel` and returns `self`, which is the case a
    /// table keyed by the receiver's own name would get wrong twice over — wrong owner, and
    /// then `Kernel` instead of the receiver.
    const TYPED_RBS: &str = "\
module Kernel
  def tap: () { (self) -> void } -> self
end

class Object
  include Kernel
end

class String
  def upcase: () -> String
  def length: () -> Integer
  def scan: (String pattern) -> Array[String]
  def sub: (String pattern) -> String
         | (Integer index) -> Integer
  def bytes: () -> Array[Integer]
           | () { (Integer byte) -> void } -> self
end

class Integer
  def succ: () -> Integer
  def digits: () -> Array[Integer]
end

class Float
  def round: (?half: Symbol) -> Integer
           | (Integer digits, ?half: Symbol) -> (Integer | Float)
end

class Array[E]
  def join: (?String separator) -> String
  def first: () -> E
          | (Integer count) -> Array[E]
end

module Enumerable
  def sort: () -> Array[untyped]
end
";

    /// A workspace whose only signatures are [`TYPED_RBS`], plus one file of the user's code.
    fn with_types(source: &str) -> (Harness, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();
        assert!(
            harness.has("String#upcase()"),
            "the signature root was not indexed"
        );
        (harness, uri)
    }

    /// Which class the cursor at `~` completes against, named by the methods offered.
    ///
    /// The class rather than the list, because the list is what every other completion test is
    /// about and the *type* is what these are: a chain that answers with `Integer`'s members
    /// where `String`'s were meant is a wrong answer whose rows are all individually plausible.
    fn class_at(harness: &mut Harness, uri: &DocUri, marked: &str) -> String {
        let offered = harness.declarations_at(uri, marked);
        let has = |name: &str| offered.iter().any(|label| label == name);
        // Each class is named by what it has *and by what it does not*. Absence is the load
        // bearing half: the name-based fallback offers every method in the graph, so a test
        // that only looked for `upcase` would call it `String` and pass while the whole tier
        // was broken.
        let only = |mine: &[&str], theirs: &[&str]| {
            mine.iter().all(|name| has(name)) && !theirs.iter().any(|name| has(name))
        };
        match () {
            () if only(&["upcase", "length", "scan"], &["succ", "join"]) => "String".to_owned(),
            () if only(&["succ", "digits"], &["upcase", "join"]) => "Integer".to_owned(),
            () if only(&["join", "first"], &["upcase", "succ"]) => "Array".to_owned(),
            () if offered.is_empty() => "(nothing)".to_owned(),
            () => "(everything, which is the name-based list)".to_owned(),
        }
    }

    #[test]
    fn a_chain_completes_against_what_the_signature_says_it_returns() {
        // A chain through signatures, end to end and through the wire. Without it every line
        // here answers `(unrecognised)` —
        // the name-based list, every method in the graph — before the return-type table
        // existed.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".upcase.~"), "String");
        // A chain of two, which is the property that makes it a chain rather than one step.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".upcase.upcase.~"),
            "String"
        );
        // A link that changes class, and then a link on the class it changed to.
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".length.~"), "Integer");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".length.succ.~"),
            "Integer"
        );
        // A generic: the head is the answer, and the element type is a question nothing here
        // asks.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").~"),
            "Array"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").join.~"),
            "String"
        );
    }

    #[test]
    fn self_in_a_signature_is_the_receiver_and_not_the_class_that_declared_it() {
        // `Kernel#tap` returns `self`. Resolving that where the signature is *written* answers
        // `Kernel` — three methods, none of them the receiver's. The lookup goes through
        // rubydex's ancestor walk and the `self` is resolved against what the call was made on.
        //
        // Written with a block, because rbs declares `tap` with a required one and Ruby raises
        // `LocalJumpError` without it. Which is the second thing this pins: an arm whose block
        // is required is not what a blockless call reaches.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".tap { |s| s }.~"),
            "String"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".length.tap { |n| n }.~"),
            "Integer"
        );
        let blockless = class_at(&mut harness, &uri, "\"hi\".tap.~");
        assert!(
            blockless.starts_with("(everything"),
            "a required block is not optional: {blockless}"
        );
    }

    #[test]
    fn a_local_assigned_a_chain_is_typed_by_it() {
        // `type_the_local` does not stop at literals and `.new`.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "shouted = \"hi\".upcase\nshouted.~\n"),
            "String"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "n = \"hi\".length\nn.succ.~\n"),
            "Integer"
        );
    }

    #[test]
    fn a_block_at_the_call_site_chooses_which_overload_answered() {
        // The example that is easy to get wrong. `String#bytes`
        // declares `() -> Array[Integer]` and `() { (Integer) -> void } -> self`. That is not a
        // union: which arm applies is decided by whether a block was written, which is syntax
        // the cursor has already read. Both answers are exact, and they are different.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".bytes.~"), "Array");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".bytes { |b| b }.~"),
            "String"
        );
        // `&:to_s` and a forwarded `&blk` pass a block too, so they reach the same arm.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".bytes(&:to_s).~"),
            "String"
        );
    }

    #[test]
    fn how_many_arguments_the_call_wrote_chooses_which_overload_answered() {
        // The block's argument, made for the arity. `Float#round` declares `(?half: ...) ->
        // Integer` beside `(Integer, ?half: ...) -> (Integer | Float)`. Read as one partition
        // those disagree and both go; read by arity the zero-argument side agrees with itself,
        // and `3.7.round.` is an `Integer`. Not a guess and not a tier.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "3.7.round.~"), "Integer");
        // Keywords are not positional arguments on either side of the question.
        assert_eq!(
            class_at(&mut harness, &uri, "3.7.round(half: :up).~"),
            "Integer"
        );
        // And the arm the digit count reaches is a union, which arity does not rescue.
        let digits = class_at(&mut harness, &uri, "3.7.round(1).~");
        assert!(digits.starts_with("(everything"), "{digits}");
        // `Array#first` is the other half, and the half `types.md` had wrong: `() -> E` is a
        // type variable and stays dropped, while `(Integer count) -> Array[E]` is a class and
        // answers now. Generic instantiation would have fixed the first and not the second.
        assert_eq!(class_at(&mut harness, &uri, "[1, 2].first(3).~"), "Array");
        let bare = class_at(&mut harness, &uri, "[1, 2].first.~");
        assert!(bare.starts_with("(everything"), "{bare}");
    }

    #[test]
    fn a_call_the_signature_cannot_accept_falls_back_rather_than_to_the_nearest_arm() {
        // The rule that keeps this a reading of the signature rather than a guess at it, and
        // the one place the split takes an answer away. `scan` has one arm and requires a
        // pattern, so `"hi".scan.` reaches no arm at all rather than answering `Array`. Over a
        // thousand methods in Ruby's own signatures are in that position; `types.md` has the
        // argument.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").~"),
            "Array"
        );
        let bare = class_at(&mut harness, &uri, "\"hi\".scan.~");
        assert!(bare.starts_with("(everything"), "{bare}");
        let too_many = class_at(&mut harness, &uri, "\"hi\".scan(\"a\", 1).~");
        assert!(too_many.starts_with("(everything"), "{too_many}");
        // A call whose arguments cannot be counted gets what every arm agrees on, which is the
        // answer it got before arity was read at all. That is what makes this a partition.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(*args).~"),
            "Array"
        );
    }

    #[test]
    fn a_chain_through_something_the_table_dropped_answers_nothing_exact() {
        // The `Unknown` path at the new tier, which is the half that a green suite hides: the
        // fallback silently absorbing a bug is how this fails without anyone noticing. Each of
        // these has to reach the name-based list rather than a class.
        let (mut harness, uri) = with_types("");
        // `Array#first` returns the type variable `E`. Dropped, so the chain stops here.
        let dropped = class_at(&mut harness, &uri, "\"hi\".scan(\"a\").first.~");
        assert!(dropped.starts_with("(everything"), "{dropped}");
        // `String#sub` declares two arms returning two classes. A union, dropped.
        let union = class_at(&mut harness, &uri, "\"hi\".sub(\"a\").~");
        assert!(union.starts_with("(everything"), "{union}");
        // A method no signature declares at all.
        let absent = class_at(&mut harness, &uri, "\"hi\".nonesuch.~");
        assert!(absent.starts_with("(everything"), "{absent}");
        // And a receiver that was never anything: one `Unknown` ends the chain.
        let nothing = class_at(&mut harness, &uri, "thing.upcase.~");
        assert!(nothing.starts_with("(everything"), "{nothing}");
    }

    #[test]
    fn an_instance_variable_completes_against_what_its_class_assigned_it() {
        // An instance variable, end to end, in the shape a Rails controller is written in: assigned once in
        // one method, read in every other. This is the most common receiver in an application
        // that ya-lsp answered `Unknown` for, and it needs no annotation from anybody.
        let (mut harness, uri) = with_types("");
        let controller = "\
class Report
  def initialize
    @title = \"quarterly\"
  end

  def render
    @title.~
  end
end
";
        assert_eq!(class_at(&mut harness, &uri, controller), "String");

        // Through a chain, which is the three derived rungs composing: the ivar is typed by an
        // assignment,
        // the assignment by a signature, and the cursor sits one link past both.
        let chained = "\
class Report
  def initialize
    @size = \"quarterly\".length
  end

  def render
    @size.~
  end
end
";
        assert_eq!(class_at(&mut harness, &uri, chained), "Integer");
    }

    #[test]
    fn an_instance_variable_from_another_self_does_not_leak_into_instance_methods() {
        // The `Unknown` path for an instance variable, and the one that would be a *wrong*
        // answer rather than
        // an absent one: `@seed` in `def self.build` belongs to the class object, and joining
        // it to the instance's `@seed` would offer `String`'s methods for something that has
        // never been a string.
        let (mut harness, uri) = with_types("");
        let split = "\
class Report
  def self.build
    @seed = \"x\"
  end

  def render
    @seed.~
  end
end
";
        let answered = class_at(&mut harness, &uri, split);
        assert!(answered.starts_with("(everything"), "{answered}");
    }

    /// One file holding every tier an answer can come from, so the cards can be read together.
    ///
    /// The trailing comments are the needles: a hover fixture points at the *start* of what it
    /// searches for, so each row needs a spelling of its own method name that appears once.
    const TIERS: &str = "\
class Report
  def initialize
    @title = \"quarterly\"
    @size = \"quarterly\".length
  end

  def a
    \"hi\".upcase # resolved
  end

  def b
    \"hi\".upcase.length # derived
  end

  def c
    \"hi\".upcase.length.digits # chained
  end

  def d
    @title.upcase # assigned
  end

  def e
    @size.succ # both
  end

  def f
    thing.upcase # guessed
  end

  def g
    person.shout # named
  end
end

class Person
  def shout
  end
end
";

    /// A `semanticTokens/full` answer read back into absolute positions and named kinds.
    ///
    /// The wire format is deltas from the previous token, which is unreadable and is exactly
    /// what has to be checked: an entry that is off by one does not misplace one colour, it
    /// misplaces every colour after it. Decoding it here is the only way an assertion can be
    /// about what the user sees.
    fn decoded(answer: &serde_json::Value) -> Vec<(u64, u64, u64, &'static str)> {
        let data = answer["data"].as_array().expect("token data");
        let mut rows = Vec::new();
        let (mut line, mut start) = (0, 0);
        for token in data.chunks(5) {
            let numbers: Vec<u64> = token
                .iter()
                .map(|n| n.as_u64().unwrap_or_default())
                .collect();
            line += numbers[0];
            start = if numbers[0] == 0 {
                start + numbers[1]
            } else {
                numbers[1]
            };
            rows.push((
                line,
                start,
                numbers[2],
                *tokens::LEGEND
                    .get(numbers[3] as usize)
                    .expect("a type in the legend"),
            ));
        }
        rows
    }

    #[test]
    fn semantic_tokens_arrive_as_deltas_from_the_token_before() {
        let mut harness = Harness::new();
        let source = "def render(scale)\n  size = scale\n  size\nend\n";
        let uri = harness.write("app/big.rb", source);
        harness.index();
        harness.open(&uri, source);

        let answer = harness.ask(
            "textDocument/semanticTokens/full",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );

        assert_eq!(
            decoded(&answer),
            vec![
                (0, 4, 6, "method"),
                (0, 11, 5, "parameter"),
                (1, 2, 4, "variable"),
                (1, 9, 5, "variable"),
                (2, 2, 4, "variable"),
            ]
        );
    }

    #[test]
    fn a_token_length_is_counted_in_the_encoding_the_client_negotiated() {
        // `имя` is a legal Ruby local and three characters of two bytes each. A length taken as
        // `end - start` in bytes underlines six units where the client counts three, which
        // paints the colour over whatever follows. The offsets go through `TextDocument` for
        // exactly this reason, and a fixture that is all ASCII cannot see it.
        let mut harness = Harness::new();
        let source = "имя = 1\nимя\n";
        let uri = harness.write("app/utf.rb", source);
        harness.index();
        harness.open(&uri, source);

        let answer = harness.ask(
            "textDocument/semanticTokens/full",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );

        assert_eq!(
            decoded(&answer),
            vec![(0, 0, 3, "variable"), (1, 0, 3, "variable")],
            "three UTF-16 code units, not six bytes"
        );
    }

    #[test]
    fn a_call_with_no_receiver_at_all_has_no_type_to_derive() {
        // `resolve_typed` runs on every call the graph could not resolve, and most of those
        // have no receiver written: a bare `render` is a call on an implicit `self`, whose type
        // is a question about the enclosing class rather than about the text. There is nothing
        // for the new rung to do, so the answer is the name-based one it always was.
        let mut harness = Harness::new();
        harness.write("app/view.rb", "class View\n  def render\n  end\nend\n");
        let source = "render\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let markdown = card(&mut harness, &caller, source, "render");
        assert!(markdown.contains("View#render"), "{markdown}");
        assert!(
            markdown.contains("Matched on the method name alone"),
            "and it is still a guess: {markdown}"
        );
    }

    #[test]
    fn the_three_tiers_of_answer_drawn_side_by_side() {
        // The three tiers side by side, in `GALLERY` shape. Every one of these
        // cards is individually plausible; what has to be legible is the *difference* between
        // them, and a card asserted on its own cannot show that. Read down the right-hand
        // column: nothing, a signature, a line, both, and a guess.
        //
        // The property under test is not the wording. It is that a reader can tell, without
        // leaving the card, which of the three things ya-lsp did to arrive at the answer.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write("app/report.rb", TIERS);
        harness.index();
        harness.index_gems();

        let drawn: String = [
            "upcase # resolved",
            "length # derived",
            "digits # chained",
            "upcase # assigned",
            "succ # both",
            "upcase # guessed",
            "shout # named",
        ]
        .into_iter()
        .map(|needle| {
            let found = harness.hover_at(&uri, TIERS, needle);
            let card = found["contents"]["value"].as_str().unwrap_or("null");
            let body: String = card
                .lines()
                .map(|line| {
                    if line.is_empty() {
                        "\n".to_owned()
                    } else {
                        format!("  {line}\n")
                    }
                })
                .collect();
            format!("{needle}\n{body}")
        })
        .collect();

        assert_eq!(
            drawn,
            "\
upcase # resolved
  ```ruby
  String#upcase
  ```
length # derived
  ```ruby
  String#length
  ```

  *Type derived through `String#upcase()` — from what those methods declare, not from this expression.*
digits # chained
  ```ruby
  Integer#digits
  ```

  *Type derived through `String#upcase()` \u{2192} `String#length()` — from what those methods declare, not from this expression.*
upcase # assigned
  ```ruby
  String#upcase
  ```

  *Type taken from the assignment on line 3, which may not be the one that ran.*
succ # both
  ```ruby
  Integer#succ
  ```

  *Type derived through `String#length()` — from what those methods declare, not from this expression.*

  *Type taken from the assignment on line 4, which may not be the one that ran.*
upcase # guessed
  ```ruby
  String#upcase
  ```

  *Matched on the method name alone — the receiver's type is unknown.*
shout # named
  ```ruby
  Person#shout
  ```

  *Type guessed from the name `person` alone — nothing in the code says so.*
"
        );
    }

    /// The fixture the coordinate bug was found in, laid out so the collision is exact.
    ///
    /// `Alpha.` and `Gamma.` are on consecutive lines, so the two constant references are
    /// **exactly `"Alpha.\n".len()` apart** — seven bytes. Insert seven bytes above them and a
    /// buffer offset that names `Alpha` names `Gamma` in the graph: not "roughly wrong", the
    /// other class's reference, byte for byte. That is what makes the two tests below a pair —
    /// one asserts the right answer, the other asserts the wrong one is what you get without
    /// the map.
    const SHIFTED: &str = "class Alpha\n  def self.alpha_only\n  end\nend\n\n\
                           class Gamma\n  def self.gamma_only\n  end\nend\n\n\
                           Alpha.\nGamma.\n";

    /// Seven bytes, which is the distance between the two references.
    const PAD: &str = "# pad!\n";

    /// A constant reference that really is one, and a declaration *below* where the pad goes.
    ///
    /// `SHIFTED` cannot serve the jump test: `Alpha.\nGamma.` is one chained call to Ruby, so
    /// its `Gamma` is a method name and resolves to nothing even with no deferral at all. And
    /// its only jumpable declaration is at offset 0, which `Rebase`'s strict bounds refuse
    /// whenever the edit is also at offset 0.
    const JUMPABLE: &str = "class Alpha\nend\n\nclass Gamma\nend\n\nGamma\n";

    /// Open `SHIFTED`, index it, then defer and insert `PAD` at the top without indexing.
    fn deferred_after_a_shift() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        // From here the graph is frozen: `didChange` records the edit and nothing indexes it,
        // which is the whole of the deferred design.
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 0),
                }),
                text: PAD.to_owned(),
            }],
        );
        (harness, uri)
    }

    /// The cursor just after the `.` of `Alpha.`, in the buffer's coordinates.
    fn after_alpha_dot() -> serde_json::Value {
        serde_json::json!({ "line": 11, "character": 6 })
    }

    /// That the request really was answered from the graph as it stood, and not by falling back.
    ///
    /// **The fallback property is what makes this necessary.** A refused map settles and asks again, so the
    /// *answer* is correct either way and asserting on it proves nothing about the translation.
    /// What only a genuinely deferred answer leaves behind is a graph that never saw the edit.
    fn assert_deferred(harness: &Harness, uri: &DocUri, indexed: &str) {
        assert_eq!(
            harness.analysis.indexed_text.get(uri).map(String::as_str),
            Some(indexed),
            "the request fell back and indexed the buffer, so the map was never exercised"
        );
    }

    /// The labels a completion response offered, and whether the receiver was resolved at all.
    ///
    /// The second half is what tells a real answer from the fall-through: `precise: false` is
    /// the name-based list, which matches **every method in the project** by name and is what
    /// completion degrades to when it cannot type the receiver.
    fn offered(answer: &serde_json::Value) -> (Vec<String>, bool) {
        let items = answer["items"].as_array().cloned().unwrap_or_default();
        let precise = items
            .first()
            .and_then(|item| item["data"]["precise"].as_bool())
            .unwrap_or(false);
        let labels = items
            .iter()
            .map(|item| item["label"].as_str().unwrap_or_default().to_owned())
            .collect();
        (labels, precise)
    }

    #[test]
    fn a_deferred_completion_is_answered_in_the_graph_s_coordinates_and_not_the_buffer_s() {
        let (mut harness, uri) = deferred_after_a_shift();

        let offered_answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": after_alpha_dot(),
            }),
        );
        let (labels, precise) = offered(&offered_answer);

        // The receiver the *buffer* has is `Alpha`, and nothing was indexed after the edit.
        assert!(
            precise,
            "the deferred answer fell through to the name-based list: {labels:?}"
        );
        assert_eq!(
            labels,
            vec!["alpha_only".to_owned()],
            "the deferred answer is not the receiver the buffer has"
        );
    }

    #[test]
    fn without_the_map_the_same_deferred_completion_answers_the_wrong_class() {
        // Delete the mechanism and watch it break. `rebase_for` falls back to the identity when
        // it has no record of what the document was indexed as, so clearing the record is
        // exactly "defer the index and keep using buffer offsets as graph keys" — which is the
        // configuration this design exists to avoid, and it is `Alpha.new.` offering `Gamma`'s
        // members. Reproduced here rather than described.
        let (mut harness, uri) = deferred_after_a_shift();
        harness.analysis.indexed_text.clear();

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": after_alpha_dot(),
            }),
        );
        let (labels, precise) = offered(&answer);

        // What the buffer offset names in the older text is `Gamma`'s reference, and the call's
        // own method reference is narrower than it — so `locate` keeps the call, `constant_at`
        // finds no constant at all, and the receiver types as nothing. The degradation is
        // therefore the **name-based list** rather than a confident answer about `Gamma`, and
        // that list matches every method in the project: it contains `gamma_only`, which the
        // correct answer above does not offer at all.
        assert!(
            !precise,
            "without the map the receiver resolved, which this test cannot then tell apart"
        );
        assert!(
            labels.iter().any(|label| label == "gamma_only"),
            "the bug the map exists to remove did not reproduce: {labels:?}"
        );
    }

    #[test]
    fn a_receiver_inside_what_was_just_typed_is_refused_rather_than_guessed_at() {
        // The other half of the map's contract. Here the *receiver itself* is being typed, so
        // no graph offset names it at all — the map refuses, the request falls back to a
        // settle and asks again, and `Alph` is a constant nothing declares either way. What must not
        // happen, on either side of that fallback, is `Alpha`'s members: they are what sits at
        // this offset in the older text, and they are the wrong answer twice over.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        // Rewrite the `Alpha.` line into `Alph.` — the constant under the cursor is now text
        // the graph has never held.
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(10, 0),
                    end: lsp_types::Position::new(10, 6),
                }),
                text: "Alph.".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 10, "character": 5 },
            }),
        );
        let (labels, precise) = offered(&answer);

        assert!(
            !precise,
            "a receiver the graph has never held was typed anyway: {labels:?}"
        );
    }

    /// Two methods with a scope boundary between them, and a `self.` that can tell the class
    /// side from the instance side by which name comes back.
    const TWO_SCOPES: &str = "class Alpha\n  def self.klass_only\n  end\n\n                                def inst_only\n  end\n\n  def first\n    x = 1\n  end\n\n                                def second\n    y = 2\n  end\nend\n";

    #[test]
    fn an_edit_that_swallows_a_scope_boundary_does_not_answer_from_the_wrong_scope() {
        // **The case `changed_in_graph` cannot serve and the fallback cannot catch.** The edit
        // runs from inside `first` to inside `second`, so the region the graph disagrees with
        // spans an `end` and a `def`. The narrowest graph scope containing all of it is the
        // *class body*, where `self` is the class object — so `self.` would offer the singleton
        // while the caret is plainly inside an instance method. It is a wrong answer rather
        // than an empty one, so retrying never happens.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", TWO_SCOPES);
        harness.index();
        harness.open(&uri, TWO_SCOPES);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(8, 4),
                    end: lsp_types::Position::new(12, 9),
                }),
                text: "self.".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 8, "character": 9 },
            }),
        );
        let (labels, _precise) = offered(&answer);

        assert!(
            labels.iter().any(|label| label == "inst_only"),
            "the caret is inside an instance method and was offered {labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label == "klass_only"),
            "answered from the class body's scope: {labels:?}"
        );
    }

    #[test]
    fn a_member_typed_after_a_settled_receiver_is_answered_without_indexing_it() {
        // **The path the whole design is for**, and the one the other two tests do not reach.
        // Here the *caret* is inside text the graph has never been given while the receiver is
        // not, so `to_graph` refuses the cursor, the scope question is asked over the changed
        // region instead, and `Receiver::Constant` still maps because it sits in the common
        // prefix. This is what every keystroke of a member does.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(10, 6),
                    end: lsp_types::Position::new(10, 6),
                }),
                text: "al".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 10, "character": 8 },
            }),
        );
        let (labels, precise) = offered(&answer);

        assert!(
            precise && labels.iter().any(|label| label == "alpha_only"),
            "a member typed on a settled receiver answered {labels:?}"
        );
        // And it was answered *deferred*: the fallback would have indexed the buffer, so the
        // text the indexer was last handed still being the file on disk is what says the graph
        // was never touched. Without this the assertion above passes either way.
        assert_eq!(
            harness.analysis.indexed_text.get(&uri).map(String::as_str),
            Some(SHIFTED),
            "the deferred path indexed the buffer after all"
        );
    }

    #[test]
    fn a_deferred_hover_cards_the_constant_under_the_caret_and_not_the_one_below_it() {
        // `PAD` is the distance between the two references on purpose, so an offset handed to
        // the graph unmapped lands exactly on the *other* class — a card that is precise,
        // confident and about the wrong constant. The range has to come back through the map
        // too, or the highlight sits a line above the word.
        let (mut harness, uri) = deferred_after_a_shift();

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 11, "character": 2 },
            }),
        );

        let card = answer["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            card.contains("Alpha") && !card.contains("Gamma"),
            "the caret is on Alpha and the card said: {card}"
        );
        assert_eq!(
            answer["range"]["start"]["line"], 11,
            "the span came back in the graph's coordinates rather than the buffer's"
        );
        assert_deferred(&harness, &uri, SHIFTED);
    }

    #[test]
    fn a_deferred_jump_lands_where_the_declaration_is_now_and_not_where_it_was() {
        // The inverse map's own test, and both halves of the map are in it: the caret is below
        // the edit and so is the class it names. `class Gamma` is on line 3 of the text the
        // graph holds and line 4 of the buffer, so a jump answered in the graph's coordinates
        // lands a line above the class — the right file, the wrong line, and nothing about the
        // answer says so.
        //
        // **The pad goes in the middle, and that is `Rebase`'s strict bounds paying a cost
        // rather than a quirk of the fixture.** A declaration starting at graph offset 0, with
        // an insertion also at offset 0, is honestly either 0 or 7 in the buffer — so
        // `to_buffer` refuses and the jump falls back to a settle. It still answers correctly,
        // because a refused map settles and re-asks, which is exactly why `assert_deferred` has
        // to be here.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", JUMPABLE);
        harness.index();
        harness.open(&uri, JUMPABLE);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(2, 0),
                    end: lsp_types::Position::new(2, 0),
                }),
                text: PAD.to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 7, "character": 2 },
            }),
        );

        assert_eq!(
            answer[0]["targetSelectionRange"]["start"]["line"],
            serde_json::json!(4),
            "the jump answered {answer} instead of `class Gamma` on line 4"
        );
        assert_eq!(
            answer[0]["originSelectionRange"]["start"]["line"],
            serde_json::json!(7),
            "the origin came back in the graph's coordinates rather than the buffer's"
        );
        assert_deferred(&harness, &uri, JUMPABLE);
    }

    #[test]
    fn an_index_that_crashed_does_not_leave_the_map_claiming_the_graph_caught_up() {
        // **The bulkhead meeting the map, and the failure is silent in both directions.** A
        // contained panic costs the document its update: the graph keeps the version it already had. If
        // `indexed_text` recorded the text that *failed* to go in, the map would compare two
        // equal strings, answer `identity`, and hand a buffer offset to a graph some unknown
        // number of edits behind — with no refusal, so nothing falls back.
        //
        // What the caret then lands on is text seven bytes along, and on *this* fixture that is
        // the `Gamma` of `Alpha.\nGamma.` — one chained call to Ruby, so a method name that
        // resolves to nothing, and the symptom is silence rather than a wrong class. It is the
        // same defect either way: the offset was handed to a graph that never received the
        // edit. The assertion is on the answer being right, which covers both.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 0),
                }),
                text: PAD.to_owned(),
            }],
        );
        // Armed rather than written into the text, because the sentinel is 33 bytes and this
        // fixture's whole point is a *seven*-byte shift: a sentinel in the buffer destroys the
        // common suffix, the map then refuses everything, and the test would pass on a
        // refusal instead of on the map being right.
        indexer::SOURCE_INDEXES_TO_CRASH.with(|counter| counter.set(1));
        // Something that is not deferred forces the index, which panics and is contained.
        harness.analysis.settle();

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 11, "character": 2 },
            }),
        );

        let card = answer["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            card.contains("Alpha") && !card.contains("Gamma"),
            "the map trusted an index that never happened and said: {card}"
        );
        assert_deferred(&harness, &uri, SHIFTED);
    }

    #[test]
    fn a_refused_receiver_is_answered_by_indexing_rather_than_by_answering_nothing() {
        // **The map is an optimization and not a filter**, and this is the test that says so.
        // Rewriting `Alpha.` into `Gamma.` puts a constant the graph knows perfectly well at an
        // offset the graph has never seen — the refusal is about the *offset*, not the name —
        // so the deferred attempt has nothing to say. Answering nothing there would trade a
        // correct answer for a fast empty list, so the request settles and asks again.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(10, 0),
                    end: lsp_types::Position::new(10, 6),
                }),
                text: "Gamma.".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 10, "character": 6 },
            }),
        );
        let (labels, precise) = offered(&answer);

        assert!(
            precise && labels.iter().any(|label| label == "gamma_only"),
            "a refused deferral answered {labels:?} instead of indexing and answering Gamma's"
        );
    }

    #[test]
    fn a_completion_row_says_which_tier_its_list_came_from() {
        // The tiers reaching a completion row. A row's card is the same card a hover draws,
        // and a `completionItem/resolve` that built it with `precise: true` unconditionally
        // would present every row off the name-based list — every method in the project matched
        // on its name — as certain.
        //
        // The tier is a property of the *list* rather than of the row: every row was offered
        // for the same receiver. It travels on `data`, because by the time the client asks to
        // resolve one, that is all either side still knows about where the list came from.
        let (mut harness, uri) = with_types("");

        let derived = harness.complete(&uri, "\"hi\".upcase.len~");
        let guessed = harness.complete(&uri, "thing.len~");

        let resolve = |harness: &mut Harness, list: &serde_json::Value| {
            let item = list["items"]
                .as_array()
                .and_then(|items| items.first())
                .cloned()
                .expect("a row");
            harness.ask("completionItem/resolve", serde_json::json!(item))["documentation"]["value"]
                .as_str()
                .unwrap_or("(nothing)")
                .to_owned()
        };

        let derived = resolve(&mut harness, &derived);
        assert!(
            derived.contains("String#length"),
            "a derived row still gets its card: {derived}"
        );
        assert!(
            !derived.contains("Matched on the method name"),
            "and must not be presented as a guess: {derived}"
        );

        let guessed = resolve(&mut harness, &guessed);
        assert!(
            guessed.contains("Matched on the method name alone"),
            "a name-matched row has to say so: {guessed}"
        );
    }

    fn card(harness: &mut Harness, uri: &DocUri, source: &str, needle: &str) -> String {
        harness.hover_at(uri, source, needle)["contents"]["value"]
            .as_str()
            .unwrap_or_else(|| panic!("no hover on {needle:?}"))
            .to_owned()
    }

    #[test]
    fn a_core_method_hovers_as_rdoc_written_in_markdown() {
        // The whole card, not a `contains`. A hover card is a composition, so asserting its
        // parts one `contains` at a time is how the two shapes of one answer — a guessed single
        // match and a guessed list — drift apart.
        //
        // What this pins on the way past: `<code>self</code>` reaching the user as markdown
        // rather than as a span a client silently eats, `[Case Mapping](rdoc-ref:…)` losing a
        // link that goes nowhere while keeping its words, the call-seq lifted out of RDoc's HTML
        // header as Ruby, and the indented example surviving untouched.
        //
        // **No footnote at all.** `greeting` is a local assigned a string literal, which
        // completion types exactly and which hover would otherwise match on the name. Both go
        // through `types::method_receiver`, so a card that would carry "matched on the method
        // name alone" carries nothing instead — there is nothing to doubt, and it is not a
        // *derived* answer either: a literal assigned one line up is code the reader can see.
        let source = "greeting = \"hello\"\ngreeting.upcase\n";
        let (mut harness, uri) = with_signatures(source);
        assert_eq!(
            card(&mut harness, &uri, source, "upcase"),
            "```ruby\n\
             String#upcase(mapping = ...)\n\
             ```\n\
             \n\
             ---\n\
             \n\
             ```ruby\n\
             upcase(mapping = :ascii) -> new_string\n\
             ```\n\
             \n\
             Returns a new string containing `self`'s upcased characters:\n\
             \n\
             \u{20}   'hello'.upcase # => \"HELLO\"\n\
             \n\
             See Case Mapping."
        );
    }

    #[test]
    fn a_stdlib_method_hovers_the_same_way_a_core_one_does() {
        // Different directory under the rbs root, same card. `<tt>` is RDoc's other spelling of
        // `<code>` and appears 22 times in the vendored signatures; it must not be the one that
        // still leaks. The footnote went the same way it did above, and for the same reason:
        // `parser = OptionParser.new` is a receiver the code names.
        let source = "parser = OptionParser.new\nparser.parse!\n";
        let (mut harness, uri) = with_signatures(source);
        assert_eq!(
            card(&mut harness, &uri, source, "parse!\n"),
            "```ruby\n\
             OptionParser#parse!(argv = ...)\n\
             ```\n\
             \n\
             ---\n\
             \n\
             ```ruby\n\
             parse!(argv = default_argv) -> argv\n\
             ```\n\
             \n\
             Parses `argv` in place and returns what is left of it."
        );
    }

    #[test]
    fn every_shape_of_card_puts_what_it_knows_in_the_same_place() {
        // The four cards side by side, which is the only way the convention is visible: answer
        // first, then one italic line per thing ya-lsp knows *about* the answer. A precise hit
        // says nothing extra; a reopened class says where else it lives; a guess says it is a
        // guess; and a guess with more than one candidate says the same sentence in the same
        // place, rather than in bold at the top after an em dash.
        let source =
            "class Radio\n  def shout; end\nend\n\nPerson.build(\"x\")\nthing.shout\nthing.extra\n";
        let (mut harness, uri) = with_signatures(source);

        // Precise: a constant receiver is the one thing rubydex can name without inference.
        assert_eq!(
            card(&mut harness, &uri, source, "build("),
            "```ruby\nPerson.build(name)\n```\n\n---\n\nBuild one."
        );

        // Reopened, and the one thing a hover cannot show is the half that is elsewhere.
        assert_eq!(
            card(&mut harness, &uri, source, "Person.build"),
            "```ruby\nclass Person\n```\n\n---\n\nSomeone with a name.\n\nReopened \
             below.\n\n*Defined in 2 places.*"
        );

        // One name-based match: a whole card, and the caveat under it.
        assert_eq!(
            card(&mut harness, &uri, source, "extra"),
            "```ruby\nPerson#extra\n```\n\n*Matched on the method name alone — the \
             receiver's type is unknown.*"
        );

        // Several: a list, and the same caveat in the same place. Naming one of them would be
        // presenting a coin flip as an answer.
        assert_eq!(
            card(&mut harness, &uri, source, "shout\n"),
            "**2 possible definitions**\n\n- `Person#shout`\n- `Radio#shout`\n\n*Matched on \
             the method name alone — the receiver's type is unknown.*"
        );
    }

    #[test]
    fn goto_definition_follows_a_call_with_a_known_receiver() {
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "Person.build(\"x\")\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "build");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(library.as_str()));
        // Not the whole method body: the name, which is where the cursor should land.
        assert_eq!(targets[0]["targetSelectionRange"]["start"]["line"], 9);
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
    }

    #[test]
    fn goto_definition_on_a_constant_ignores_the_synthetic_singleton_reference() {
        // Regression: rubydex records a *second*, invented constant reference over the same
        // bytes as `Person` in `Person.build`, pointing at the singleton class, so that the
        // call can be resolved. Following it would jump to the wrong thing — or, for a call
        // with an implicit receiver, to a `class << self` block on the other side of the file.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "Person.build(\"x\")\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "Person");
        assert_eq!(
            targets.as_array().unwrap().len(),
            2,
            "both `class Person`: {targets}"
        );
        for target in targets.as_array().unwrap() {
            assert_eq!(target["targetUri"], serde_json::json!(library.as_str()));
            assert_eq!(target["targetSelectionRange"]["start"]["character"], 6);
        }
    }

    /// Three shapes of `new`: an ordinary constructor, a class that writes its own `self.new`,
    /// and a class with no constructor at all.
    const CONSTRUCTORS: &str = "\
class Money
  def initialize(cents)
    @cents = cents
  end
end

class Registry
  def self.new(*args)
    super
  end

  def initialize; end
end

class Plain
end
";

    #[test]
    fn new_navigates_to_the_constructor_rather_than_to_class_new() {
        // `Foo.new` really is `Class#new`, so the exact answer is a signature file nobody asked
        // to read. Both goto-definition and hover redirect to the constructor, and hover is the
        // half that matters most: it is where the parameter list comes from.
        let mut harness = Harness::new();
        let library = harness.write("lib/shop.rb", CONSTRUCTORS);
        let source = "Money.new(1)\nRegistry.new\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "new(1)");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(library.as_str()));
        assert_eq!(
            targets[0]["targetSelectionRange"]["start"]["line"], 1,
            "the `initialize` on line 2, not the class: {targets}"
        );

        let markdown = harness.hover_at(&caller, source, "new(1)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Money#initialize(cents)"), "{markdown}");
        assert!(
            !markdown.contains("receiver's type is unknown"),
            "a redirect is still exact: {markdown}"
        );

        // A class that writes its own `new` is reached by that method, and `initialize` is one
        // `super` further on. Redirecting here would skip the code the call actually runs.
        let markdown = harness.hover_at(&caller, source, "new\n")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Registry.new(*args)"), "{markdown}");
    }

    #[test]
    fn find_all_references_on_new_lists_call_sites_and_not_the_constructor() {
        // The redirect is a navigation affordance, and `references` is the one caller that must
        // not take it: `def initialize` is not a declaration of `new`, and a work list of
        // `.new` call sites with the constructor in it is noise. Before the flag it appeared
        // for every class whose constructor is in the user's own code.
        let mut harness = Harness::new();
        harness.write("lib/shop.rb", CONSTRUCTORS);
        let source = "Money.new(1)\nMoney.new(2)\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.reference_list(&caller, source, "new(1)", true),
            vec!["main.rb:0:6", "main.rb:1:6"]
        );
    }

    #[test]
    fn a_constructors_keyword_arguments_complete_at_the_call() {
        // The redirect goes through `locator::resolve`, which is also where `Context::Argument`
        // gets the method whose parameters it offers. Before it, `Foo.new(` completed against
        // `Class#new`'s `(*untyped, **untyped)` — a signature with no keywords in it at all.
        let mut harness = Harness::new();
        harness.write(
            "lib/order.rb",
            "class Order\n  def initialize(total:, currency: \"USD\")\n  end\nend\n",
        );
        let caller = harness.write("lib/main.rb", "");
        harness.index();

        let offered = harness.declarations_at(&caller, "Order.new(~)\n");

        assert!(offered.contains(&"total:".to_owned()), "{offered:?}");
        assert!(offered.contains(&"currency:".to_owned()), "{offered:?}");
    }

    #[test]
    fn a_class_with_no_constructor_keeps_the_honest_answer() {
        // Every object inherits `BasicObject#initialize`, so with rbs indexed there is always
        // *an* `initialize` to redirect to — and for a class that defines none it is as useless
        // as `Class#new` and less true. The guard is what keeps the redirect meaning something.
        let dir = tempfile::tempdir().expect("tempdir");
        let core = dir.path().join("sig/core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(
            core.join("core.rbs"),
            "\
class BasicObject
  def initialize: () -> void
end

class Class
  def new: (*untyped) -> untyped
end
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                dir.path().join("sig").display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("lib/shop.rb", CONSTRUCTORS);
        let source = "Plain.new\nMoney.new(1)\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();

        assert!(
            harness.has("BasicObject#initialize()"),
            "the signature root was not indexed"
        );

        let markdown = harness.hover_at(&caller, source, "new\n")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Class#new"),
            "an inherited empty constructor is not a constructor to redirect to: {markdown}"
        );

        // And the guard has not simply turned the redirect off for everyone.
        let markdown = harness.hover_at(&caller, source, "new(1)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Money#initialize(cents)"), "{markdown}");
    }

    #[test]
    fn a_call_on_a_local_is_answered_as_a_guess() {
        // Without type inference `thing.shout` can only be matched by name. The answer is
        // still worth giving — it is right most of the time — but it must not be dressed up
        // as certainty.
        let mut harness = Harness::new();
        harness.write("lib/person.rb", LIBRARY);
        let source = "thing = something\nthing.shout\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "shout");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");

        let markdown = harness.hover_at(&caller, source, "shout")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person#shout"), "{markdown}");
        assert!(
            markdown.contains("receiver's type is unknown"),
            "{markdown}"
        );
    }

    #[test]
    fn several_classes_with_the_same_method_are_listed_rather_than_guessed_between() {
        let mut harness = Harness::new();
        harness.write("lib/a.rb", "class Alpha\n  def ping; end\nend\n");
        harness.write("lib/b.rb", "class Beta\n  def ping; end\nend\n");
        let source = "thing.ping\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let markdown = harness.hover_at(&caller, source, "ping")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("2 possible definitions"), "{markdown}");
        assert!(markdown.contains("Alpha#ping"), "{markdown}");
        assert!(markdown.contains("Beta#ping"), "{markdown}");
    }

    #[test]
    fn a_require_path_navigates_to_the_file_it_names() {
        // The graph indexes the call to `require` but never its argument, so this is the one
        // navigation answer that comes from parsing rather than from the index.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "require \"person\"\nrequire_relative \"person\"\nrequire \"nope\"\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        for needle in ["person\"\nrequire_relative", "person\"\nrequire \"nope"] {
            let targets = harness.definition_at(&caller, source, needle);
            assert_eq!(
                targets[0]["targetUri"],
                serde_json::json!(library.as_str()),
                "{needle}: {targets}"
            );
            assert_eq!(targets[0]["targetRange"]["start"]["line"], 0);
        }

        assert_eq!(
            harness.definition_at(&caller, source, "nope"),
            serde_json::Value::Null,
            "a require of a file we do not have is not an error"
        );
    }

    #[test]
    fn navigation_sees_an_incremental_edit_immediately() {
        // The buffer the editor is typing into shadows the file on disk, and every answer has
        // to come from the buffer — including one assembled from range edits.
        let mut harness = Harness::new();
        let source = "class Person\n  def shout; end\nend\n";
        let uri = harness.write("lib/person.rb", source);
        harness.index();
        harness.open(&uri, source);

        harness.edit(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 6,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 11,
                    },
                }),
                text: "whisper".to_owned(),
            }],
        );

        let edited = "class Person\n  def whisper; end\nend\n";
        let markdown = harness.hover_at(&uri, edited, "whisper")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person#whisper"), "{markdown}");

        // And the old name is gone from the index rather than merely shadowed by it.
        let outline = harness.outline(&uri);
        assert_eq!(outline[0]["children"][0]["name"], "whisper", "{outline}");
        assert_eq!(
            outline[0]["children"].as_array().unwrap().len(),
            1,
            "{outline}"
        );
    }

    #[test]
    fn a_position_with_nothing_under_it_answers_null_rather_than_erroring() {
        // An error response is something editors show the user. "No definition here" is not
        // an error, it is an answer.
        let (mut harness, uri) = library();
        for method in [
            "textDocument/hover",
            "textDocument/definition",
            "textDocument/documentSymbol",
        ] {
            let answer = harness.ask(
                method,
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": 4, "character": 0 },
                }),
            );
            if method == "textDocument/documentSymbol" {
                continue;
            }
            assert_eq!(answer, serde_json::Value::Null, "{method}");
        }

        // Malformed params are client input, and must not take the thread down.
        assert_eq!(
            harness.ask(
                "textDocument/hover",
                serde_json::json!({ "nonsense": true })
            ),
            serde_json::Value::Null
        );
    }

    #[test]
    fn navigation_ranges_are_in_the_negotiated_encoding() {
        // The failure this guards against is invisible on ASCII and silent everywhere else: a
        // range built from byte offsets sends the editor to the wrong column on every line
        // that contains an emoji, an accent, or CJK text.
        let source = "x = \"\u{1f600}\u{1f600}\"; class Person; end\n";
        for (encoding, expected) in [
            (PositionEncoding::Utf8, 22),  // two 4-byte emoji
            (PositionEncoding::Utf16, 18), // two surrogate pairs
            (PositionEncoding::Utf32, 16), // two characters
        ] {
            let mut harness = Harness::with_encoding(encoding);
            let uri = harness.write("lib/person.rb", source);
            harness.index();

            let outline = harness.outline(&uri);
            assert_eq!(
                outline[0]["selectionRange"]["start"]["character"], expected,
                "{encoding:?}: {outline}"
            );

            // And the request side agrees: a position expressed in the same units has to come
            // back to the same byte offset, or hover would land one construct off.
            let hover = harness.ask(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": 0, "character": expected },
                }),
            );
            assert!(
                hover["contents"]["value"]
                    .as_str()
                    .is_some_and(|markdown| markdown.contains("class Person")),
                "{encoding:?}: {hover}"
            );
        }
    }
    #[test]
    fn the_outline_lists_definitions_and_not_the_statements_around_them() {
        // Each of these is something rubydex records as a definition for resolution's sake and
        // nobody would want in a file's structure: `private :shout` is a visibility statement,
        // not a second declaration of `shout`, and a variable would appear once per assignment.
        let mut harness = Harness::new();
        let source = "\
$LOG = nil
alias $log $LOG

class Widget
  @@count = 0
  @name = nil

  def shout
    @volume = 1
  end
  private :shout

  SECRET = 1
  private_constant :SECRET
end
";
        let uri = harness.write("lib/widget.rb", source);
        harness.index();
        harness.open(&uri, source);

        let listed: Vec<String> = all_symbols(&harness.outline(&uri))
            .into_iter()
            .map(|symbol| symbol["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        for statement in ["$LOG", "$log", "@@count", "@name", "@volume"] {
            assert!(
                !listed.contains(&statement.to_owned()),
                "{statement} is not an outline entry: {listed:?}"
            );
        }
        // Not vacuous: the definitions those statements are about are all still there.
        for wanted in ["Widget", "shout", "SECRET"] {
            assert!(listed.contains(&wanted.to_owned()), "{listed:?}");
        }
    }

    #[test]
    fn the_symbol_picker_answers_nothing_when_asked_for_nothing() {
        // `MAX_WORKSPACE_SYMBOLS` is a latency control — a subsequence match on one character
        // hits most of a bundle on every keystroke — so a zero limit has to stop before the
        // search runs at all rather than after it.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        harness.index();

        let own = harness.analysis.own_documents();
        assert!(
            search::search(
                &harness.analysis.graph,
                &harness.analysis.synthesized,
                "Person",
                1,
                &own
            )
            .len()
                == 1,
            "the fixture has to match at all for a zero limit to mean anything"
        );
        assert!(
            search::search(
                &harness.analysis.graph,
                &harness.analysis.synthesized,
                "Person",
                0,
                &own
            )
            .is_empty()
        );
    }

    #[test]
    fn a_constant_that_is_not_a_namespace_offers_nothing_after_its_colons() {
        // `MAX::` is legal to type and means nothing: an `Integer` has no members to write
        // there. rubydex resolves the receiver to a declaration all the same, and every step
        // that follows — the ancestor walk, the member list — has to answer "not a namespace"
        // rather than assume the id it was handed names one.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "MAX = 10\n");
        harness.index();

        let offered = harness.suggestions(&uri, "MAX = 10\nMAX::~\n");
        assert!(offered.is_empty(), "{offered:?}");
    }

    #[test]
    fn a_call_on_a_constant_that_was_never_declared_still_answers() {
        // `Nowhere` is a receiver rubydex can name and cannot resolve — the normal state of a
        // file mid-refactor, and of every constant a gem defines when the gem is not indexed.
        // The precise path has to stand down rather than resolve against a missing owner.
        let mut harness = Harness::new();
        let person = harness.write(
            "app/person.rb",
            "class Person\n  def frobnicate\n  end\nend\n",
        );
        let source = "Nowhere.frobnicate\n";
        let main = harness.write("app/main.rb", source);
        harness.index();

        // Name-based, so the one declaration spelled this way is still the answer — and *which*
        // declaration is the assertion. "not null" would pass just as happily on a jump to the
        // wrong file, which is the failure this degradation actually risks.
        let found = harness.definition_at(&main, source, "frobnicate");
        let targets = found.as_array().expect("link targets");
        assert_eq!(targets.len(), 1, "{found}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(person.as_str()));
        assert_eq!(
            targets[0]["targetSelectionRange"]["start"],
            serde_json::json!({ "line": 1, "character": 6 }),
            "the name in `def frobnicate`, not the class or the body: {found}"
        );
    }

    #[test]
    fn a_singleton_method_written_on_a_constant_is_scoped_to_that_class() {
        // `def Person.build` is the same method as `def self.build` written from outside the
        // class body, and rubydex records the receiver differently for each. Only the `self`
        // form had a test, so the constant form's `self` could have been anything at all —
        // and inside it `self` is `Person`, which is what decides whether the class's own
        // singleton methods are callable without a receiver.
        let mut harness = Harness::new();
        harness.write(
            "app/person.rb",
            "class Person\n  def self.find\n  end\n  def self.all\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/patch.rb", "");

        let offered = harness.suggestions(&uri, "def Person.build\n  fin~\nend\n");
        assert!(
            offered.contains(&"find".to_owned()),
            "`self` inside `def Person.build` is Person: {offered:?}"
        );
    }

    #[test]
    fn every_receiver_that_can_precede_a_double_colon_is_answered() {
        // `::` after something that is not a namespace is legal to type and means nothing, and
        // each shape reaches a different arm. Left unanswered they are not silence but a
        // *wrong* list — the fall-through would offer whatever the enclosing scope had.
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "module HR\n  MAX = 1\n  class Person\nend\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // `self::` is legal and rare: the nesting is what it means, so a module's own
        // constants are what it can be followed by.
        // The answer has to be the same one `HR::` gives, from inside the module and from
        // inside one of its methods alike: `::` asks about a namespace, and the namespace is
        // the same either way.
        let named = harness.suggestions(&uri, "HR::~\n");
        assert_eq!(named, vec!["MAX".to_owned(), "Person".to_owned()]);
        assert_eq!(
            // Something has to follow the line: `self::` with only an `end` after it is a
            // *method* call in Prism's recovery, with `::` read as the call operator.
            harness.suggestions(&uri, "module HR\n  self::~\n  X = 1\nend\n"),
            named,
            "in a module body, `self` is the module"
        );
        assert_eq!(
            harness.suggestions(&uri, "module HR\n  def y\n    self::P~\n  end\nend\n"),
            vec!["Person".to_owned()],
            "and inside a method it is still the module that `::` asks about"
        );

        // An instance is not a namespace, and neither is a literal. Both parse.
        for marked in ["\"foo\"::~\n", "HR::Person.new::~\n", "whatever::~\n"] {
            let offered = harness.suggestions(&uri, marked);
            assert!(offered.is_empty(), "{marked:?} offered {offered:?}");
        }
    }

    #[test]
    fn hover_reads_a_method_that_no_def_wrote() {
        // `attr_reader :name` declares a method whose definition is not a `Definition::Method`,
        // so the signature lookup finds nothing to read parameters or visibility from. It still
        // has to name the method rather than fall through to a bare string — and `attr_reader`
        // is how a large share of a Rails app's methods are declared.
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "name\n\n  # Build")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person#name"), "{markdown}");
        assert!(
            !markdown.contains('('),
            "no parameter list to render: {markdown}"
        );
    }

    #[test]
    fn an_outline_names_a_singleton_method_on_a_constant_it_cannot_resolve() {
        // `def Nowhere.thing` parses and is indexed; the receiver names a constant the graph
        // never saw. The outline still has to carry a row for it, and the honest name is the
        // bare method — inventing `Nowhere.thing` from an unresolved reference would put a
        // name in the picker that leads nowhere.
        let mut harness = Harness::new();
        let source = "def Nowhere.thing\nend\n";
        let uri = harness.write("app/patch.rb", source);
        harness.index();
        harness.open(&uri, source);

        let outline = harness.ask(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );
        let names: Vec<&str> = all_symbols(&outline)
            .into_iter()
            .filter_map(|symbol| symbol["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec!["Nowhere.thing"],
            "the receiver is named from the reference, declared or not: {outline}"
        );
    }

    #[test]
    fn a_namespace_the_resolver_invented_is_not_somewhere_to_jump() {
        // `Missing::Thing` makes the resolver record a `Missing` it never saw defined. It is a
        // placeholder with no definitions behind it, so listing it in the picker would offer a
        // destination that does not exist.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "Missing::Thing.new\n");
        harness.index();

        // Nor somewhere to hover. The cursor is on a real reference, so there is something to
        // resolve — but what it resolves to is a name the resolver wrote down and nothing else,
        // and a card saying `Missing` over a constant spelled `Missing` tells a reader only
        // that the server has no idea either. Silence says the same thing and takes no space.
        let source = "Missing::Thing.new\n";
        assert!(harness.hover_at(&uri, source, "Missing").is_null());

        let names: Vec<String> = harness
            .ask(
                "workspace/symbol",
                serde_json::json!({ "query": "Missing" }),
            )
            .as_array()
            .into_iter()
            .flatten()
            .map(|symbol| symbol["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn a_file_the_index_never_took_answers_nothing_rather_than_guessing() {
        // `index.include` is `**/*.rb`, so a Ruby-looking buffer under another extension has
        // text on disk and no document in the graph. Every request has to survive that: the
        // text is readable, so nothing short of the graph lookup can tell them apart.
        let mut harness = Harness::new();
        let source = "class Person\nend\n";
        let uri = harness.write("app/notes.txt", source);
        harness.index();

        assert!(harness.outline(&uri).is_null(), "{}", harness.outline(&uri));
        assert!(harness.hover_at(&uri, source, "Person").is_null());
        assert!(
            harness
                .ask(
                    "textDocument/definition",
                    serde_json::json!({
                        "textDocument": { "uri": uri.as_str() },
                        "position": position_of(source, "Person"),
                    }),
                )
                .is_null()
        );
    }

    #[test]
    fn a_recovered_span_that_does_not_contain_its_name_is_widened_to_fit() {
        // The other half of `a_half_typed_def_does_not_take_the_outline_down_with_it`. The
        // outline drops a nameless `def` before it ever builds a range; goto-definition does
        // not, because looking a name up per definition would cost every hover in the file.
        // So `locator::spans` is the one place the containment rule is enforced, and this is
        // the request that reaches it.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        // Asked of `spans` directly: containment is a property of every pair it hands out, and
        // the requests that carry one filter half-typed definitions out before they get there.
        let mut broken = 0;
        for source in [
            "class A\n def\n",
            "class A\n def \n",
            "def \n",
            "class A\n  private def \n",
            "module M\n  class B\n    def\n",
        ] {
            harness.change(&uri, source);
            for definition in harness.analysis.graph.definitions().values() {
                let (full, selection) = locator::spans(definition);
                assert!(
                    selection.0 >= full.0 && selection.1 <= full.1,
                    "{source:?}: selection {selection:?} escapes {full:?}"
                );
                if definition.name_offset().is_some_and(|name| {
                    let raw = definition.offset();
                    name.start() < raw.start() || name.end() > raw.end()
                }) {
                    // The shape VS Code threw on: Prism recovered `def` into a node spanning
                    // the three keyword bytes, with its name span in the whitespace after them.
                    assert_eq!(
                        selection, full,
                        "{source:?}: a name outside the span widens"
                    );
                    broken += 1;
                }
            }
        }
        assert!(
            broken > 0,
            "no fixture here recovered the pair the rule exists for"
        );
    }

    #[test]
    fn an_alias_is_navigable_from_its_call_sites() {
        // rubydex records a method call under its bare name — except an `alias`, which it
        // records with the parentheses already on. Both have to arrive at the same member key
        // or the lookup silently misses and `yell` navigates nowhere.
        let mut harness = Harness::new();
        let library = "class Person\n  def shout\n  end\n  alias yell shout\nend\n";
        let person = harness.write("app/person.rb", library);
        let source = "Person.new.yell\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        let link = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, "yell"),
            }),
        );
        assert!(!link.is_null(), "an alias call site navigates nowhere");
        assert_eq!(link[0]["targetUri"], person.as_str(), "{link}");

        // And from inside the `alias` statement itself, which is the reference rubydex records
        // with the parentheses already on.
        let from_alias = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": person.as_str() },
                "position": position_of(library, "shout\nend"),
            }),
        );
        assert!(!from_alias.is_null(), "{from_alias}");
    }

    #[test]
    fn a_half_typed_def_does_not_take_the_outline_down_with_it() {
        // Reported from a real editor: typing `def` inside a class made VS Code throw
        // `selectionRange must be contained in fullRange` and drop the *entire* outline, so the
        // file's structure vanished mid-keystroke. Prism recovers a bare `def` into a node whose
        // location is the three keyword bytes and whose name location is the whitespace *after*
        // them, so `range` ended where `selectionRange` began.
        //
        // Half-typed code is the normal state of a buffer, not an edge case, so the containment
        // rule has to hold for whatever the parser recovered.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();
        harness.open(&uri, "");

        for source in [
            "class A\n def\n",
            "class A\n def \n",
            "def \n",
            "class A\n  private def \n",
            "module M\n  class B\n    def\n",
        ] {
            harness.change(&uri, source);
            let outline = harness.outline(&uri);

            assert!(
                uncontained(&outline).is_empty(),
                "{source:?}: {:?}\n{outline}",
                uncontained(&outline)
            );
            // And nothing with no name in it: the recovered node is not a symbol yet, and a
            // blank row in the outline is the visible half of the same bug.
            for symbol in all_symbols(&outline) {
                let name = symbol["name"].as_str().unwrap_or_default();
                assert!(
                    !name.trim().is_empty(),
                    "{source:?}: blank symbol\n{outline}"
                );
            }
        }

        // Not vacuous by way of an empty answer: the enclosing class is still outlined while
        // the method inside it is being typed.
        harness.change(&uri, "class A\n def\n");
        assert_eq!(harness.outline(&uri)[0]["name"], "A");
    }

    #[test]
    fn a_definition_link_never_points_outside_the_construct_it_names() {
        // `LocationLink::targetSelectionRange` carries the identical containment rule, from the
        // identical pair of spans. VS Code does not validate this one, so the symptom is quieter
        // — goto-definition parks the cursor on a newline outside the construct it claims — but
        // it is the same defect and it is fixed in the same place.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();
        let source = "class A\n def\n";
        harness.open(&uri, source);

        // The whitespace after the keyword, which is where the recovered name span sits.
        let targets = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 1, "character": 4 },
            }),
        );

        let point = |value: &serde_json::Value| {
            (
                value["line"].as_u64().unwrap_or_default(),
                value["character"].as_u64().unwrap_or_default(),
            )
        };
        assert!(
            targets.as_array().is_some_and(|links| !links.is_empty()),
            "nothing to check: {targets}"
        );
        for target in targets.as_array().into_iter().flatten() {
            let (range, selection) = (&target["targetRange"], &target["targetSelectionRange"]);
            assert!(
                point(&selection["start"]) >= point(&range["start"])
                    && point(&selection["end"]) <= point(&range["end"]),
                "{targets}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Gem indexing
    // -----------------------------------------------------------------------

    /// Build a project with one installed gem, and an `Env` pointing at it.
    ///
    /// The gem is a real one in shape: unpacked under `gems/<full name>/lib`, with the
    /// serialised gemspec RubyGems writes beside it. The gem home deliberately sits *outside*
    /// the project, which is where a version manager puts it; the vendored case, where it does
    /// not, has its own test.
    fn project_with_gem(gem_source: &str) -> (tempfile::TempDir, tempfile::TempDir, gems::Env) {
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        std::fs::write(
            root.join("Gemfile.lock"),
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    shouty (1.2.3)\n",
        )
        .unwrap();

        let gem_home = elsewhere.path().to_path_buf();
        std::fs::create_dir_all(gem_home.join("gems/shouty-1.2.3/lib")).unwrap();
        std::fs::write(gem_home.join("gems/shouty-1.2.3/lib/shouty.rb"), gem_source).unwrap();
        std::fs::create_dir_all(gem_home.join("specifications")).unwrap();
        std::fs::write(
            gem_home.join("specifications/shouty-1.2.3.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = [\"lib\".freeze]\nend\n",
        )
        .unwrap();

        let env = gems::Env {
            gem_home: Some(gem_home),
            ..gems::Env::default()
        };
        (dir, elsewhere, env)
    }

    #[test]
    fn goto_definition_lands_inside_a_gem() {
        // Gem indexing in one test: a constant defined only in an installed gem, found with no
        // Ruby anywhere in the picture.
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty::Megaphone.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // Before the gems are in, the constant genuinely is not in the graph. Answering `null`
        // rather than guessing is the correct behaviour, and it is what the user sees during
        // the first second of a cold start.
        assert!(
            harness.definition_at(&uri, source, "Megaphone").is_null(),
            "a gem that has not been indexed yet must not produce an answer"
        );

        harness.index_gems();

        let definition = harness.definition_at(&uri, source, "Megaphone");
        let target = definition[0]["targetUri"].as_str().expect("a target uri");
        assert!(
            target.ends_with("gems/shouty-1.2.3/lib/shouty.rb"),
            "{definition}"
        );
    }

    #[test]
    fn a_require_of_a_gem_navigates_into_it() {
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "require \"shouty\"\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        // `shouty` is on no workspace load path; it resolves only because the gem's own `lib`
        // joined the load path when the gem was found.
        let definition = harness.definition_at(&uri, source, "shouty");
        let target = definition[0]["targetUri"].as_str().expect("a target uri");
        assert!(
            target.ends_with("gems/shouty-1.2.3/lib/shouty.rb"),
            "{definition}"
        );
    }

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

    #[test]
    fn a_workspace_that_never_says_which_ruby_it_uses_hears_about_it() {
        // `ruby_lib_dirs` refuses to guess a Ruby — rightly, because guessing puts Apple's
        // vestigial 2.6 stdlib into the graph and answers `"hello".u` with `unspace` — and
        // refusing in *silence* costs every one of Ruby's own 727 library files with nothing
        // said. The unit test in `workspace::gems`
        // pins the text; this pins that it reaches `window/showMessage` at all.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("ya-lsp.toml"), "[rbs]\nenabled = false\n").unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("lib/thing.rb", "class Thing\nend\n");
        harness.index();
        let _ = harness.messages();

        harness.index_gems();

        assert_eq!(harness.messages(), vec![messages::no_ruby_version()]);
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
    fn a_response_that_cannot_be_serialised_becomes_an_error_rather_than_silence() {
        // Every request's answer goes out through `reply`. `serde_json::to_value` cannot fail
        // for any type it is handed today, but the protocol has no shape for "no response":
        // a client that sent an id waits on it forever. An `InternalError` is the only exit
        // that lets the editor carry on.
        let id = RequestId::from(7);
        let ok = reply(&id, Some("fine"));
        assert_eq!(ok.response_result.expect("a result"), "fine");

        // A map whose keys are not strings is what `serde_json` actually refuses — a non-finite
        // float is quietly written as `null` rather than rejected.
        let broken = reply(
            &id,
            Some(std::collections::BTreeMap::from([((1u8, 2u8), 3u8)])),
        );
        let error = broken.response_result.expect_err("an error");
        assert_eq!(error.code, ErrorCode::InternalError as i32);
        assert!(error.message.contains("could not serialise"), "{error:?}");
    }

    #[test]
    fn a_signature_file_that_cannot_be_read_is_skipped_rather_than_indexed_raw() {
        // A signature root is walked and then read, and a file can go between the two. The
        // fallback is to leave it to `index_files`, which is the same answer as for a file
        // with no interfaces in it.
        let mut harness = Harness::new();
        let absent = harness.root.path().join("gone.rbs");
        assert!(!harness.analysis.index_edited_signature(&absent));

        // Not a signature file at all: nothing to edit, and the same answer.
        let ruby = harness.write("app/person.rb", "class Person\nend\n");
        assert!(
            !harness
                .analysis
                .index_edited_signature(&ruby.to_path().expect("a path"))
        );
    }

    #[test]
    fn references_from_a_method_definition_find_its_call_sites() {
        // The other half of `method_references_are_name_based`: the cursor on `def shout`
        // rather than on a call. What was defined decides the mechanism — a method is matched
        // by name, and a constant through the resolution.
        let mut harness = Harness::new();
        let declaration = "class Person\n  def shout\n  end\n  attr_reader :volume\nend\n";
        let person = harness.write("app/person.rb", declaration);
        let main = harness.write(
            "app/main.rb",
            "Person.new.shout\nPerson.new.volume\nother.shout\n",
        );
        harness.index();
        // Open, so the ranges come from the buffer rather than from a re-read of disk: an open
        // file is the one a find-references result is most likely to name.
        harness.open(&main, "Person.new.shout\nPerson.new.volume\nother.shout\n");

        // Name-based, so `other.shout` is in the answer too — stated rather than hidden.
        assert_eq!(
            harness.reference_list(&person, declaration, "shout", false),
            vec!["main.rb:0:11", "main.rb:2:6"]
        );
        // `attr_reader :volume` defines a method as much as `def` does.
        assert_eq!(
            harness.reference_list(&person, declaration, "volume", false),
            vec!["main.rb:1:11"]
        );
    }

    #[test]
    fn a_hover_on_an_untyped_receiver_caps_the_list_of_candidates() {
        // With no inference there is nothing to narrow `thing.call` down to. Listing all of
        // them would be a page; the count is what tells the user the answer is a guess.
        let mut harness = Harness::new();
        let mut classes = String::new();
        for index in 0..12 {
            classes.push_str(&format!("class Holder{index}\n  def call\n  end\nend\n"));
        }
        harness.write("app/holders.rb", &classes);
        let source = "thing.call\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "call")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("**12 possible definitions**"),
            "{markdown}"
        );
        assert!(markdown.contains("…and 2 more"), "{markdown}");
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

    /// A project whose Ruby is installed the way asdf installs one, with a default gem in it.
    ///
    /// The shape is the point: `gems/json-2.18.0/` exists and is *empty*, which is exactly what
    /// RubyGems leaves behind for a gem that ships inside Ruby, and the code is over in
    /// `lib/ruby/4.0.0/json.rb`.
    fn project_with_a_default_gem() -> (tempfile::TempDir, tempfile::TempDir, gems::Env) {
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");

        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    json (2.18.0)\n",
        )
        .unwrap();
        // Pins which Ruby the library directory is taken from, so the machine running the test
        // cannot answer with its own.
        std::fs::write(dir.path().join(".ruby-version"), "4.0.1\n").unwrap();
        std::fs::write(dir.path().join("ya-lsp.toml"), "[rbs]\nenabled = false\n").unwrap();

        let ruby = elsewhere.path().join(".asdf/installs/ruby/4.0.1/lib/ruby");
        std::fs::create_dir_all(ruby.join("gems/4.0.0/gems/json-2.18.0")).unwrap();
        std::fs::create_dir_all(ruby.join("4.0.0")).unwrap();
        std::fs::write(
            ruby.join("4.0.0/json.rb"),
            "# Ruby's JSON library.\nmodule JSON\n  def self.parse(source)\n  end\nend\n",
        )
        .unwrap();

        let env = gems::Env {
            home: Some(elsewhere.path().to_path_buf()),
            ..gems::Env::default()
        };
        (dir, elsewhere, env)
    }

    #[test]
    fn a_default_gem_is_navigable_even_though_its_gem_directory_is_empty() {
        let (dir, _home, env) = project_with_a_default_gem();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "require \"json\"\n\nJSON.parse(\"{}\")\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        // The constant, which exists nowhere but inside Ruby.
        let definition = harness.definition_at(&uri, source, "JSON");
        let target = definition[0]["targetUri"]
            .as_str()
            .unwrap_or_else(|| panic!("expected Ruby's own library, got {definition}"));
        assert!(target.ends_with("lib/ruby/4.0.0/json.rb"), "{definition}");

        // And the `require` that names it. This is the other half of the same gap: the load
        // path is what `require` resolves against, and Ruby's own library was never on it.
        let definition = harness.definition_at(&uri, source, "json\"");
        let target = definition[0]["targetUri"]
            .as_str()
            .unwrap_or_else(|| panic!("expected require navigation, got {definition}"));
        assert!(target.ends_with("lib/ruby/4.0.0/json.rb"), "{definition}");
    }

    #[test]
    fn default_gems_can_be_turned_off_without_turning_off_the_bundle() {
        let (dir, _home, env) = project_with_a_default_gem();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "JSON.parse(\"{}\")\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        assert!(harness.definition_at(&uri, source, "JSON").is_null());
    }

    #[test]
    fn gem_indexing_can_be_turned_off() {
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\nenabled = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty::Megaphone.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        assert!(harness.definition_at(&uri, source, "Megaphone").is_null());
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
    // Workspace symbols and references
    // -----------------------------------------------------------------------

    #[test]
    fn constant_references_are_resolved_rather_than_matched_by_name() {
        // The whole point of doing this against a resolved graph. `Person` inside `module HR`
        // and `HR::Person` at the top level are the same constant written two ways, and the
        // top-level `Person` is a different class that merely shares a name. Any grep gets all
        // three wrong.
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "module HR\n  class Person\n  end\n\n  class Team\n    def lead\n      Person.new\n    end\n  end\nend\n",
        );
        harness.write("app/person.rb", "class Person\nend\n");
        let source = "HR::Person.new\nPerson.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // The cursor is on the `Person` half of `HR::Person`, which is what the user points at.
        let found = harness.reference_list(&uri, source, "Person.new", false);
        assert_eq!(found, vec!["hr.rb:6:6", "main.rb:0:4"], "{found:?}");
        // Line 1's bare `Person` is a different class and is not in the list. Nothing that
        // matches on text could tell the two apart in either direction.
        assert!(!found.contains(&"main.rb:1:0".to_owned()), "{found:?}");
    }

    #[test]
    fn a_reference_is_the_name_the_user_wrote_not_the_call_around_it() {
        // rubydex fabricates a constant reference for every call with a constant receiver so
        // that `Person.new` can resolve against `Person`'s singleton class. Listing those bytes
        // would show a second, wider hit over text the user never wrote.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        let source = "Person.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let found = harness.references_at(&uri, source, "Person", false);
        assert_eq!(found.as_array().map(Vec::len), Some(1), "{found}");
        assert_eq!(
            found[0]["range"],
            serde_json::json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 6 },
            }),
            "{found}"
        );
    }

    #[test]
    fn references_can_be_asked_for_from_the_definition() {
        // The common gesture: the cursor is on `class Person`, not on a use of it.
        let mut harness = Harness::new();
        let declaration = "class Person\nend\n";
        let person = harness.write("app/person.rb", declaration);
        let source = "Person.new\n";
        harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.reference_list(&person, declaration, "Person", false),
            vec!["main.rb:0:0"]
        );
        // With the declaration included, its own name span joins the list.
        assert_eq!(
            harness.reference_list(&person, declaration, "Person", true),
            vec!["main.rb:0:0", "person.rb:0:6"]
        );
    }

    #[test]
    fn method_references_are_name_based_and_will_over_report() {
        // Stated rather than hidden: with no type inference, `shout` is `shout` whoever the
        // receiver is. `Megaphone#shout` is a different method and it is in the answer anyway.
        let mut harness = Harness::new();
        harness.write(
            "app/person.rb",
            "class Person\n  def shout\n  end\nend\n\nclass Megaphone\n  def shout\n  end\nend\n",
        );
        let source = "Person.new.shout\nMegaphone.new.shout\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let found = harness.reference_list(&uri, source, "shout", false);
        assert_eq!(found, vec!["main.rb:0:11", "main.rb:1:14"], "{found:?}");
    }

    #[test]
    fn a_call_to_a_method_that_was_never_defined_still_finds_its_call_sites() {
        // `define_method` and friends mean a name can have call sites and no declaration at
        // all. Routing through the resolution would answer `null` for exactly the code where
        // the editor's own word search is least able to help.
        let mut harness = Harness::new();
        let source = "widget.summon\nother.summon\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert!(!harness.has("#summon()"));

        assert_eq!(
            harness.reference_list(&uri, source, "summon", true),
            vec!["main.rb:0:7", "main.rb:1:6"]
        );
    }

    #[test]
    fn references_never_leave_the_users_own_code() {
        // A gem that uses the same constant is not an answer: nobody is going to edit it, and
        // for the name-based half of this feature a Rails bundle would drown the real hits.
        let (dir, _gem_home, env) = project_with_gem("Shouty = 1\nShouty\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        let found = harness.reference_list(&uri, source, "Shouty", true);
        assert!(
            found.iter().all(|hit| hit.starts_with("main.rb")),
            "{found:?}"
        );
    }

    #[test]
    fn a_symbol_search_ranks_the_name_the_user_typed_first() {
        // rubydex's fuzzy match is a subsequence test, so all four of these match `user` and it
        // scores them identically. Without a ranking of our own the picker's first row is
        // whichever one the hash map happened to yield.
        let mut harness = Harness::new();
        harness.write(
            "app/models.rb",
            "class UserSerializer\nend\n\nclass User\nend\n\nclass SuperUserPolicy\nend\n\nclass Ultra\n  def send_error(u)\n  end\nend\n",
        );
        harness.index();

        let found = harness.symbol_names("user");
        assert_eq!(
            found,
            vec![
                "User",
                "UserSerializer",
                "SuperUserPolicy",
                "Ultra#send_error"
            ],
            "{found:?}"
        );
    }

    const PICKER: &str = "\
class User
end

class UserSerializer
end

class SuperUserPolicy
end

module Admin
  class User
  end
end

class Ultra
  def send_error(u)
  end
end

class Account
  USER_LIMIT = 10

  def user
  end

  def user_name
  end
end
";

    /// The picker's rows the way it draws them: the name, the container the client shows beside
    /// it, and the file it would jump to.
    ///
    /// The file is here because `own` — the user's code before a gem's — is the first field the
    /// ranking sorts on and the decision the feature stands on, and it is invisible in a list of
    /// names.
    fn picker_rows(harness: &mut Harness, query: &str) -> Vec<String> {
        let found = harness.symbol_search(query);
        let Some(symbols) = found.as_array() else {
            return Vec::new();
        };
        symbols
            .iter()
            .map(|symbol| {
                let file = symbol["location"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                format!(
                    "{}  {}  {file}",
                    symbol["name"].as_str().unwrap_or_default(),
                    symbol["containerName"].as_str().unwrap_or("-"),
                )
            })
            .collect()
    }

    /// A project and a gem, opened by `picker` below.
    ///
    /// Every name here matches `user`, and they are chosen so that each is the *only* one that
    /// separates two of the ranking's fields: `User` and `Admin::User` differ only in qualified
    /// length, `Account#user` and `#user_name` only in simple length, `USER_LIMIT` only in case,
    /// `SuperUserPolicy` only in where the match falls, and `Ultra#send_error` matches nothing
    /// but a subsequence. The gem's `UserAgent` is an exact match on a name the project does not
    /// have, so its position is the whole of what `own` decides.
    /// The gem home comes back with the harness because dropping it deletes the gem, and the
    /// harness holds only the project's own directory.
    fn picker() -> (Harness, tempfile::TempDir) {
        let (dir, gem_home, env) =
            project_with_gem("class User\nend\n\nmodule Shouty\n  class UserAgent\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/accounts.rb", PICKER);
        harness.index();
        harness.index_gems();
        (harness, gem_home)
    }

    #[test]
    fn the_rows_of_a_symbol_search_in_the_order_the_picker_draws_them() {
        // `ANCESTRY`'s treatment for the other ranked list in the crate. Every field of `rank`
        // is separated by exactly one adjacent pair here, so the whole list is the ordering
        // stated once: `own` before everything (the gem's exact `UserAgent` is last, under a
        // subsequence match in the project), then match quality, then the shorter simple name,
        // then the shorter qualified one, then alphabetical.
        //
        // Four of these rows were asserted nowhere before — the constant, the nested class, the
        // second method and the gem — and a ranking is not a set of rows, it is their order.
        let (mut harness, _gem_home) = picker();

        assert_eq!(
            picker_rows(&mut harness, "user"),
            [
                "User  -  accounts.rb",
                "User  Admin  accounts.rb",
                "user  Account  accounts.rb",
                "user_name  Account  accounts.rb",
                "USER_LIMIT  Account  accounts.rb",
                "UserSerializer  -  accounts.rb",
                "SuperUserPolicy  -  accounts.rb",
                "send_error  Ultra  accounts.rb",
                "UserAgent  Shouty  shouty.rb",
            ]
        );
    }

    #[test]
    fn a_one_letter_query_ranks_rather_than_gives_up() {
        // The query a picker actually receives first, and the one every ordering decision was
        // made for: on a Rails bundle a single letter subsequence-matches most of a hundred and
        // fifty thousand declarations. Here it adds `Ultra`, `Account` and `Shouty` to the list
        // above — and puts none of them above a name the letter actually starts.
        let (mut harness, _gem_home) = picker();

        assert_eq!(
            picker_rows(&mut harness, "u"),
            [
                "User  -  accounts.rb",
                "User  Admin  accounts.rb",
                "user  Account  accounts.rb",
                "Ultra  -  accounts.rb",
                "user_name  Account  accounts.rb",
                "USER_LIMIT  Account  accounts.rb",
                "UserSerializer  -  accounts.rb",
                "Account  -  accounts.rb",
                "SuperUserPolicy  -  accounts.rb",
                "send_error  Ultra  accounts.rb",
                "UserAgent  Shouty  shouty.rb",
                "Shouty  -  shouty.rb",
            ]
        );
    }

    #[test]
    fn a_query_that_spells_a_path_is_matched_on_the_path() {
        // `rank`'s tier 1: the query matched nothing in the simple name and everything in the
        // qualified one, which is what somebody typing `Account#user` means and the only tier
        // that cannot be reached by typing a name.
        let (mut harness, _gem_home) = picker();

        assert_eq!(
            picker_rows(&mut harness, "Account#user"),
            [
                "user  Account  accounts.rb",
                "user_name  Account  accounts.rb",
            ]
        );
    }

    #[test]
    fn a_symbol_search_answers_the_same_list_however_the_query_is_cased() {
        // Every comparison in `tier` is case-insensitive, and a picker that reordered itself
        // when the user pressed shift would be worse than one that did not rank at all.
        let (mut harness, _gem_home) = picker();

        let typed = picker_rows(&mut harness, "user");
        assert_eq!(picker_rows(&mut harness, "User"), typed);
        assert_eq!(picker_rows(&mut harness, "USER"), typed);
    }

    #[test]
    fn a_symbol_search_prefers_the_users_own_code_to_a_gem() {
        // A gem reopening a class the project also defines is one declaration with definitions
        // in both, so there is one row and it has to point somewhere. It points at the file the
        // user can edit — and at the same place goto-definition would have taken them.
        let (dir, _gem_home, env) = project_with_gem("class Megaphone\n  def blare\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        harness.write("app/megaphone.rb", "class Megaphone\nend\n");
        harness.index();
        harness.index_gems();

        let found = harness.symbol_search("Megaphone");
        let symbols = found.as_array().expect("an array");
        assert!(
            symbols[0]["location"]["uri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("app/megaphone.rb"),
            "{found}"
        );
        // The gem's method is in the answer too — `Megaphone#blare` contains the query as a
        // subsequence — but a name that *is* the query outranks a name that merely contains it.
        assert_eq!(
            harness.symbol_names("Megaphone"),
            vec!["Megaphone", "Megaphone#blare"]
        );
    }

    #[test]
    fn a_symbol_search_reaches_into_the_gems() {
        // The moat again: a class that exists nowhere in the project is still findable.
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        harness.write("app/main.rb", "1\n");
        harness.index();
        assert!(harness.symbol_search("Megaphone").is_null());

        harness.index_gems();
        assert_eq!(harness.symbol_names("Megaphone"), vec!["Shouty#Megaphone"]);
    }

    #[test]
    fn symbols_are_spelled_the_way_the_outline_spells_them() {
        let mut harness = Harness::new();
        harness.write(
            "app/person.rb",
            "class Person\n  MAX_AGE = 120\n  attr_reader :name\n\n  class << self\n    def build\n    end\n  end\nend\n",
        );
        harness.index();

        assert_eq!(harness.symbol_names("build"), vec!["Person#self.build"]);
        assert_eq!(harness.symbol_names("MAX_AGE"), vec!["Person#MAX_AGE"]);
        assert_eq!(harness.symbol_names("name"), vec!["Person#name"]);
        // The singleton class itself has no name a person would ever search for: rubydex calls
        // it `Person::<Person>`, and it is not a thing you can jump to.
        assert!(
            !harness
                .symbol_names("Person")
                .iter()
                .any(|name| name.contains('<')),
            "{:?}",
            harness.symbol_names("Person")
        );
    }

    #[test]
    fn a_symbol_search_is_capped() {
        // At gem scale an unbounded answer is the failure mode: a two-letter query subsequence-
        // matches a large fraction of a bundle, on every keystroke.
        let mut harness = Harness::new();
        let classes: String = (0..MAX_WORKSPACE_SYMBOLS + 50)
            .map(|index| format!("class Widget{index}\nend\n"))
            .collect();
        harness.write("app/widgets.rb", &classes);
        harness.index();

        let found = harness.symbol_search("Widget");
        assert_eq!(
            found.as_array().map(Vec::len),
            Some(MAX_WORKSPACE_SYMBOLS),
            "the cap is not being applied"
        );
    }

    #[test]
    fn too_many_references_are_truncated_and_the_user_is_told() {
        // The cap is a safety property, not a preference: without it a name-based match in a
        // large workspace hands the editor a multi-megabyte response. Measured, `.new` across a
        // 17,557-file tree finds 35,733. Truncating silently would be a wrong answer that looks
        // exactly like a right one, so it is said out loud.
        let mut harness = Harness::new();
        let source = "widget.ping\n".repeat(MAX_REFERENCES + 1);
        let uri = harness.write("app/main.rb", &source);
        harness.index();

        let found = harness.references_at(&uri, &source, "ping", false);
        assert_eq!(found.as_array().map(Vec::len), Some(MAX_REFERENCES));

        assert_eq!(
            harness.messages(),
            vec![messages::references_truncated(
                MAX_REFERENCES + 1,
                MAX_REFERENCES
            )],
            "a truncated answer has to say so, and say by how much"
        );
    }

    #[test]
    fn nothing_found_is_null_rather_than_an_empty_list() {
        // The same contract as every other handler: `[]` claims the project has no such symbol,
        // which is only ever true by accident.
        let mut harness = Harness::new();
        let source = "class Person\nend\n";
        let uri = harness.write("app/person.rb", source);
        harness.index();

        assert!(harness.symbol_search("nothing_is_called_this").is_null());
        assert!(
            harness
                .references_at(&uri, source, "Person", false)
                .is_null()
        );
    }

    #[test]
    fn the_bytes_rubydex_invented_are_never_listed_as_references() {
        // rubydex fabricates a constant reference to `<Person>` for every call with a `Person`
        // receiver, so that the singleton class can be resolved. Those references are attached
        // to the singleton class — which is exactly what a cursor on `class << self` resolves
        // to. Verified by removing the filter: this returns `Person.new` in main.rb, a span the
        // user never wrote and cannot rename.
        let mut harness = Harness::new();
        let declaration = "class Person\n  class << self\n    def build\n    end\n  end\nend\n";
        let person = harness.write("app/person.rb", declaration);
        harness.write("app/main.rb", "Person.new\n");
        harness.index();

        assert!(
            harness
                .references_at(&person, declaration, "self\n", false)
                .is_null(),
            "a call is not a reference to the callee's singleton class"
        );
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    /// A project whose shape exercises every completion context.
    const OFFICE: &str = "\
module HR
  MAX_STAFF = 50

  class Person
    NAME_LIMIT = 40

    def self.build(name:, age: 1)
      new
    end

    def shout(volume)
      volume
    end

    private

    def secret
    end
  end

  class Manager < Person
    def delegate
    end
  end
end
";

    #[test]
    fn a_namespace_access_offers_what_is_inside_it() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // Alphabetical, because nothing has been typed for a length or a match quality to
        // separate them by.
        let found = harness.declarations_at(&uri, "HR::~\n");
        assert_eq!(found, vec!["MAX_STAFF", "Manager", "Person"], "{found:?}");
    }

    #[test]
    fn a_namespace_access_is_narrowed_by_what_has_been_typed() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(harness.declarations_at(&uri, "HR::Pe~\n"), vec!["Person"]);
        // And the nested one, which is where a bare name lookup would have stopped.
        assert_eq!(
            harness.declarations_at(&uri, "HR::Person::NAME~\n"),
            vec!["NAME_LIMIT"]
        );
    }

    #[test]
    fn a_constant_receiver_offers_singleton_methods_and_not_instance_ones() {
        // The distinction rubydex models with a synthetic singleton class, and the reason
        // `Foo.` resolves to `Foo::<Foo>` rather than to `Foo`.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "HR::Person.~\n");
        assert!(found.contains(&"build".to_owned()), "{found:?}");
        assert!(!found.contains(&"shout".to_owned()), "{found:?}");
    }

    #[test]
    fn an_expression_sees_the_lexical_scope_and_the_ancestor_chain() {
        let mut harness = Harness::new();
        let uri = harness.write("app/hr.rb", OFFICE);
        harness.index();

        let found = harness.declarations_at(
            &uri,
            &OFFICE.replace("    def delegate\n", "    def delegate\n      ~\n"),
        );
        // Its own method, its parent's, the constants either scope reaches, and the class.
        for expected in [
            "delegate",
            "shout",
            "secret",
            "MAX_STAFF",
            "NAME_LIMIT",
            "Person",
            "Manager",
        ] {
            assert!(
                found.contains(&expected.to_owned()),
                "{expected}: {found:?}"
            );
        }
        // `build` is a singleton method: not callable on an instance, so not offered.
        assert!(!found.contains(&"build".to_owned()), "{found:?}");
    }

    /// A project whose ancestry is written down, for pinning the *order* of a list.
    ///
    /// Every other fixture in this module asks whether a name is offered. This one asks where
    /// it lands, which is a question a green suite and a millisecond benchmark can both miss:
    /// `"hello".` opening on `DelegateClass, Digest, append_as_bytes, …` passes both.
    ///
    /// The shape is chosen so every rung of the ancestor chain holds exactly one method: `Item`
    /// includes `Auditable` and inherits `Record`, and `Object` sits past both. Reopening
    /// `String` and `Object` is what lets a literal receiver be ranked here at all — this
    /// harness has no core signatures by design, and adding them would be ~800 files of work
    /// for a question about ordering.
    ///
    /// `Item#initialize` and its `private def stash` are here so the pinned lists carry the
    /// other half of the question: not only where a row lands, but whether Ruby would let it be
    /// written at all. Both are absent from every explicit receiver below — and from the class
    /// body, where `self` is the class rather than an instance. They appear in exactly one list,
    /// the expression inside `#price`, which is the only cursor here that could write either.
    const ANCESTRY: &str = "\
module Store
  DEFAULT_CURRENCY = 1

  module Auditable
    def audit
    end
  end

  class Record
    def save
    end
  end

  class Item < Record
    include Auditable

    LIMIT = 10

    def self.build
    end

    def initialize
    end

    def price
      audit
    end

    private

    def stash
    end
  end
end

class String
  def shout
  end
end

class Object
  def global_helper
  end
end
";

    #[test]
    fn an_instance_receiver_is_ranked_by_ancestor_distance() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // One method per rung, in the order Ruby resolves them: the class, the module it
        // includes, the class it inherits, and `Object` past all three. Alphabetically this
        // reads `audit, global_helper, price, save`, which is what shipped.
        assert_eq!(
            harness.first_rows(&uri, "Store::Item.new.~\n", 10),
            [
                "price  Store::Item#price",
                "audit  Store::Auditable#audit",
                "save  Store::Record#save",
                "global_helper  Object#global_helper",
            ]
        );
    }

    #[test]
    fn a_literal_receiver_leads_with_its_own_class() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // The finding, in the small: alphabetical order puts `global_helper` first, and it is
        // the one row here that `String` does not declare.
        assert_eq!(
            harness.first_rows(&uri, "\"hi\".~\n", 10),
            ["shout  String#shout", "global_helper  Object#global_helper"]
        );
    }

    #[test]
    fn a_singleton_receiver_leads_with_the_class_own_methods() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(
            harness.first_rows(&uri, "Store::Item.~\n", 10),
            [
                "build  Store::Item.build",
                "global_helper  Object#global_helper"
            ]
        );
    }

    #[test]
    fn a_namespace_receiver_leads_with_what_is_nested_in_it() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // A module's `::` is its own contents and stops there. Alphabetical, because nothing has
        // been typed and a namespace has no ancestor chain to measure — the same seam the
        // untyped receiver sits on, for the same reason.
        //
        // What matters more than the order is the last row: there isn't one from `Object`.
        // rubydex's namespace walk deliberately stops before `Object`'s own members, or `String::`
        // would list every top-level constant in the project — and `Object` is where everything
        // it could not attribute ends up.
        assert_eq!(
            harness.first_rows(&uri, "Store::~\n", 10),
            [
                "Auditable  Store::Auditable",
                "DEFAULT_CURRENCY  Store::DEFAULT_CURRENCY",
                "Item  Store::Item",
                "Record  Store::Record",
            ]
        );

        // A *class* under `::` carries its singleton chain as well, because `Store::Item.build`
        // may also be written `Store::Item::build`. So the nested constant leads, the class's own
        // singleton method follows, and `Object` sits at the bottom where distance puts it.
        //
        // Which makes the two lists above and below asymmetric: `Store.` offers `global_helper`
        // and `Store::` does not, though both name the same module object. That is rubydex's
        // namespace walk rather than a rule stated here, and it is finding B's territory — the
        // fixture's job is to make the seam visible, not to close it.
        assert_eq!(
            harness.first_rows(&uri, "Store::Item::~\n", 10),
            [
                "LIMIT  Store::Item::LIMIT",
                "build  Store::Item.build",
                "global_helper  Object#global_helper",
            ]
        );
    }

    #[test]
    fn a_namespace_receiver_is_ranked_by_how_well_the_name_matches() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // One letter is enough to separate them, and it has to be the right way round: `Item`
        // starts with it, `Auditable` merely contains it (`aud-i-table`, folded). Alphabetically
        // `Auditable` leads, so this is the one namespace list whose order is a claim rather
        // than the alphabet — and the row a user typed `I` for is the one they get.
        assert_eq!(
            harness.first_rows(&uri, "Store::I~\n", 10),
            ["Item  Store::Item", "Auditable  Store::Auditable"]
        );
    }

    #[test]
    fn an_expression_is_ranked_outwards_from_the_cursor() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // Two scales meeting on one number. Constants are reached through the lexical nesting
        // and methods through the ancestor chain, so each walk is counted from the cursor
        // rather than laid end to end — otherwise every method in the list would sit below
        // every constant, or the reverse.
        //
        // `Object` is where they touch: it is the last rung of the ancestor chain *and* the
        // outermost lexical scope. It has to be scored as the former, which is why `save` on
        // `Record` outranks `global_helper` here. Score it as the latter and everything rubydex
        // could not attribute — the whole of finding B — lands one step from the cursor.
        assert_eq!(
            harness.first_rows(&uri, &ANCESTRY.replace("      audit\n", "      ~\n"), 12),
            [
                "LIMIT  Store::Item::LIMIT",
                // The receiver is implicit here, so Ruby permits both of these and the three
                // receiver lists above must not carry either. That they do not is what makes
                // this pair a guard rather than decoration.
                "initialize  Store::Item#initialize",
                "price  Store::Item#price",
                "stash  Store::Item#stash",
                "Auditable  Store::Auditable",
                "DEFAULT_CURRENCY  Store::DEFAULT_CURRENCY",
                "Item  Store::Item",
                "Record  Store::Record",
                "audit  Store::Auditable#audit",
                "save  Store::Record#save",
                "Object  Object",
                "Store  Store",
            ]
        );
    }

    #[test]
    fn a_class_body_is_ranked_from_the_class_the_cursor_is_writing() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // The receiver is implicit and `self` is the *class*, not an instance of it — so this
        // list is the mirror of the one above. `build` is offered without a receiver, because
        // that is where `def self.build` can be called from; `initialize`, `price` and `stash`
        // are gone, because none of the three can be written here at all. The pair is the whole
        // point of both assertions: the same fixture, the same names, two cursors, and Ruby
        // permits a different set at each.
        assert_eq!(
            harness.first_rows(
                &uri,
                &ANCESTRY.replace("    LIMIT = 10\n", "    LIMIT = 10\n    ~\n"),
                10
            ),
            [
                "LIMIT  Store::Item::LIMIT",
                "build  Store::Item.build",
                "Auditable  Store::Auditable",
                "DEFAULT_CURRENCY  Store::DEFAULT_CURRENCY",
                "Item  Store::Item",
                "Record  Store::Record",
                "Object  Object",
                "Store  Store",
                "String  String",
                // Last of the declarations, which is where `Object` belongs: everything rubydex
                // could not attribute lands there, and a class body is one keystroke from being
                // the list finding B is about.
                "global_helper  Object#global_helper",
            ]
        );
    }

    #[test]
    fn an_untyped_receiver_falls_back_to_the_alphabet_when_nothing_is_nearer() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // The seam, pinned deliberately. The typed and untyped paths share a ranking
        // constructor and nothing else: one walks a receiver's ancestor chain, the other is a
        // flat name search over the graph with no receiver in it at all. There is no distance
        // where there is no chain, so this list is exactly what it was — which is the case
        // against leaving it as a list, not an argument that it is fine.
        //
        // Every candidate is in the one file this fixture has, so `Locality` scores them all
        // alike and says nothing — which is the point. A ranking term that invents an order
        // where there is no information would be worse than the alphabet, not better.
        //
        // The receiver is written after the last `end` on purpose. Put it inside the method and
        // Prism's recovery eats that `end` instead, refiling every later top-level class one
        // level deeper: the answer is the same six names, spelled `Store::String#shout`.
        assert_eq!(
            harness.first_rows(&uri, &format!("{ANCESTRY}@foo.~\n"), 12),
            [
                "audit  Store::Auditable#audit",
                "build  Store::Item.build",
                "global_helper  Object#global_helper",
                "price  Store::Item#price",
                "save  Store::Record#save",
                "shout  String#shout",
            ]
        );
    }

    #[test]
    fn an_untyped_receiver_is_ranked_by_how_near_the_file_is() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.write("app/near.rb", "class Near\n  def zzz_near\n  end\nend\n");
        harness.write("lib/far.rb", "class Far\n  def aaa_far\n  end\nend\n");
        harness.index();

        // The two names are spelled to sort the wrong way round on purpose: `aaa_far` is the
        // alphabetically first method in the whole project and it belongs last, `zzz_near` is
        // the last and belongs above it. Nothing else here can tell them apart — both are the
        // user's own code, neither matches a prefix, and there is no receiver to measure a
        // chain from.
        assert_eq!(
            harness.first_rows(&uri, &format!("{ANCESTRY}@foo.~\n"), 12),
            [
                "audit  Store::Auditable#audit",
                "build  Store::Item.build",
                "global_helper  Object#global_helper",
                "price  Store::Item#price",
                "save  Store::Record#save",
                "shout  String#shout",
                "zzz_near  Near#zzz_near",
                "aaa_far  Far#aaa_far",
            ]
        );
    }

    #[test]
    fn an_rbs_interface_never_reaches_the_graph() {
        // The end of the path `analysis::signatures` starts: the unit tests there check the text
        // that comes out, and this checks that rubydex agreed to read it and that nothing from
        // inside the block survived indexing.
        //
        // A signature root of its own rather than the vendored one, so the fixture owns exactly
        // what is in it: `_Reader` at the top level lands its member on `Object`, and `_Rand`
        // inside `class Bag` lands its member on `Bag` — both shapes appear in one real file,
        // `core/array.rbs`, and a rule that only looked at the top level would miss the second.
        let dir = tempfile::tempdir().expect("tempdir");
        let core = dir.path().join("sig/core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(
            core.join("bag.rbs"),
            "\
class Bag
  %a{deprecated: Use Bag::_Rand, or make your own}
  interface _Rand
    def roll: (Integer max) -> Integer
  end

  def keep: () -> void
end

interface _Reader
  def read: () -> String
end
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                dir.path().join("sig").display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write("app/main.rb", "");
        harness.index();
        harness.index_gems();

        // The signatures did get indexed — without this the rest of the test passes vacuously,
        // and an edited file rbs refuses to parse is exactly how it would come to.
        assert!(
            harness.has("Bag#keep()"),
            "the signature root was not indexed"
        );
        assert!(!harness.has("Bag#roll()"));
        assert!(!harness.has("Object#read()"));

        // And the shape a user sees: `Object` is every receiver's ancestor, so a member misfiled
        // there is offered on everything in the language.
        let found = harness.declarations_at(&uri, "Bag.new.~\n");
        assert!(found.contains(&"keep".to_owned()), "{found:?}");
        assert!(!found.contains(&"roll".to_owned()), "{found:?}");
        assert!(!found.contains(&"read".to_owned()), "{found:?}");
    }

    #[test]
    fn a_projects_own_signatures_are_edited_the_same_way() {
        // The second of the two routes an `.rbs` file takes into the graph, and the one that was
        // missed the first time. These files arrive through `index_workspace` rather than through
        // the background signature index; filtered one way and not the other, `Object#slurp` came
        // back and was offered on every receiver in the project.
        //
        // **No `index.include` here, and that is half of what this test pins.** With a
        // `**/*.rb` default a project's own `sig/` reaches nothing at all unless the project
        // widens the glob itself — the feature exists and is invisible.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("sig")).unwrap();
        std::fs::write(
            dir.path().join("sig/widget.rbs"),
            "\
class Widget
  interface _Spinnable
    def spin: () -> void
  end

  def render: () -> String
end

interface _Readerish
  def slurp: () -> String
end
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\nenabled = false\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write("lib/app.rb", "class Widget\nend\n");
        harness.index();

        assert!(
            harness.has("Widget#render()"),
            "the sig/ directory was not indexed"
        );
        assert!(!harness.has("Widget#spin()"));
        assert!(!harness.has("Object#slurp()"));

        let found = harness.declarations_at(&uri, "Widget.new.~\n");
        assert_eq!(found, vec!["render".to_owned()], "{found:?}");
    }

    #[test]
    fn a_gems_own_signatures_are_indexed_and_type_its_methods() {
        // A gem's own `sig/`, end to end. It is excluded twice over unless both halves are
        // fixed — by the
        // `.rb` extension filter and by the walk running over `require_paths`, which never
        // contains it — so a gem that ships correct, maintained RBS contributed none of it.
        let (dir, gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let sig = gem_home.path().join("gems/shouty-1.2.3/sig");
        std::fs::create_dir_all(&sig).unwrap();
        std::fs::write(
            sig.join("shouty.rbs"),
            "\
module Shouty
  class Megaphone
    def shout: () -> String
  end
end
",
        )
        .unwrap();
        // Ruby's own signatures, so `String` exists to be chained from.
        let signatures = dir.path().join("rbs");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\ndefault_gems = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "Shouty::Megaphone.new.shout.upcase\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        assert!(
            harness.has("Shouty::Megaphone#shout()"),
            "the gem's sig/ was not indexed"
        );

        // The chain is the assertion, not the declaration: `upcase` resolves at all only because
        // the table read `-> String` out of a gem's own signature, and the card names the
        // signature it followed rather than leaving the reader to guess.
        let markdown = card(&mut harness, &uri, source, "upcase");
        assert!(markdown.contains("String#upcase"), "{markdown}");
        assert!(
            markdown.contains("Shouty::Megaphone#shout()"),
            "the card has to name the gem signature it followed: {markdown}"
        );

        let found = harness.declarations_at(&uri, "Shouty::Megaphone.new.shout.~\n");
        assert!(found.contains(&"upcase".to_owned()), "{found:?}");
    }

    /// A project whose bundle holds one Rails engine: a gem with `require_paths = ["lib"]`
    /// whose real code is under `app/`.
    fn project_with_engine(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, gems::Env) {
        let (dir, elsewhere, env) = project_with_gem("module Shouty\n  class Horn\n  end\nend\n");
        let gem = elsewhere.path().join("gems/shouty-1.2.3");
        for (relative, source) in files {
            let path = gem.join("app").join(relative);
            std::fs::create_dir_all(path.parent().expect("a relative path")).unwrap();
            std::fs::write(path, source).unwrap();
        }
        // Kept alive by the caller: `elsewhere` is a `TempDir` and dropping it would delete the
        // bundle out from under the test.
        let root = elsewhere.keep();
        (dir, root, env)
    }

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
    fn a_require_never_resolves_against_an_engines_app_directory() {
        // The teeth of the third-list rule, and the sharpest shape of it: this engine ships
        // `app/shouty.rb` *and* `lib/shouty.rb`. If `app/` were a load path — the one-line
        // version of gate 1 — `require "shouty"` would silently start meaning the other file.
        let (dir, root, env) = project_with_engine(&[("shouty.rb", "class Decoy\nend\n")]);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "require \"shouty\"\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        assert!(harness.has("Decoy"), "the decoy really is indexed");
        let found = harness.definition_at(&uri, source, "shouty").to_string();
        assert!(
            found.contains("lib/shouty.rb") && !found.contains("app/shouty.rb"),
            "the require resolves against lib/ and nothing else: {found}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_engines_associations_declare_members_on_the_engines_own_class() {
        // Gate 2, end to end. `ActiveStorage::Blob` writes `has_many :attachments, class_name:
        // "ActiveStorage::Attachment"` and an application chains off it. Without gate 2 the pass
        // skips the file because `is_own_code` says no, so an engine's whole declarative surface
        // stays invisible even once gate 1 has indexed it.
        let (dir, root, env) = project_with_engine(&[
            (
                // `< ActiveRecord::Base` because the host test asks whether it is a model, and
                // because it is what an engine really writes: `ActiveStorage::Blob` reaches the
                // base through `ActiveStorage::Record`, one file away in the same `app/`.
                "models/shouty/message.rb",
                "class Shouty::Message < ActiveRecord::Base\n  \
                 belongs_to :horn, class_name: \"Shouty::Horn\"\n\
                 end\n",
            ),
            (
                "models/shouty/horn.rb",
                "class Shouty::Horn\n  def toot\n  end\nend\n",
            ),
        ]);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let uri = harness.write("app/main.rb", "Shouty::Message.new\n");
        harness.index();
        harness.index_gems();

        assert!(
            harness.has("Shouty::Message#horn()"),
            "the engine's belongs_to declared nothing"
        );
        let found = harness.declarations_at(&uri, "Shouty::Message.new.horn.~\n");
        assert!(
            found.contains(&"toot".to_owned()),
            "and the chain runs on through it: {found:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_engine_may_not_claim_a_table_or_host_the_route_helpers() {
        // The two of `Context`'s six outputs that mean "the application" rather than "a class
        // the reader can name", and each is a way gate 2 could have made an answer worse.
        //
        // A table is claimed by pluralizing a top-level class's name, so an engine that defines
        // one would take a table the application's own model owns — or, as here, a table no
        // class of the user's has, which would put the schema's columns on somebody's gem. And
        // an engine's controllers are hosts in Rails and are not hosts here, because
        // `List::Routes` is closed to engines: there are no helpers for them to be given.
        let (dir, root, env) = project_with_engine(&[
            ("models/widget.rb", "class Widget\nend\n"),
            (
                "controllers/shouty/base_controller.rb",
                "class Shouty::BaseController < ActionController::Base\nend\n",
            ),
            ("models/shouty/message.rb", "class Shouty::Message\nend\n"),
            // A module whose name is the one spelling that makes a module a host. Rails does
            // give an engine's helper modules the application's route helpers; ya-lsp does not,
            // because `List::Routes` is closed to engines and there is nothing to give them.
            (
                "helpers/shouty/blast_helper.rb",
                "module Shouty::BlastHelper\nend\n",
            ),
        ]);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[8.0].define(version: 1) do\n  \
             create_table \"widgets\", force: :cascade do |t|\n    \
             t.string \"name\"\n  \
             end\n\
             end\n",
        );
        harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :stories\nend\n",
        );
        harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController < ActionController::Base\nend\n",
        );
        harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\nend\n",
        );
        harness.index();
        harness.index_gems();

        let context = harness.analysis.context();
        assert!(
            context.classes.contains("Shouty::Message"),
            "the engine's classes are nameable — that is the widening"
        );
        assert!(
            !context.claims.contains_key("widgets"),
            "and an engine claims no table: {:?}",
            context.claims
        );
        assert!(
            !harness.has("Widget#name()"),
            "so the schema declares nothing on it"
        );
        let hosts = format!("{:?}", context.hosts);
        assert!(
            hosts.contains("ApplicationController") && hosts.contains("StoriesHelper"),
            "the application's own base and helper module are hosts: {hosts}"
        );
        assert!(
            !hosts.contains("Shouty::BaseController") && !hosts.contains("Shouty::BlastHelper"),
            "and neither the engine's controller nor its helper module is: {hosts}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_engines_routes_reach_the_applications_controllers_only_when_they_draw_into_it() {
        // The routes reader, over a gem's own `config/routes.rb`. `activestorage` writes
        // `Rails.application.routes.draw` and its `rails_direct_uploads_path` really is a method
        // on this application's controllers; `blazer` writes `Blazer::Engine.routes.draw` and
        // its helpers are reached as `blazer.queries_path` after a `mount`, which is a spelling
        // this crate does not read and which zero of six applications use.
        let (dir, root, env) = project_with_engine(&[]);
        let gem = root.join("gems/shouty-1.2.3/config");
        std::fs::create_dir_all(&gem).unwrap();
        std::fs::write(
            gem.join("routes.rb"),
            "Rails.application.routes.draw do\n  \
             resources :megaphones, only: [:show]\n  \
             resources :stories, only: [:index]\n\
             end\n",
        )
        .unwrap();

        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        // The application names one of the two itself, which is the collision that decides
        // whether "the project's own routes file first" is load-bearing or cosmetic.
        harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :stories\nend\n",
        );
        harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController < ActionController::Base\nend\n",
        );
        let source = "class StoriesController < ApplicationController\n                        def show\n    megaphone_path\n    stories_path\n  end\nend\n";
        let uri = harness.write("app/controllers/stories_controller.rb", source);
        harness.index();
        harness.index_gems();

        let engine = harness.hover_at(&uri, source, "megaphone_path").to_string();
        assert!(
            engine.contains("shouty-1.2.3/config/routes.rb"),
            "the engine's helper is a method on this application's controller: {engine}"
        );

        // And the application's own routes file declares the name they share, so the jump lands
        // in the project rather than in somebody's bundle.
        let own = harness.hover_at(&uri, source, "stories_path").to_string();
        assert!(
            own.contains("config/routes.rb") && !own.contains("shouty-1.2.3"),
            "the project's own file wins the collision: {own}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_engine_that_draws_into_its_own_route_set_declares_no_helper() {
        // The three of seven that would otherwise put a helper on every controller in the
        // project: blazer, pghero and mission_control-jobs. The decline is `rails::Whose`, and
        // it has to survive the whole pass rather than only the reader — `Reader::call` walks an
        // unknown call's block transparently, so a fall-through would read this body as though
        // the application had written it.
        let (dir, root, env) = project_with_engine(&[]);
        let gem = root.join("gems/shouty-1.2.3/config");
        std::fs::create_dir_all(&gem).unwrap();
        std::fs::write(
            gem.join("routes.rb"),
            "Shouty::Engine.routes.draw do\n  resources :megaphones, only: [:show]\nend\n",
        )
        .unwrap();

        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :stories, only: [:index]\nend\n",
        );
        harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController < ActionController::Base\nend\n",
        );
        let uri = harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController < ApplicationController\n  def index\n  end\nend\n",
        );
        harness.index();
        harness.index_gems();

        let found = harness.declarations_at(
            &uri,
            "class StoriesController < ApplicationController\n  def index\n    ~\n  end\nend\n",
        );
        assert!(
            found.contains(&"stories_path".to_owned()),
            "the application's own helpers are still there: {found:?}"
        );
        assert!(
            !found.iter().any(|name| name.starts_with("megaphone")),
            "and the engine's own route set contributes none: {found:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_caption_names_the_gem_a_file_came_from_or_falls_back_to_the_file() {
        // `workspace_relative` would say nothing outside the root can reach it; an engine makes
        // that false — an engine's `config/routes.rb` declares helpers now — and the first card
        // the engine-routes test printed read "From `routes.rb`", the same caption the project's
        // own routes file gets. The marker is a directory named `gems`, which every layout
        // `gems::gem_roots` knows ends in.
        let gem = |path: &str| {
            synthesize::gem_relative(Path::new(path)).map(|it| it.to_string_lossy().into_owned())
        };
        assert_eq!(
            gem("/home/me/.gem/ruby/4.0.0/gems/shouty-1.2.3/config/routes.rb"),
            Some("shouty-1.2.3/config/routes.rb".to_owned())
        );
        // A git source unpacks under `bundler/gems`, and the same marker finds it.
        assert_eq!(
            gem("/w/vendor/bundle/ruby/4.0.0/bundler/gems/shouty-abc123/app/models/m.rb"),
            Some("shouty-abc123/app/models/m.rb".to_owned())
        );
        // And a path with no such ancestor gets nothing, so the caller keeps its own fallback
        // rather than this one guessing at a name.
        assert_eq!(gem("/somewhere/else/config/routes.rb"), None);
    }

    #[test]
    fn which_of_the_six_lists_an_engines_document_may_go_on() {
        // The engine rule, asserted through the real path rather than by reading the table:
        // a list is open to an engine when what it reads declares members on a class the reader
        // can name, and closed when it declares something scoped to an application.
        //
        // `Routes` is open, and what a gem's routes file is *allowed to say* is `rails::Whose`'s
        // question rather than this list's — the list only decides which documents a generator
        // may open. The two closed ones are the ones with a consequence: a `schema.rb` is the
        // *application's* database and an engine ships migrations rather than one, and
        // `self.table_name=` is the input to a generator that is itself closed. Both files are
        // staged under `app/` precisely so the list rule is what is being measured rather than
        // the walk.
        use synthesize::List;
        let (dir, root, env) = project_with_engine(&[
            (
                "models/shouty/message.rb",
                "class Shouty::Message\n  has_many :horns\nend\n",
            ),
            (
                "models/shouty/tagged.rb",
                "class Shouty::Tagged\n  # @return [String]\n  def tag\n  end\nend\n",
            ),
            (
                "jobs/shouty/blast_job.rb",
                "class Shouty::BlastJob < ActiveJob::Base\n  def perform\n  end\nend\n",
            ),
            (
                "models/shouty/renamed.rb",
                "class Shouty::Renamed\n  self.table_name = \"loud\"\nend\n",
            ),
            (
                "misc/schema.rb",
                "ActiveRecord::Schema[8.0].define(version: 1) do\nend\n",
            ),
            (
                "misc/routes.rb",
                "Rails.application.routes.draw do\n  resources :blobs\nend\n",
            ),
        ]);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty::Message.new\n");
        harness.index();
        harness.index_gems();

        let context = harness.analysis.context();
        let on = |list: List, needle: &str| {
            context
                .documents(list)
                .iter()
                .any(|uri| uri.contains("shouty-1.2.3") && uri.ends_with(needle))
        };
        assert!(on(List::Models, "message.rb"), "models open");
        assert!(on(List::Annotated, "tagged.rb"), "annotations open");
        assert!(on(List::Entrypoints, "blast_job.rb"), "entry points open");
        assert!(on(List::Routes, "routes.rb"), "routes open");
        assert!(!on(List::Renamed, "renamed.rb"), "renames closed");
        assert!(!on(List::Schemas, "schema.rb"), "schemas closed");
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

    #[test]
    fn ruby_that_is_not_named_rb_is_indexed_with_no_configuration() {
        // Ruby that is not named `.rb`: `Rakefile`, `Gemfile`, `*.gemspec`, `*.rake` and `config.ru`
        // are Ruby, define constants and methods like any other Ruby, and were missed by a
        // default include of `**/*.rb` — **12 files in lobsters**.
        //
        // rubydex dispatches on the extension and calls everything that is not `.rbs` Ruby, so
        // no name here needs a special case; what needed one was the glob.
        let mut harness = Harness::new();
        harness.write("Rakefile", "class RakeRoot\nend\n");
        harness.write(
            "lib/tasks/build.rake",
            "class BuildTask\n  def run\n  end\nend\n",
        );
        harness.write("Gemfile", "class GemfileRoot\nend\n");
        harness.write("thing.gemspec", "class GemspecRoot\nend\n");
        harness.write("config.ru", "class RackRoot\nend\n");
        // Data Bundler writes, not Ruby, and it must stay out however similar the name looks.
        harness.write("Gemfile.lock", "GEM\n  specs:\n");
        let source = "BuildTask.new.run\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        for name in [
            "RakeRoot",
            "BuildTask",
            "GemfileRoot",
            "GemspecRoot",
            "RackRoot",
        ] {
            assert!(harness.has(name), "{name} was not indexed");
        }
        assert!(!harness.has("GEM"), "Gemfile.lock is not Ruby");

        // In one line: a `.rake` file's method is a go-to-definition target.
        let definition = harness.definition_at(&uri, source, "run");
        let target = definition[0]["targetUri"].as_str().expect("a target uri");
        assert!(target.ends_with("lib/tasks/build.rake"), "{definition}");
    }

    // -----------------------------------------------------------------------
    // Generated declarations, and where they came from
    // -----------------------------------------------------------------------

    /// A schema of the shape the reader takes: two columns on one table.
    const SCHEMA: &str = "\
ActiveRecord::Schema.define(version: 1) do
  create_table \"stories\" do |t|
    t.string \"title\"
    t.string \"byline\"
  end
end
";

    /// What a generator writes for [`SCHEMA`].
    const SCHEMA_RBS: &str = "\
class Story
  def title: () -> String
  def byline: () -> String
end
";

    /// The byte span of `needle` in `text`, for building a mapping out of a fixture rather than
    /// out of hand-counted offsets that go stale the moment a line moves.
    fn span(text: &str, needle: &str) -> (u32, u32) {
        let at = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in the fixture"));
        let start = u32::try_from(at).expect("a fixture shorter than 4GB");
        (start, start + u32::try_from(needle.len()).expect("ditto"))
    }

    /// A project with a model, a file that declares columns, and Ruby's own signatures.
    ///
    /// The declaring file is deliberately **not** at `db/schema.rb`, and that is what makes
    /// this fixture worth keeping beside the real schema reader: what these tests pin is the
    /// side table — replace rather than append, no mapping means no place, a deleted source
    /// takes its declarations with it — and every generator relies on the same
    /// table from a different kind of file. Pointing them at the real schema would test the
    /// schema reader instead, and would stop testing the withheld answer at all, because a real
    /// reader maps every declaration it writes.
    ///
    /// So the tests here play a generator, and what they hand over is exactly what
    /// [`Analysis::synthesize`] hands over: RBS text, and one span of it per line that implied
    /// it.
    fn synthetic_project(caller: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let schema = harness.write("db/legacy.rb", SCHEMA);
        let uri = harness.write("app/main.rb", caller);
        harness.index();
        harness.index_gems();
        (harness, schema, uri)
    }

    /// The mapping a generator would record beside [`SCHEMA_RBS`]: the `title` column, and
    /// deliberately not the `byline` one.
    ///
    /// One mapping short on purpose. Half of what this table is for is the answer it *withholds*
    /// — a generator that emits a declaration and forgets to say where it came from must lose
    /// the jump rather than invent one — and a fixture where everything is mapped could not tell
    /// the two apart.
    fn title_only(schema: &DocUri) -> Vec<synthesized::Mapping> {
        vec![synthesized::Mapping {
            generated: span(SCHEMA_RBS, "  def title: () -> String\n"),
            declared: Site {
                uri: schema.as_str().to_owned(),
                full: span(SCHEMA, "t.string \"title\""),
                selection: span(SCHEMA, "\"title\""),
            },
        }]
    }

    #[test]
    fn a_generated_declaration_jumps_to_the_line_that_declared_it() {
        // The first thing the side table has to do, and the reason it exists at all: the
        // declaration is real, its offsets are into bytes nobody can open, and the place the
        // user has to land is a line in a completely different file.
        let source = "Story.new.title.upcase\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        assert!(
            harness.has("Story#title()"),
            "the generated RBS was not indexed"
        );

        let definition = harness.definition_at(&uri, source, "title");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str()),
            "{definition}"
        );
        // The two spans a link carries, and the mapping decides both: the whole `t.string` call
        // to reveal, the string literal to select.
        assert_eq!(
            definition[0]["targetRange"]["start"]["line"], 2,
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetRange"]["start"]["character"], 4,
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetSelectionRange"]["start"]["character"], 13,
            "{definition}"
        );

        // The other half of the boundary, in the same call: the graph learned the declaration and
        // the type table learned what it returns. `upcase` resolves at all only because
        // `-> String` was harvested out of text this crate wrote itself.
        let markdown = card(&mut harness, &uri, source, "upcase");
        assert!(markdown.contains("String#upcase"), "{markdown}");
        assert!(
            harness
                .declarations_at(&uri, "Story.new.title.~\n")
                .contains(&"upcase".to_owned())
        );
    }

    #[test]
    fn a_generated_declaration_with_no_mapping_is_not_a_jump_target() {
        // The second, and the one the table exists for. `byline` is in the graph, it
        // types its receiver, and it has nowhere to go — so it answers everything *except* the
        // question whose wrong answer opens a file that is not there.
        let source = "Story.new.byline.upcase\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        assert!(harness.has("Story#byline()"));
        let markdown = card(&mut harness, &uri, source, "byline");
        assert!(markdown.contains("Story#byline"), "{markdown}");

        let definition = harness.definition_at(&uri, source, "byline");
        assert!(
            definition.is_null(),
            "an unmapped declaration is not a place: {definition}"
        );

        // Not by luck: the generated document's URI is not one a client could be sent, and no
        // answer anywhere names it.
        assert!(DocUri::from_uri_str(&synthesized::generated_uri(&schema)).is_none());
    }

    #[test]
    fn the_class_a_generated_document_reopens_still_jumps_to_the_users_own_file() {
        // A generated document defines `class Story` because RBS has no other way to hang a
        // method off it, so the declaration has two definitions: one the user wrote and one
        // this crate did. Keying the table by *declaration* would have redirected both and
        // taken the model file off the map; keying it by definition leaves the real one alone.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        let definition = harness.definition_at(&uri, source, "Story");
        let targets = definition.as_array().expect("a link");
        assert_eq!(targets.len(), 1, "{definition}");
        assert!(
            targets[0]["targetUri"]
                .as_str()
                .is_some_and(|target| target.ends_with("app/models/story.rb")),
            "{definition}"
        );
    }

    #[test]
    fn regenerating_a_source_leaves_one_answer_rather_than_two() {
        // The third. An edit to `db/schema.rb` re-runs whatever read it, and the
        // failure being designed out is silent: a column that was renamed answering from both
        // its old name and its new one, forever, with nothing on screen to say so.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        let renamed = "class Story\n  def headline: () -> String\nend\n";
        harness.synthesize(
            &schema,
            renamed,
            vec![synthesized::Mapping {
                generated: span(renamed, "  def headline: () -> String\n"),
                declared: Site {
                    uri: schema.as_str().to_owned(),
                    full: span(SCHEMA, "t.string \"byline\""),
                    selection: span(SCHEMA, "\"byline\""),
                },
            }],
        );

        assert!(harness.has("Story#headline()"));
        assert!(
            !harness.has("Story#title()"),
            "the column that went away is still answering"
        );
        assert!(
            harness.definition_at(&uri, source, "title").is_null(),
            "and still jumping"
        );
    }

    #[test]
    fn a_generated_declaration_whose_source_is_deleted_stops_being_reachable() {
        // The fourth, through the path a `git checkout` takes: the watcher says the
        // file is gone. Nothing will ever re-read a file that is not there, so a generated
        // declaration left behind by that would outlive every chance to correct it.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        std::fs::remove_file(schema.to_path().unwrap()).unwrap();
        harness.watch(&[&schema]);

        assert!(!harness.has("Story#title()"));
        assert!(harness.analysis.synthesized.is_empty());
        assert!(harness.definition_at(&uri, source, "title").is_null());
    }

    #[test]
    fn a_generated_document_is_not_the_users_code_and_says_nothing_on_screen() {
        // A generated document is not under the workspace root — it is not under any root — so
        // the test that keeps a vendored bundle's diagnostics off the screen already covers it.
        // Worth pinning rather than assuming: a generated document is a document like any other
        // to rubydex, and every rule that keeps somebody else's files off the screen has to hold
        // for text that has no file at all.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        let generated = synthesized::generated_uri(&schema);
        assert!(!harness.analysis.is_own_code(&generated));
        assert!(
            harness
                .analysis
                .synthesized
                .is_generated(&UriId::from(generated.as_str()))
        );
        assert!(
            harness
                .published()
                .into_iter()
                .all(|(uri, _)| uri != generated),
            "a document with no file behind it must not publish diagnostics"
        );
    }

    #[test]
    fn rbs_this_crate_wrote_that_does_not_parse_declares_nothing_at_all() {
        // The two consumers of generated text fail *independently* and neither says so:
        // `Types::harvest` returns on a parse error and rubydex indexes what it can. A generator
        // with a bug would then declare a partial class nobody can explain, silently. Refusing
        // the whole document is the right blast radius, and the warning names the file.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        harness.synthesize(&schema, "class Story\n  def title: () ->\n", Vec::new());

        assert!(!harness.has("Story#title()"));
        assert!(
            harness.analysis.synthesized.is_empty(),
            "text that does not parse must take what the source declared before with it"
        );
    }

    #[test]
    fn rbs_this_crate_wrote_that_crashes_the_indexer_declares_nothing_at_all() {
        // The bulkhead's seventh route. The recovery is the one the branch above already wrote,
        // because the two failures have one blast radius: this generator's declarations, and
        // nobody else's. What is new is that the analysis thread is still here to have them —
        // `Synthesized::record` runs inline on it, on the settle path, which is every keystroke.
        //
        // Armed rather than provoked, and `indexer::SOURCE_INDEXES_TO_CRASH` says why: forty-
        // three RBS shapes were run through rubydex's RBS indexer looking for one that panics
        // and none does. The two routes a *user's* file takes have real Ruby fixtures.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        indexer::SOURCE_INDEXES_TO_CRASH.set(1);
        let (_, logged) = crate::testing::captured_logs(tracing::Level::WARN, || {
            harness.synthesize(
                &schema,
                "class Story\n  def title: () -> String\nend\n",
                Vec::new(),
            );
        });
        assert_eq!(
            indexer::SOURCE_INDEXES_TO_CRASH.replace(0),
            0,
            "the crash was armed and taken"
        );

        assert!(!harness.has("Story#title()"));
        assert!(
            harness.analysis.synthesized.is_empty(),
            "text that cannot be indexed takes what the source declared before with it"
        );
        assert!(logged.contains("crashed; it declares nothing"), "{logged}");
    }

    #[test]
    fn a_class_a_generated_document_declares_is_refused_by_rename() {
        // The existing guard covers this with nothing added. Renaming reads *every* definition of a
        // name and refuses unless all of them are somewhere ya-lsp is willing to edit — the case it
        // was written for is `class String` reopened beside Ruby's own signatures — and a generated
        // definition is not the user's own code by exactly the same test.
        //
        // The refusal is the safe answer and not a placeholder for a better one: the alternative
        // is a rename that reaches through the mapping and edits `db/schema.rb`, which is a
        // generated file whose column is not renamed by rewriting it.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        harness.open(&uri, source);

        let renamed = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, "Story"),
                "newName": "Article",
            }),
        );
        assert!(renamed.is_null(), "{renamed}");
        assert!(
            harness
                .messages()
                .iter()
                .any(|message| message.contains("Story")),
            "a refusal is said out loud"
        );
    }

    #[test]
    fn a_generated_declaration_in_the_symbol_picker_points_at_the_real_file() {
        // `workspace/symbol` reaches `locator::site` by a different route from goto-definition,
        // and a row an editor cannot open is worse than a row that is not there — which is the
        // sentence `hierarchy` already carries about `rubydex:built-in`. Both halves here, over
        // one fixture where the two columns differ in nothing but their mapping: the mapped one
        // is offered and points at the schema, the unmapped one is not offered at all.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        let mut rows = |query: &str| -> Vec<(String, String)> {
            let found = harness.ask("workspace/symbol", serde_json::json!({ "query": query }));
            found
                .as_array()
                .map(|symbols| {
                    symbols
                        .iter()
                        .filter_map(|symbol| {
                            Some((
                                symbol["name"].as_str()?.to_owned(),
                                symbol["location"]["uri"].as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        assert_eq!(
            rows("title"),
            vec![("title".to_owned(), schema.as_str().to_owned())]
        );
        assert!(
            rows("byline").is_empty(),
            "an unmapped row has nowhere to open"
        );
    }

    // ---------------------------------------------------------------------------------------
    // `db/schema.rb`
    // ---------------------------------------------------------------------------------------

    /// A Rails application, as small as one can be and still be one.
    ///
    /// Two tables and one model between them: `stories` is claimed, `widgets` is not, and the
    /// difference between them is the whole of what "the schema does not type more receivers,
    /// it makes the ones already typed answer" means in a fixture.
    const SCHEMA_RB: &str = "\
ActiveRecord::Schema[7.1].define(version: 2024_01_01_000000) do
  create_table \"stories\", force: :cascade do |t|
    t.string \"title\", null: false
    t.text \"description\"
    t.string \"tags\", default: [], array: true
  end

  create_table \"widgets\", force: :cascade do |t|
    t.string \"name\", null: false
  end
end
";

    fn rails_project(caller: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let schema = harness.write("db/schema.rb", SCHEMA_RB);
        let uri = harness.write("app/main.rb", caller);
        harness.index();
        harness.index_gems();
        (harness, schema, uri)
    }

    #[test]
    fn a_column_is_a_method_that_types_its_chain_and_jumps_to_the_schema() {
        // The schema's whole point in one expression. `Story` is found, `title` is a column,
        // and no `def title` exists anywhere in the repository. What has to happen is three things at once: the member is found, the chain
        // off it is typed, and the jump lands on the line of `db/schema.rb` that said so.
        let source = "Story.new.title.upcase\n";
        let (mut harness, schema, uri) = rails_project(source);

        assert!(harness.has("Story#title()"), "the column is not a member");

        let card = card(&mut harness, &uri, source, "upcase");
        assert!(card.contains("String#upcase"), "{card}");

        let definition = harness.definition_at(&uri, source, "title");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str()),
            "{definition}"
        );
        // `    t.string "title", null: false` on line 2, revealed whole, with the name selected.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(2),
                &serde_json::json!(4),
                &serde_json::json!(14),
            ),
            "{definition}"
        );
    }

    /// A line inserted above a column moves the jump and leaves the graph alone.
    ///
    /// **The whole claim of the text test, and both halves are needed to make it true.** A
    /// provenance comment names a file and the macro it read and never a line number, so an
    /// edit that pushes `t.string "title"` down a line re-derives RBS that is byte-identical
    /// and mappings that have all moved. Handing that to rubydex makes it drop the generated
    /// document, invalidate every declaration the old one touched and link it all again to
    /// arrive at the graph it already had: one keystroke in `app/models/user.rb` cost **940 ms**
    /// on discourse and 80 on lobsters, and the cost is a function of the workspace rather than
    /// of the file being edited.
    ///
    /// The counter is the only instrument that can say so, for the reason `Analysis::walks` is
    /// one: a graph rebuilt into exactly the shape it already had answers every question the
    /// same way. And the second assertion is why the mappings cannot simply be ignored too —
    /// what moved really did move, and a jump that lands one line high is worse than a slow one.
    #[test]
    fn an_edit_that_moves_a_declaration_without_changing_it_leaves_the_graph_alone() {
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = rails_project(source);

        let before = harness.definition_at(&uri, source, "title");
        assert_eq!(
            before[0]["targetRange"]["start"]["line"],
            serde_json::json!(2),
            "{before}"
        );
        let indexed = harness.analysis.synthesized.indexed();
        assert!(indexed > 0, "the schema declared something to begin with");

        // Opening it changes nothing at all, and then one blank first line puts every column
        // one line further down without changing a word any of them says.
        harness.open(&schema, SCHEMA_RB);
        harness.change(&schema, &format!("\n{SCHEMA_RB}"));

        assert_eq!(
            harness.analysis.synthesized.indexed(),
            indexed,
            "the RBS is byte-identical, so the graph has nothing to learn from it"
        );
        let after = harness.definition_at(&uri, source, "title");
        assert_eq!(
            after[0]["targetRange"]["start"]["line"],
            serde_json::json!(3),
            "and the jump still lands on the line that declares it: {after}"
        );
    }

    #[test]
    fn a_hover_on_a_column_says_which_file_and_which_table_it_came_from() {
        // What keeps this tier honest. A schema-derived answer looks exactly like
        // a resolved one on the fence line, so the card has to say where it came from — and it
        // says it as the declaration's *documentation*, which is how the RBS carries it. That
        // is why no module outside `workspace::rails` has to learn the word "table".
        let source = "Story.new.title\n";
        let (mut harness, _schema, uri) = rails_project(source);

        let card = card(&mut harness, &uri, source, "title");
        assert!(card.contains("Story#title"), "{card}");
        assert!(card.contains("db/schema.rb"), "{card}");
        assert!(card.contains("table"), "{card}");
        assert!(card.contains("stories"), "{card}");
        assert!(card.contains("string"), "{card}");
    }

    #[test]
    fn a_nullable_column_says_so_and_a_null_false_one_does_not() {
        // Roughly a third of a real application's columns can be `nil`, and RBS is the one
        // output format in reach that can say which. So the two columns are declared
        // differently — and because a hover card shows a name rather than a return type, the
        // provenance line is where a person sees it.
        let source = "Story.new.description.upcase\n";
        let (mut harness, _schema, uri) = rails_project(source);

        let nullable = card(&mut harness, &uri, source, "description");
        assert!(nullable.contains("may be `nil`"), "{nullable}");

        let stated = "Story.new.title\n";
        let other = harness.write("app/other.rb", stated);
        harness.watch(&[&other]);
        let stated = card(&mut harness, &other, stated, "title");
        assert!(stated.contains("`null: false`"), "{stated}");
        assert!(!stated.contains("may be `nil`"), "{stated}");

        // And the chain is typed either way: an optional return is still a `String` to whoever
        // asks what comes next, which is the same answer Ruby's own signatures give.
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(chained.contains("String#upcase"), "{chained}");
    }

    /// A second database's schema. Rails names it `db/<database>_schema.rb`.
    const ANIMALS_SCHEMA: &str = "\
ActiveRecord::Schema[8.0].define(version: 2024_01_01_000000) do
  create_table \"dogs\", force: :cascade do |t|
    t.string \"name\", null: false
  end
end
";

    #[test]
    fn a_second_database_s_schema_is_read_and_says_which_file_it_is() {
        // Rails has had several databases since 6.0, and a new Rails 8 application ships three
        // secondary schemas — solid_queue, solid_cache, solid_cable — before anybody writes a
        // line of it. lobsters, which every number in this section comes from, has three of its
        // own. Reading only `db/schema.rb` misses whole models silently.
        let source = "Dog.new.name\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let animals = harness.write("db/animals_schema.rb", ANIMALS_SCHEMA);
        let dog = harness.write("app/models/dog.rb", "class Dog\nend\n");
        harness.watch(&[&animals, &dog]);

        assert!(
            harness.has("Dog#name()"),
            "the second database was not read"
        );
        assert!(
            harness.has("Story#title()"),
            "and the first one stopped being"
        );
        assert_eq!(harness.analysis.synthesized.len(), 2);

        let definition = harness.definition_at(&uri, source, "name");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(animals.as_str()),
            "{definition}"
        );
        // The provenance line names the file it really came from. Naming `db/schema.rb` above a
        // column that is not in it would be a confidently wrong answer, which is the one thing
        // this half of the release is built not to give.
        let card = card(&mut harness, &uri, source, "name");
        assert!(card.contains("db/animals_schema.rb"), "{card}");
        assert!(!card.contains("`db/schema.rb`"), "{card}");
    }

    #[test]
    fn a_table_two_schemas_declare_is_declared_by_neither() {
        // The same rule as two classes claiming one table, for the same reason and one worse:
        // the model would answer with two schemas at once, and `Types::harvest` would keep
        // whichever column it read last — a type that depends on the order a `HashMap` iterates.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        assert!(harness.has("Story#title()"));

        let replica = harness.write("db/replica_schema.rb", SCHEMA_RB);
        harness.watch(&[&replica]);

        assert!(
            !harness.has("Story#title()"),
            "an ambiguous table answered anyway"
        );
        assert!(harness.analysis.synthesized.is_empty());
    }

    #[test]
    fn a_schema_that_stops_declaring_anything_takes_its_columns_with_it() {
        // A generator that reads several files gets no notification when one of them stops
        // having something to say — the file is still there, still indexed, still a schema. So
        // the pass that writes is also the pass that prunes.
        let (mut harness, _schema, _uri) = rails_project("Dog.new\n");
        let animals = harness.write("db/animals_schema.rb", ANIMALS_SCHEMA);
        let dog = harness.write("app/models/dog.rb", "class Dog\nend\n");
        harness.watch(&[&animals, &dog]);
        assert!(harness.has("Dog#name()"));

        harness.open(&animals, ANIMALS_SCHEMA);
        harness.change(
            &animals,
            "ActiveRecord::Schema[8.0].define(version: 0) do\nend\n",
        );

        assert!(
            !harness.has("Dog#name()"),
            "an emptied schema still answers"
        );
        assert!(
            harness.has("Story#title()"),
            "and it took the other one with it"
        );
        assert_eq!(harness.analysis.synthesized.len(), 1);
    }

    #[test]
    fn the_schema_pass_leaves_another_generator_s_work_where_it_is() {
        // What every generator after this one inherits. They generate from *model* files
        // through the same side
        // table, so a pass over `db/*schema.rb` that pruned by "everything I did not just
        // write" would delete their work on the way past. Which sources belong to which
        // generator is the caller's question, which is why `Synthesized::sources` hands back
        // sources rather than deciding.
        let source = "Story.new.author\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let model = DocUri::from_path(&harness.root.path().join("app/models/story.rb"))
            .expect("a file uri");
        let rbs = "class Story\n  def author: () -> String\nend\n";
        harness.synthesize(
            &model,
            rbs,
            vec![synthesized::Mapping {
                generated: span(rbs, "  def author: () -> String\n"),
                declared: Site {
                    uri: model.as_str().to_owned(),
                    full: (0, 11),
                    selection: (6, 11),
                },
            }],
        );

        // Both generators' documents, both still answering, after a settle that ran the schema
        // pass over a workspace whose only schema is `db/schema.rb`.
        assert_eq!(harness.analysis.synthesized.len(), 2);
        assert!(
            harness.has("Story#author()"),
            "another generator was pruned"
        );
        assert!(harness.has("Story#title()"));
        assert!(
            harness.definition_at(&uri, source, "author")[0]["targetUri"]
                .as_str()
                .is_some_and(|target| target.ends_with("app/models/story.rb")),
        );
    }

    // ---------------------------------------------------------------------------------------
    // `db/structure.sql`
    // ---------------------------------------------------------------------------------------

    /// The same two tables as [`SCHEMA_RB`], as pg_dump would have written them.
    ///
    /// Deliberately the same database, because the item's claim is that the *format* is the
    /// only difference — so the two fixtures declaring the same thing is the assertion, and a
    /// dump that happened to describe some other schema would hide it.
    const STRUCTURE_SQL: &str = "\
SET statement_timeout = 0;

CREATE TABLE public.stories (
    id bigint NOT NULL,
    title character varying NOT NULL,
    description text,
    tags character varying[]
);

CREATE TABLE public.widgets (
    id bigint NOT NULL,
    name character varying NOT NULL
);

CREATE INDEX index_stories_on_title ON public.stories USING btree (title);
";

    /// The same project, dumped as SQL instead of as Ruby.
    fn sql_project(caller: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/story.rb", "class Story\nend\n");
        let dump = harness.write("db/structure.sql", STRUCTURE_SQL);
        let uri = harness.write("app/main.rb", caller);
        harness.index();
        harness.index_gems();
        (harness, dump, uri)
    }

    #[test]
    fn a_dumped_column_is_a_method_that_types_its_chain_and_jumps_to_the_sql() {
        // The SQL reader's whole point, and it is the `.rb` schema's test against the other
        // file. An application that sets `schema_format = :sql` has **zero** column types
        // without it — a cliff rather than a gradient — and what has to happen is the same
        // three things at once: the member exists, the chain off it is
        // typed, and the jump lands on the line of SQL that said so.
        let source = "Story.new.title.upcase\n";
        let (mut harness, dump, uri) = sql_project(source);

        assert!(harness.has("Story#title()"), "the column is not a member");

        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(chained.contains("String#upcase"), "{chained}");

        let definition = harness.definition_at(&uri, source, "title");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(dump.as_str()),
            "{definition}"
        );
        // `    title character varying NOT NULL` on line 4, revealed whole, name selected.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(4),
                &serde_json::json!(4),
                &serde_json::json!(4),
            ),
            "{definition}"
        );

        // And the card says which file, in the dump's own vocabulary mapped to the schema's.
        let column = card(&mut harness, &uri, source, "title");
        assert!(column.contains("db/structure.sql"), "{column}");
        assert!(column.contains("stories"), "{column}");
        assert!(column.contains("`null: false`"), "{column}");
        let nullable = "Story.new.description\n";
        let other = harness.write("app/other.rb", nullable);
        harness.watch(&[&other]);
        let nullable = card(&mut harness, &other, nullable, "description");
        assert!(nullable.contains("may be `nil`"), "{nullable}");
    }

    #[test]
    fn an_array_column_answers_with_an_array_from_either_kind_of_schema() {
        // Both readers at once. Unread, `array: true` makes a Postgres array column answer its
        // *element* type — a wrong answer rather than an absent one, and wrong in the direction
        // that looks right:
        // `story.tags.upcase` resolved and `story.tags.join` did not. 39 such columns in three
        // of the six corpora's `schema.rb` alone, before the SQL side was counted.
        let source = "Story.new.tags.join\n";

        let (mut harness, _schema, uri) = rails_project(source);
        let ruby = card(&mut harness, &uri, source, "join");
        assert!(ruby.contains("Array#join"), "from schema.rb: {ruby}");

        let (mut harness, _dump, uri) = sql_project(source);
        let dumped = card(&mut harness, &uri, source, "join");
        assert!(
            dumped.contains("Array#join"),
            "from structure.sql: {dumped}"
        );
        // And the card on the column itself says there are many of them, because a hover shows
        // a name rather than a return type — the same argument as `null: false`.
        let column = card(&mut harness, &uri, source, "tags");
        assert!(column.contains("`string[]`"), "{column}");
    }

    #[test]
    fn a_schema_dump_is_never_a_document_in_the_graph() {
        // The property `make canary`'s file count depends on, and the reason the plumbing is a
        // watcher rather than the blanking hook a template gets: on every route this server
        // takes by itself — the cold walk, the watcher, the pass — rubydex never sees SQL, so
        // there is no parse error to file, no diagnostics to publish and no document to count.
        // The *generated* document exists, and it has no file behind it. (A client whose
        // document selector hands a `.sql` over on `didOpen` indexes it like any other buffer,
        // which is what every non-Ruby file does; no shipped client does — the extension's `LANGUAGES` is `ruby` and `erb`.)
        let (harness, dump, _uri) = sql_project("Story.new\n");
        assert!(
            !harness.analysis.indexed(&dump),
            "the dump was indexed as if it were Ruby"
        );
        assert!(harness.latest(&dump).is_none(), "the dump got diagnostics");
        assert_eq!(harness.analysis.synthesized.len(), 1);
    }

    #[test]
    fn a_table_a_dump_and_a_ruby_schema_both_declare_is_declared_by_neither() {
        // The two-schema rule reached by a second road. An application has one format or the other,
        // so this is a repository that switched and did not delete the old file — and the two
        // are then two sources for one table, which is the case where answering at all means
        // answering with whichever was read last.
        let (mut harness, _dump, _uri) = sql_project("Story.new\n");
        assert!(harness.has("Story#title()"));

        let schema = harness.write("db/schema.rb", SCHEMA_RB);
        harness.watch(&[&schema]);

        assert!(
            !harness.has("Story#title()"),
            "an ambiguous table answered anyway"
        );
    }

    #[test]
    fn a_dump_written_deleted_and_rewritten_on_disk_re_settles_each_time() {
        // `refresh`'s third outcome: a `.sql` is neither a document to re-index nor one to
        // forget, so the branch invalidates and indexes nothing. All three events go through it,
        // which is why it sits above the `is_file` test as well as above the index gate — a deleted
        // dump is pruned by `forget_stale` on the settle this triggers, and nothing else would ever
        // trigger one.
        let source = "Story.new.title\n";
        let (mut harness, _dump, _uri) = sql_project(source);
        let secondary = harness.root.path().join("db/animals_structure.sql");
        let dog = harness.write("app/models/dog.rb", "class Dog\nend\n");
        harness.watch(&[&dog]);

        // Written: Rails names every other database's dump `db/<database>_structure.sql`,
        // exactly as it names the Ruby one `db/<database>_schema.rb`.
        let animals = harness.write(
            "db/animals_structure.sql",
            "CREATE TABLE public.dogs (\n    name character varying NOT NULL\n);\n",
        );
        harness.watch(&[&animals]);
        assert!(harness.has("Dog#name()"), "a new dump was not read");
        assert_eq!(harness.analysis.synthesized.len(), 2);

        // Deleted. Nothing re-reads a file that is gone, so a column left behind here would
        // answer for the life of the process.
        std::fs::remove_file(&secondary).unwrap();
        harness.watch(&[&animals]);
        assert!(!harness.has("Dog#name()"), "a deleted dump still answers");
        assert!(
            harness.has("Story#title()"),
            "and it took the other with it"
        );
        assert_eq!(harness.analysis.synthesized.len(), 1);
    }

    #[test]
    fn a_dump_open_in_an_editor_is_read_from_the_buffer() {
        // `with_text` prefers the buffer, so a client whose document selector is wide enough to
        // hand a `.sql` over types the models it describes before it is saved. VS Code's is
        // not — `LANGUAGES` is `ruby` and `erb` — so in that editor the watcher above is the
        // whole story; this is the other half of the same accessor and it costs nothing to have
        // right.
        let source = "Story.new.title\n";
        let (mut harness, dump, _uri) = sql_project(source);
        assert!(harness.has("Story#title()"));

        harness.open(&dump, STRUCTURE_SQL);
        harness.change(
            &dump,
            "CREATE TABLE public.stories (\n    headline character varying NOT NULL\n);\n",
        );

        assert!(harness.has("Story#headline()"), "the buffer was not read");
        assert!(
            !harness.has("Story#title()"),
            "the column the buffer removed still answers"
        );
    }

    #[test]
    fn a_project_that_excluded_its_db_directory_reads_no_dump() {
        // The one switch this feature has, and it is the switch everything else has.
        // `index.include` cannot name a `.sql` however it is spelled, so "not indexed and not
        // read are the same sentence" cannot be the gate here — but `index.exclude` is a thing
        // the user said, and `Workspace::admits` is that half of `Workspace::indexes` asked on
        // its own.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\nenabled = false\n\n[index]\nexclude = [\"db/**/*\"]\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write("db/structure.sql", STRUCTURE_SQL);
        harness.index();

        assert!(
            !harness.has("Story#title()"),
            "an excluded directory was read anyway"
        );
    }

    // ---------------------------------------------------------------------------------------
    // The associations, and the relation
    // ---------------------------------------------------------------------------------------

    /// A Rails application with three models and every association shape that matters.
    ///
    /// `Story` has one of each; `Comment` is the element type two collections share, which is
    /// what makes "one relation class per element type" observable; `Tag` exists so that a
    /// `has_many :through` has an intermediate to find. `Ghost` is named by nothing and defined
    /// by nothing, which is the decline every wrong inflection ends at.
    fn models_project(caller: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  \
             belongs_to :user\n  \
             belongs_to :parent_story, class_name: \"Story\", optional: true\n  \
             belongs_to :owner, polymorphic: true\n  \
             belongs_to :ghost\n  \
             has_one :draft, class_name: \"Comment\"\n  \
             has_many :comments\n  \
             has_many :taggings\n  \
             has_many :tags, through: :taggings\n  \
             has_many :voters, through: :votes, source: :user\n  \
             scope :recent, -> { order(created_at: :desc) }\n\
             end\n",
        );
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\n  belongs_to :story\n  has_many :comments\nend\n",
        );
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.write("app/models/tag.rb", "class Tag < ApplicationRecord\nend\n");
        harness.write(
            "app/models/tagging.rb",
            "class Tagging < ApplicationRecord\nend\n",
        );
        let uri = harness.write("app/main.rb", caller);
        harness.index();
        harness.index_gems();
        (harness, story, uri)
    }

    #[test]
    fn a_belongs_to_is_a_member_that_types_its_chain_and_jumps_to_the_macro() {
        // A `belongs_to` in one expression, and the three things that have to happen at once
        // are the three a column's first test asks for: the member exists, the chain off it is typed,
        // and the jump lands on the macro that said so rather than anywhere in the class.
        let source = "Story.new.user\n";
        let (mut harness, story, uri) = models_project(source);

        assert!(
            harness.has("Story#user()"),
            "the association is not a member"
        );

        let definition = harness.definition_at(&uri, source, "user");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(story.as_str()),
            "{definition}"
        );
        // `  belongs_to :user` on line 1, revealed whole, with the name selected past its colon.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(1),
                &serde_json::json!(2),
                &serde_json::json!(14),
            ),
            "{definition}"
        );
    }

    #[test]
    fn what_an_association_declares_and_what_it_refuses_to() {
        // The options table, as behaviour. `class_name:` wins over the association's own name
        // and it is not a refinement — 32 of the corpus's 76 carry one, and without it
        // `parent_story` camelizes to a class no application has ever defined. `polymorphic:`
        // and a name the graph does not hold are the two declines, and they are declines rather
        // than guesses because the name rung would otherwise be asked and would also fail.
        let (harness, _story, _uri) = models_project("");

        assert!(harness.has("Story#user()"));
        assert!(
            harness.has("Story#parent_story()"),
            "class_name: was not read"
        );
        assert!(harness.has("Story#draft()"));
        assert!(harness.has("Story#comments()"));
        assert!(
            harness.has("Story#tags()"),
            "has_many :through was not read"
        );

        assert!(!harness.has("Story#owner()"), "polymorphic: must decline");
        assert!(
            !harness.has("Story#ghost()"),
            "a class nobody defines must decline"
        );
        assert!(
            !harness.has("Story#voters()"),
            "a through: naming no association on this class must decline"
        );
    }

    #[test]
    fn a_hover_on_an_association_says_which_file_and_which_class_it_came_from() {
        // The provenance rule, and the boundary it exists to hold: the card names the file, the
        // macro and the class it resolved to, and it does so because the *generated RBS* carries
        // a comment above the `def`. Nothing in `hover.rs` knows the word `belongs_to`.
        let source = "Story.new.parent_story\n";
        let (mut harness, _story, uri) = models_project(source);

        let card = card(&mut harness, &uri, source, "parent_story");
        assert!(card.contains("app/models/story.rb"), "{card}");
        assert!(card.contains("belongs_to :parent_story"), "{card}");
        assert!(card.contains("which is a `Story`"), "{card}");
    }

    #[test]
    fn optional_is_what_makes_a_belongs_to_nilable_and_a_has_one_always_is() {
        // The nullability half. Rails 5 made `belongs_to` non-`nil` by default, so the
        // presence of the option is what makes the member optional — and `has_one` is optional
        // whatever anyone writes, because nothing in the file says the other record exists.
        let (harness, _story, _uri) = models_project("");
        let rbs = harness.generated_rbs("app/models/story.rb");

        assert!(rbs.contains("def user: () -> User\n"), "{rbs}");
        assert!(rbs.contains("def parent_story: () -> Story?\n"), "{rbs}");
        assert!(rbs.contains("def draft: () -> Comment?\n"), "{rbs}");
    }

    #[test]
    fn a_collection_chains_through_a_relation_class_that_no_file_declares() {
        // The relation class's whole point. `story.comments` is a relation, `.first` is a
        // `Comment`, and
        // `.title` — hmm, `Comment` has no columns here, so the chain is checked one link
        // further along instead: `.first.story` is a `Story` again, which is the association
        // `belongs_to` wrote, reached through a class this pass invented.
        let source = "Story.new.comments.first.story\n";
        let (mut harness, _story, uri) = models_project(source);

        assert!(harness.has("Comment::Relation"));
        let card = card(&mut harness, &uri, source, "story");
        assert!(card.contains("Comment#story"), "{card}");
    }

    #[test]
    fn a_relation_is_one_class_per_element_type_and_is_not_a_place() {
        // The two clauses that bound a relation class. Two models declare `has_many
        // :comments`, and
        // between them they cost **one** `Comment::Relation` — 38 models cost 38 classes and not
        // 149. And nothing in it is a jump target: no line of anybody's code declares
        // `Comment::Relation#first`, so pointing at one of the two `has_many`s would be picking
        // an arbitrary half of a coin flip.
        let source = "Story.new.comments.first\n";
        let (mut harness, _story, uri) = models_project(source);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("Comment::Relation")
                .map_or(0, |definitions| definitions.len()),
            1,
            "one relation class per element type, whoever asked for it"
        );
        assert!(
            harness.definition_at(&uri, source, "first").is_null(),
            "a class this crate invented must not be a place a user is sent"
        );
    }

    #[test]
    fn a_relation_class_the_project_already_has_is_not_shadowed() {
        // The one way the relation classes can make an answer *worse* rather than merely absent. A
        // project that wrote its own `Comment::Relation` meant something by it, so the pass emits
        // nothing at all for that element type — the collection loses its type rather than the
        // user losing their class.
        let (mut harness, _story, _uri) = models_project("");
        let own = harness.write(
            "app/models/comment/relation.rb",
            "class Comment\n  class Relation\n    def own_method\n    end\n  end\nend\n",
        );
        harness.watch(&[&own]);

        assert!(harness.has("Comment::Relation#own_method()"));
        assert!(!harness.has("Comment::Relation#first()"));
        assert!(
            !harness.has("Story#comments()"),
            "an association whose relation was declined must decline too"
        );
    }

    /// The nesting walk, end to end, and the fixture is the point: both spellings exist.
    ///
    /// Rails resolves an association's class against the module nesting of the class the macro
    /// is written on — `Spree::LineItem` naming `Adjustment` tries `Spree::LineItem::Adjustment`,
    /// then `Spree::Adjustment`, and the bare `Adjustment` **last** — so a workspace holding
    /// both answers with the nested one. Taking the bare one is wrong at real sites, which is why
    /// the assertion is on the *place* rather than on whether an answer exists: where both
    /// classes declare a `total`, the wrong order sends the jump to the wrong file, silently.
    ///
    /// The `has_many` is the other half and it is not a repetition: `Model::collections` asks
    /// which relation classes are needed and `Model::signatures` asks what each member returns,
    /// and two different answers put a `Spree::Order` member behind an `Order::Relation`.
    #[test]
    fn an_association_names_the_class_the_nesting_reaches_and_not_the_bare_one() {
        let source =
            "Spree::LineItem.new.adjustment.total\nSpree::LineItem.new.orders.first.number\n";
        let (mut harness, _story, uri) = models_project(source);
        let bare_adjustment = harness.write(
            "app/models/adjustment.rb",
            "class Adjustment < ApplicationRecord\n  def total\n  end\nend\n",
        );
        let adjustment = harness.write(
            "app/models/spree/adjustment.rb",
            "module Spree\n  class Adjustment < ApplicationRecord\n    def total\n    end\n  \
             end\nend\n",
        );
        let bare_order = harness.write(
            "app/models/order.rb",
            "class Order < ApplicationRecord\n  def number\n  end\nend\n",
        );
        let order = harness.write(
            "app/models/spree/order.rb",
            "module Spree\n  class Order < ApplicationRecord\n    def number\n    end\n  end\nend\n",
        );
        let line_item = harness.write(
            "app/models/spree/line_item.rb",
            "module Spree\n  class LineItem < ApplicationRecord\n    belongs_to :adjustment\n    \
             has_many :orders\n  end\nend\n",
        );
        harness.watch(&[
            &bare_adjustment,
            &adjustment,
            &bare_order,
            &order,
            &line_item,
        ]);

        assert!(
            harness.has("Spree::LineItem#adjustment()") && harness.has("Spree::LineItem#orders()"),
            "both macros are members"
        );
        let singular = harness.definition_at(&uri, source, "total");
        assert_eq!(
            (singular.as_array().map(Vec::len), &singular[0]["targetUri"]),
            (Some(1), &serde_json::json!(adjustment.as_str())),
            "one place, and it is the nested class: {singular}"
        );
        let collection = harness.definition_at(&uri, source, "number");
        assert_eq!(
            (
                collection.as_array().map(Vec::len),
                &collection[0]["targetUri"]
            ),
            (Some(1), &serde_json::json!(order.as_str())),
            "and the relation is a relation of the same class: {collection}"
        );
    }

    #[test]
    fn a_model_answers_the_query_interface_on_its_own_class() {
        // `Comment::Relation` is the hard half; without a class side nothing declares `first`,
        // `where` or `find` on the model *itself*, so a chain can be followed and never
        // started. `Story.recent.first.user` works off a macro alone and `Story.first.user`
        // does not, which is a strange thing for a server to be able to say.
        let source = "Story.first.user\n";
        let (mut harness, _story, uri) = models_project(source);

        assert!(
            harness.has("Story::<Story>#first()"),
            "the query interface is on the singleton, where rubydex files `def self.`"
        );
        let started = card(&mut harness, &uri, source, "user");
        assert!(started.contains("Story#user"), "{started}");
        assert!(!started.contains("guessed from the name"), "{started}");

        // `where` hands back the relation, which is the half that makes the two sides one fact:
        // `Story.where(...)` and `Story.all.where(...)` are the same method reached two ways.
        let relation = "Story.where(id: 1).first.user\n";
        let chained = harness.write("app/chained.rb", relation);
        harness.watch(&[&chained]);
        let card = card(&mut harness, &chained, relation, "user");
        assert!(card.contains("Story#user"), "{card}");
    }

    /// An `enum` in one expression: the four names a value installs, and where each of them says
    /// it was declared.
    ///
    /// `story.published?` is a member, it hovers as `bool`, and the jump lands on the
    /// `published: 1` *inside* the call with the label selected, which is the side table's
    /// mapping at its best case. `Story.published` is the same fact on the class side, and it is a `scope`
    /// because Rails installs it by calling `klass.scope`.
    #[test]
    fn an_enum_declares_its_values_and_jumps_to_the_one_that_named_them() {
        let source = "Story.published.first.published?\n";
        let (mut harness, _story, _uri) = models_project(source);
        let model = "\
class Article < ApplicationRecord
  enum :status, { draft: 0, published: 1 }
end
";
        let article = harness.write("app/models/article.rb", model);
        let caller = "Article.new.published?\n";
        let uri = harness.write("app/reads.rb", caller);
        harness.watch(&[&article, &uri]);

        assert!(
            harness.has("Article#published?()"),
            "the value is not a member"
        );
        assert!(harness.has("Article#status()"));
        assert!(harness.has("Article::<Article>#statuses()"));
        assert!(harness.has("Article::<Article>#not_published()"));

        let value = card(&mut harness, &uri, caller, "published?");
        assert!(
            value.contains("`enum :status`, value `published`"),
            "{value}"
        );
        assert!(!value.contains("guessed from the name"), "{value}");

        // And it answers `bool`, which cannot be read off a hover card: no card in this crate
        // prints a return type, and `bool` is `true | false` — a union, which
        // `class_of` deliberately declines — so it is read off the document the pass wrote,
        // which is the text `Types::harvest` then reads.
        assert!(
            harness
                .analysis
                .synthesized
                .text(&article)
                .is_some_and(|rbs| rbs.contains("def published?: () -> bool")),
            "{:?}",
            harness.analysis.synthesized.text(&article)
        );

        let definition = harness.definition_at(&uri, caller, "published?");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(article.as_str()),
            "{definition}"
        );
        // Line 1 is the `enum` call; the target is `published: 1` and the selection is the label
        // — the whole of the call is *not* what a value method was declared by.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["end"]["character"],
            ),
            (
                &serde_json::json!(1),
                &serde_json::json!(28),
                &serde_json::json!(28),
                &serde_json::json!(37),
            ),
            "{definition}"
        );
    }

    /// An `enum` re-types the column it is stored in, and does it by the column standing down.
    ///
    /// The one place in this pass where a generator's output depends on another generator's
    /// *input*. `story.status` is the label — a `String` — and the column holds the integer it
    /// is stored as; the two declarations are in two different generated documents, so `Facts`'
    /// precedence can never see the pair and the schema has to decline. What that has to
    /// produce is **one** `Story#status` and a chain that reaches `String`.
    #[test]
    fn an_enum_re_types_the_column_it_is_stored_in() {
        let source = "Story.new.status.upcase\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let schema = harness.write(
            "db/schema.rb",
            "\
ActiveRecord::Schema[7.1].define(version: 2024_01_01_000000) do
  create_table \"stories\", force: :cascade do |t|
    t.string \"title\", null: false
    t.integer \"status\", default: 0, null: false
  end

  create_table \"widgets\", force: :cascade do |t|
    t.integer \"status\", default: 0, null: false
  end
end
",
        );
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  enum :status, { draft: 0 }\nend\n",
        );
        let widget = harness.write("app/models/widget.rb", "class Widget\nend\n");
        harness.watch(&[&schema, &story, &widget]);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("Story#status()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "a column and an enum of one name is one declaration, not an overload"
        );
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(
            chained.contains("String#upcase"),
            "the label is a String, not the integer it is stored as: {chained}"
        );
        // Scoped to the class that wrote the `enum`, so another table's `status` is untouched.
        let widget = card(&mut harness, &uri, source, "status");
        assert!(widget.contains("`enum :status`"), "{widget}");
        assert!(harness.has("Widget#status()"));
    }

    /// `attribute`'s precedence, which Rails documents and which is easy to get backwards.
    ///
    /// `attributes.rb` says a cast type "will override the type of existing attributes if
    /// needed" and that a call with no cast type keeps "the previously defined type" — so the
    /// two halves of this test are the two halves of that sentence. `price` is re-typed and the
    /// schema withdraws its column, which has to produce **one** `Story#price` and a chain that
    /// reaches the cast type rather than the storage. `note` names no type, so nothing is
    /// declared for it at all and the column is exactly where it was — which is both what Rails
    /// does and what the serializer gems require, since a call with no cast type is the shape
    /// their macros also have.
    #[test]
    fn an_attribute_re_types_the_column_it_overrides_and_defers_where_it_names_no_type() {
        let source = "Story.new.price.upcase\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let schema = harness.write(
            "db/schema.rb",
            "\
ActiveRecord::Schema[7.1].define(version: 2024_01_01_000000) do
  create_table \"stories\", force: :cascade do |t|
    t.integer \"price\", null: false
    t.string \"note\", null: false
  end
end
",
        );
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  attribute :price, :string\n  attribute :note\nend\n",
        );
        harness.watch(&[&schema, &story]);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("Story#price()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "a column an `attribute` re-types is one declaration, not an overload"
        );
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(
            chained.contains("String#upcase"),
            "the cast type, not the integer it is stored as: {chained}"
        );
        // The other half: an `attribute` with no cast type declares nothing, so the column is
        // the only declaration there is and it still types the chain.
        assert_eq!(
            harness
                .analysis
                .graph
                .get("Story#note()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "an `attribute` that names no type leaves the column exactly where it was"
        );
        let note = "Story.new.note.upcase\n";
        let reads = harness.write("app/reads.rb", note);
        harness.watch(&[&reads]);
        let chained = card(&mut harness, &reads, note, "upcase");
        assert!(
            chained.contains("String#upcase"),
            "the column still types the chain: {chained}"
        );
    }

    /// The long tail end to end: two families, and the two shapes the whole table has.
    ///
    /// A `store_accessor` key is `untyped` and is therefore a **name** and no more — the half of
    /// the long tail that is navigational, exactly as a `delegate` is — and a
    /// `class_attribute` is six
    /// members across both sides of the class, of which the predicate is the one thing a macro
    /// that says nothing about its type still types: `!!self.setting` is a `bool` whatever
    /// `setting` turns out to hold.
    #[test]
    fn a_long_tail_macro_is_a_member_on_both_sides_and_its_predicate_is_a_bool() {
        let source = "Story.new.setting?\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  class_attribute :setting\n  \
             store_accessor :description, :colour\nend\n",
        );
        harness.watch(&[&story]);
        harness.analysis.settle();

        // The type is pinned as text in `tail.rs`; a card does not print a return type, which
        // true of every card. What this asks is the other half: the member
        // reaches the editor and says which line of the user's own file declared it.
        let predicate = card(&mut harness, &uri, source, "setting?");
        assert_eq!(
            predicate,
            "```ruby\nStory#setting?\n```\n\n*From `app/models/story.rb`, \
             `class_attribute :setting`.*"
        );
        // Asked after the hover, because a completion fixture replaces the document's text and
        // the positions the hover was asked at are the old text's.
        let offered = harness.declarations_at(&uri, "Story.~\n");
        assert!(
            ["setting", "setting=", "setting?"]
                .iter()
                .all(|name| offered.contains(&(*name).to_owned())),
            "a class_attribute is three members on the class side too: {offered:?}"
        );
        let instance = harness.declarations_at(&uri, "Story.new.~\n");
        for name in [
            "setting",
            "setting=",
            "setting?",
            "colour",
            "colour=",
            "colour_changed?",
        ] {
            assert!(
                instance.contains(&name.to_owned()),
                "{name} is missing from {instance:?}"
            );
        }
    }

    /// A `serialize` re-types its column, and the class it names may be a **generic** one.
    ///
    /// The reason this is an end-to-end test and not a rendering one: `Array` and `Hash` are the
    /// two classes the corpus writes as a `type:` and both take type arguments in RBS. If
    /// rubydex's parser refused a bare one, `Synthesized::record`'s gate would throw away the
    /// *whole* generated document — every other member in the file with it — and every unit test
    /// in `tail.rs` would still pass, because none of them parses what it renders.
    #[test]
    fn a_serialize_re_types_its_column_with_a_class_that_takes_type_arguments() {
        let source = "Story.new.description.first\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  serialize :description, coder: YAML, type: Array\n\
             end\n",
        );
        harness.watch(&[&story]);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("Story#description()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "the column is withdrawn, so the serialized type is the only declaration"
        );
        assert!(
            harness.has("Story#title()"),
            "the rest of the document survived the generic"
        );
        let chained = card(&mut harness, &uri, source, "first");
        assert!(
            chained.contains("Array#first"),
            "a text column carrying YAML is an Array in Ruby and never the String the schema \
             says: {chained}"
        );
    }

    /// The long tail's phase two: an alias takes the type of the column it aliases, across two
    /// generated documents.
    ///
    /// The second consumer of `Facts::returns`, and the shorter of the two paths — one
    /// hop where a `delegate` takes two. `title` is declared into `db/schema.rb`'s document and
    /// the alias into the model's, in the same pass, with nothing resolved and nothing indexed.
    #[test]
    fn an_alias_attribute_takes_the_type_of_the_column_it_aliases() {
        let source = "Story.new.headline.upcase\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  alias_attribute :headline, :title\nend\n",
        );
        harness.watch(&[&story]);

        assert!(
            harness.has("Story#headline?()"),
            "the pattern set, not only the reader"
        );
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(
            chained.contains("String#upcase"),
            "the aliased column's own type: {chained}"
        );
    }

    /// The bundle answers on the settle it lands, and not on the settle after that.
    ///
    /// **A whole-graph lookup that answers about the previous settle.** `Context::framework` asks
    /// `Graph::get`, which reads the map `Resolver::resolve` builds — and this pass runs
    /// immediately *before* the resolve, so it was answering about the previous settle. Measured
    /// over lobsters at the moment it is asked: 4 declarations against 1,968 definitions on the
    /// cold settle, **2,431 against 174,919** on the settle the bundle lands, 144,299 only on
    /// the one after. `has_one_attached` therefore declared nothing until the user's next
    /// keystroke, which is invisible to every measurement that does not settle twice.
    ///
    /// `index_gems` settles exactly once, which is what makes this a test rather than a
    /// coincidence: that settle is the one a bundle macro has to be answered on.
    #[test]
    fn the_bundle_says_which_class_a_macro_names_on_the_settle_it_lands() {
        let (dir, _gem_home, env) = project_with_gem(
            "module ActiveStorage\n  module Attached\n    class One\n      def attach\n      \
             end\n    end\n  end\nend\n",
        );
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "Story.new.avatar.attach\n";
        let uri = harness.write("app/main.rb", source);
        harness.write(
            "app/models/story.rb",
            "class Story\n  has_one_attached :avatar\nend\n",
        );
        harness.index();
        assert!(
            !harness.has("Story#avatar()"),
            "before the bundle is in, the class the macro names is genuinely not there"
        );

        harness.index_gems();

        assert!(
            harness.has("Story#avatar()"),
            "and the settle the bundle lands on is the one that declares it"
        );
        let chained = card(&mut harness, &uri, source, "attach");
        assert!(
            chained.contains("ActiveStorage::Attached::One#attach"),
            "the chain runs on through the gem's own class: {chained}"
        );
    }

    /// Two settles over a workspace nobody touched write the same bytes.
    ///
    /// **The pass must never read a document it generated.** The graph holds the *previous*
    /// settle's generated documents at the moment it is read, so a lookup that could see one
    /// would answer differently on every pass and nothing in the output would say so. The
    /// filter is a property of the URI **scheme** rather than of a list, which is what a
    /// non-`file:` scheme is for; this asserts the property rather than the filter.
    #[test]
    fn two_settles_over_an_unchanged_workspace_generate_the_same_bytes() {
        let (mut harness, _schema, _uri) = rails_project("Story.new.title\n");
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\n  \
             delegate :name, to: :author\nend\n",
        );
        let comment = harness.write(
            "app/models/comment.rb",
            "module Reports::Registry\n  Row = Struct.new(:total)\nend\nclass Comment < \
             ApplicationRecord\n  belongs_to :story\nend\n",
        );
        harness.watch(&[&story, &comment]);
        let before = harness.every_generated_document();
        assert!(before.len() >= 3, "the fixture feeds several generators");

        harness.analysis.dirty = true;
        harness.analysis.settle();

        assert_eq!(
            harness.every_generated_document(),
            before,
            "a settle over an unchanged workspace is a settle that changes nothing"
        );
    }

    /// A namespace only a **gem** declares is one a generated name may be spelled under.
    ///
    /// `Namespaces::spellable` is where it lands: a joined `class Shouty::Thing::Point`
    /// introduces `Shouty`, which is safe exactly when
    /// something declares `Shouty` — and *something* has never meant *this application* except
    /// by accident of what `Context` was allowed to walk. What is **not** widened is which class
    /// a macro may name; `synthesized.md` has the six-corpus measurement that settles it.
    #[test]
    fn a_namespace_only_a_gem_declares_is_one_a_generated_name_may_hang_off() {
        let (dir, _gem_home, env) = project_with_gem("module Shouty\nend\n");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty::Thing::Point.new.x\n");
        harness.write(
            "app/models/thing.rb",
            "class Shouty::Thing\n  Point = Struct.new(:x)\nend\n",
        );
        harness.index();
        assert!(
            !harness.has("Shouty::Thing::Point#x()"),
            "with nothing declaring `Shouty`, the joined name would introduce it"
        );

        harness.index_gems();

        assert!(
            harness.has("Shouty::Thing::Point#x()"),
            "the gem declares `Shouty`, so the joined name introduces nothing"
        );
    }

    /// The attachment macros type a receiver only when the class they name is in the graph.
    ///
    /// `ActiveStorage::Attached::One` is nobody's application class and it is not under an
    /// engine's `app/` either — it is in activestorage's `lib/`, one directory from `Blob` and on
    /// the far side of the engine gate — so `Context::classes` can never hold it and the
    /// gate is a lookup rather than a projection. What this pins is that the lookup is what
    /// decides, and `tail.rs` pins the decline on its own.
    #[test]
    fn an_attachment_declares_only_when_the_class_it_names_is_in_the_graph() {
        let source = "Story.new.avatar.attach\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let attached = harness.write(
            "app/models/active_storage/attached/one.rb",
            "module ActiveStorage\n  module Attached\n    class One\n      def attach\n      \
             end\n    end\n  end\nend\n",
        );
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_one_attached :avatar\nend\n",
        );
        harness.watch(&[&attached, &story]);
        harness.analysis.settle();

        let chained = card(&mut harness, &uri, source, "attach");
        assert!(
            chained.contains("ActiveStorage::Attached::One#attach"),
            "the chain runs on through the gem's class: {chained}"
        );
        let offered = harness.declarations_at(&uri, "Story.new.~\n");
        assert!(
            offered.contains(&"avatar".to_owned()) && offered.contains(&"avatar=".to_owned()),
            "and both halves of the macro are members: {offered:?}"
        );
    }

    /// An `attribute` and a `def` of the same name are two places, and both are the user's own.
    ///
    /// The one position in 420,249 a tier sweep calls *worse*, pinned here because it is the
    /// instrument rather than the answer: forem's `ResponseTemplate` writes
    /// `attribute :user_identifier, :string` and a `def user_identifier` under it, so the card
    /// gains a second place and `sweep.py`'s `tier` reads any card containing "Defined in " as
    /// the name-based list. The answer got strictly better — the same declaration, now with the
    /// line that typed it named beside the line that wrote it.
    #[test]
    fn an_attribute_beside_a_def_of_the_same_name_is_two_places() {
        let source = "Story.new.nickname\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  attribute :nickname, :string\n\n               def nickname\n    \"x\"\n  end\nend\n",
        );
        harness.watch(&[&story]);

        let card = card(&mut harness, &uri, source, "nickname");
        assert!(card.contains("Defined in 2 places"), "{card}");
        assert!(card.contains("`attribute :nickname, :string`"), "{card}");
    }

    /// End to end: a constant assigned a `Struct.new` is a class with members.
    ///
    /// Three things at once, and the first is the one the whole item rests on: `Point` is a
    /// **constant assignment** to rubydex and a `class Point` to the RBS this pass writes, and
    /// the two are one constant — the same property a generated `module` rests on, reached
    /// from the other side. Then the member types the chain off it, and the jump lands on the `:x`
    /// that named it.
    #[test]
    fn a_struct_constant_is_a_class_whose_members_type_and_jump() {
        let source = "Point.new(1, 2).x\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let shapes = harness.write("app/models/shapes.rb", "Point = Struct.new(:x, :y)\n");
        harness.watch(&[&shapes]);

        assert!(harness.has("Point#x()"), "the member is not there");
        assert!(harness.has("Point#x=()"), "and neither is its writer");

        let card = card(&mut harness, &uri, source, "x");
        assert!(card.contains("Point#x"), "{card}");
        assert!(card.contains("`Struct.new(:x, :y)`"), "{card}");

        let definition = harness.definition_at(&uri, source, "x");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(shapes.as_str()),
            "the jump leaves the caller for the file that declared it"
        );
        assert_eq!(
            definition[0]["targetSelectionRange"],
            serde_json::json!({
                "start": {"line": 0, "character": 20},
                "end": {"line": 0, "character": 21}
            }),
            "and selects the `x` inside the `:x`"
        );
    }

    /// `Data.define`'s `with` hands the class back, which is the one return type it can chain on.
    ///
    /// Also the two halves of the shape rule in one project: a `Data` gets no writer, and a call
    /// assigned to a local rather than to a constant declares nothing at all — 94 of finding
    /// 45's 268 uses are that second shape.
    #[test]
    fn a_data_chains_through_with_and_a_local_declares_nothing() {
        let source = "Coord.new.with.north\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let shapes = harness.write(
            "app/models/shapes.rb",
            "Coord = Data.define(:north)\nanon = Struct.new(:hidden)\n",
        );
        harness.watch(&[&shapes]);

        assert!(!harness.has("Coord#north=()"), "a `Data` has no writers");
        assert!(
            harness.analysis.graph.get("Struct#hidden()").is_none(),
            "a call assigned to a local names no class"
        );

        let card = card(&mut harness, &uri, source, "north");
        assert!(card.contains("Coord#north"), "{card}");
        assert!(
            card.contains("Type derived through `Coord#with()`"),
            "the copy is what the chain was followed through: {card}"
        );
    }

    /// One document may open the same body twice, and `Synthesized::record` has to keep it.
    ///
    /// A file under a namespace **nothing declares** writes its members out one body per
    /// segment, so a file that declares on the module *and* on a class inside it opens
    /// `class Ns` twice — once for each. Reopening is legal RBS and the parse gate is where a
    /// mistake about that goes silent: it costs forem's whole `include` document, with every
    /// test still green.
    #[test]
    fn a_document_that_opens_one_body_twice_is_still_indexed() {
        let mut harness = Harness::new();
        let file = harness.write(
            "app/services/ns/admin.rb",
            "module Ns::Admin\n  # @return [String]\n  def self.label\n  end\n\n  \
             class Panel\n    # @return [Integer]\n    def size\n    end\n  end\nend\n",
        );
        harness.watch(&[&file]);

        let rbs = harness.generated_rbs("app/services/ns/admin.rb");
        assert_eq!(rbs.matches("module Ns::Admin\n").count(), 2, "{rbs}");
        assert!(harness.has("Ns::Admin::<Admin>#label()"), "{rbs}");
        assert!(harness.has("Ns::Admin::Panel#size()"), "{rbs}");
    }

    /// A model whose whole namespace the application declares is left joined, and still answers.
    ///
    /// The other half of the rule, and the half that is *not* a repair: the damage is done by
    /// a segment **nothing declares**, so a name every segment of which some file writes down is
    /// left joined exactly as it is written. Asserting that is what keeps the rule from spreading:
    /// a body per segment written unconditionally costs real positions on names like
    /// `Comment::Relation`.
    #[test]
    fn a_model_inside_a_module_is_left_joined_and_still_answers() {
        let source = "Admin.table_name_prefix\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.write(
            "app/models/admin.rb",
            "module Admin\n  def self.table_name_prefix\n    \"admin_\"\n  end\nend\n",
        );
        harness.write("app/models/admin/deep.rb", "module Admin::Deep\nend\n");
        harness.write(
            "app/models/admin/deep/setting.rb",
            "class Admin::Deep::Setting < ApplicationRecord\n  has_many :users\nend\n",
        );
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let rbs = harness.generated_rbs("app/models/admin/deep/setting.rb");
        assert!(
            rbs.starts_with("module Admin::Deep\nclass Setting\n"),
            "the parent is a module the application writes down: {rbs}"
        );
        assert!(harness.has("Admin::Deep::Setting#users()"), "{rbs}");
        assert!(
            harness.has("User::Relation"),
            "a relation class is named after the element and stays joined: {rbs}"
        );
        let card = card(&mut harness, &uri, source, "table_name_prefix");
        assert!(
            !card.contains("Matched on the method name alone"),
            "the module keeps its own singleton: {card}"
        );
    }

    /// A namespace nobody defines keeps its own members, **and** the struct under it declares.
    ///
    /// `module Reports::Registry` is a whole Rails application's spelling for a `Reports` that
    /// Zeitwerk conjures and no file writes. Declaring `class Reports::Registry::Metric` is the
    /// first thing in the graph to introduce `Reports`, and it costs `Reports::Registry` its
    /// **own** singleton members — measured over chatwoot as 12 positions that resolved before
    /// the declaration and fell to the name list with it.
    ///
    /// **The namespace is opened rather than the call declined**, so both halves are asserted
    /// here: the module still answers for itself, and `Metric#name` exists rather than being
    /// the price of that.
    #[test]
    fn a_struct_under_a_namespace_nobody_defines_declares_and_costs_nothing() {
        let source = "Reports::Registry.supported?(1)\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let registry_uri = harness.write(
            "app/services/reports/registry.rb",
            "module Reports::Registry\n  Metric = Data.define(:name)\n\n  def self.supported?(name)\n    name\n  end\nend\n",
        );
        let source_uri = harness.write(
            "app/services/reports/source.rb",
            "class Reports::Source\n  def go\n    Reports::Registry.supported?(1)\n  end\nend\n",
        );
        // A **third** file naming it, and it is load-bearing: with one reference the answer
        // survives the joined name and with two it does not, which is why this reproduced over
        // a corpus long before it reproduced here. Chatwoot's third is the spec.
        harness.write("spec/services/reports/registry_spec.rb", source);
        let uri = harness.write("app/main.rb", source);
        harness.index();

        assert!(
            harness
                .analysis
                .graph
                .get("Reports::Registry::Metric#name()")
                .is_some(),
            "a conjured namespace no longer costs the struct under it"
        );
        let _ = (&registry_uri, &source_uri);
        let card = card(&mut harness, &uri, source, "supported?");
        assert!(card.contains("Reports::Registry.supported?"), "{card}");
        // The tier and not the list length: this workspace holds exactly one `supported?`, so
        // a receiver that fails to resolve still names the right method — on the *name* rung,
        // with the footnote that says so. Over a corpus the same failure spells itself as a
        // candidate list, and asserting on the list is what made this look unreproducible here.
        assert!(
            !card.contains("Matched on the method name alone"),
            "the module keeps its own singleton: {card}"
        );
    }

    /// One file feeding two generators merges into one document, and both halves survive.
    ///
    /// Worth asking of every pair of generators: a rank collision is resolved before
    /// render, so a bug there is a duplicate `def` that only shows up when two generators meet.
    /// The struct and the schema are the pair worth asking, because they are the two that
    /// declare a *typed* member on a class the other has never heard of.
    #[test]
    fn a_file_that_writes_a_macro_and_a_struct_declares_both() {
        let source = "Story.new.title\nPoint.new(1).x\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :stories\nend\nPoint = Struct.new(:x)\n",
        );
        harness.watch(&[&story]);

        assert!(harness.has("Point#x()"), "the struct half");
        assert!(harness.has("Story#stories()"), "the macro half");
        assert!(
            harness.has("Story#title()"),
            "and the schema's, in another document"
        );

        // Two documents, two sentences: neither generator's provenance leaks into the other's.
        let column = card(&mut harness, &uri, source, "title");
        assert!(column.contains("db/schema.rb"), "{column}");
        let member = card(&mut harness, &uri, source, "x");
        assert!(member.contains("`Struct.new(:x)`"), "{member}");
    }

    /// The host test end to end: the same macro in three bodies, and only two of them mean it.
    ///
    /// `active_model_serializers` spells `has_many`, `has_one` and `belongs_to`, stores an
    /// `Attribute` and defines **no method** — a serializer answers `respond_to?` false for
    /// every name its own macro wrote — and `has_many :comments` is byte-identical in a model
    /// and in one, so there is no shape to gate on and the host has to be asked. This runs the
    /// whole pass so that `Context::models` is what answers, rather than a set a unit test
    /// handed in.
    #[test]
    fn a_serializer_writing_an_association_macro_declares_nothing() {
        let source = "Story.new.comments\n";
        let (mut harness, _story, _uri) = models_project(source);
        let serializer = harness.write(
            "app/serializers/story_serializer.rb",
            "class StorySerializer < ActiveModel::Serializer\n  \
             has_many :comments\n  belongs_to :user\nend\n",
        );
        harness.watch(&[&serializer]);

        assert!(
            harness.has("Story#comments()"),
            "the model still declares its own"
        );
        assert!(
            !harness.has("StorySerializer#comments()"),
            "and the serializer declares nothing"
        );
        assert!(
            !harness.has("StorySerializer#user()"),
            "for the singular macros as well as the collection"
        );
    }

    /// The same rule, reached from a body that is not a serializer at all.
    ///
    /// Solidus' `Spree::Admin::ResourceController` defines its own class-side `belongs_to` for
    /// nested-resource routing and defines no method either. A blocklist of serializer names
    /// would have to be told about it; an admit list already declines it, because a controller
    /// is neither a model nor a module.
    #[test]
    fn a_controller_writing_an_association_macro_declares_nothing() {
        let source = "Story.new.comments\n";
        let (mut harness, _story, _uri) = models_project(source);
        let controller = harness.write(
            "app/controllers/admin/stories_controller.rb",
            "class Admin::StoriesController < ApplicationController\n  \
             belongs_to :story\nend\n",
        );
        harness.watch(&[&controller]);

        assert!(
            !harness.has("Admin::StoriesController#story()"),
            "a controller is not a macro host"
        );
    }

    /// The union half, end to end: a model whose base class this pass cannot see is still one.
    ///
    /// `Tag`'s superclass lives in a gem's `lib/`, which `Context::models`' walk does not reach,
    /// so inheritance says nothing about it. `Story` says `has_many :tags`, which makes `Tag` a
    /// collection element, and that is the other half of the union the host test asks. Both of forem's
    /// two gem-rooted models are this shape and both would have lost their macros to a narrower
    /// rule.
    #[test]
    fn a_model_rooted_in_a_gem_keeps_its_macros() {
        let source = "Story.new.tags\n";
        let (mut harness, _story, _uri) = models_project(source);
        let tag = harness.write(
            "app/models/tag.rb",
            "class Tag < ActsAsTaggableOn::Tag\n  belongs_to :user\nend\n",
        );
        harness.watch(&[&tag]);

        assert!(
            harness.has("Tag#user()"),
            "the collection half of the union is what keeps this one"
        );
    }

    /// The property every concern body rests on, recorded as a test.
    ///
    /// A concern's macros are declared on the **module** and the `include` the user already
    /// wrote carries them: an RBS `module Storyish` and a Ruby `module Storyish`
    /// are one constant to rubydex, `find_member_in_ancestors` crosses the `include`, the
    /// member types a receiver in a class that never mentions it, and the chain continues
    /// through it. Nothing about resolution was added for any of that.
    #[test]
    fn a_concerns_macros_reach_every_class_that_includes_it() {
        let source = "Spiked.new.notes.first.story\n";
        let (mut harness, _story, uri) = models_project(source);
        let concern = harness.write(
            "app/models/concerns/storyish.rb",
            "\
module Storyish
  extend ActiveSupport::Concern

  included do
    has_many :notes, class_name: \"Comment\"
  end
end
",
        );
        let includer = harness.write(
            "app/models/spiked.rb",
            "class Spiked < ApplicationRecord\n  include Storyish\nend\n",
        );
        harness.watch(&[&concern, &includer]);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("Storyish#notes()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "the member hangs on the module, once, not on any class"
        );
        assert!(
            !harness.has("Spiked#notes()"),
            "and it is not copied onto the includer"
        );
        let member = card(&mut harness, &uri, source, "notes");
        assert!(member.contains("Storyish#notes"), "{member}");
        assert!(
            member.contains("`has_many :notes`"),
            "the provenance names the macro in the concern: {member}"
        );
        assert!(!member.contains("guessed from the name"), "{member}");

        // …and the chain runs through it, so a concern's collection is worth exactly what a
        // class's is.
        let chained = card(&mut harness, &uri, source, "story");
        assert!(chained.contains("Comment#story"), "{chained}");

        // The corpus' dominant spelling is a concern nested under the class it is written for
        // — mastodon's `module Account::Interactions`, included by `class Account` — so the
        // generated `module` has a class in its own path. RBS holds that as happily as Ruby does.
        let nested = "Story.new.followers.first.story\n";
        let under = harness.write(
            "app/models/concerns/story/interactions.rb",
            "\
module Story::Interactions
  extend ActiveSupport::Concern

  included do
    has_many :followers, class_name: \"Comment\"
  end
end
",
        );
        let inside = harness.write("app/inside.rb", nested);
        let opened = harness.write(
            "app/models/story_opened.rb",
            "class Story\n  include Story::Interactions\nend\n",
        );
        harness.watch(&[&under, &inside, &opened]);
        assert!(harness.has("Story::Interactions#followers()"));
        let through = card(&mut harness, &inside, nested, "story");
        assert!(through.contains("Comment#story"), "{through}");

        // The jump lands on the macro in the concern, which is what the side table is for.
        let definition = harness.definition_at(&uri, source, "notes");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(concern.as_str()),
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetRange"]["start"]["line"],
            serde_json::json!(4),
            "{definition}"
        );
    }

    /// The two halves of a concern a module cannot own — one fanned out to the includers, the
    /// other declared nowhere.
    ///
    /// A `scope` in a concern is a class method of the *includer*, so the module owns nothing
    /// and `Storyish.recent` must still answer nothing — it raises in Ruby. What the fan-out
    /// adds is the other end: `Spiked.recent` is real, returns a relation of `Spiked`, and is
    /// written into the *concern's* document so the jump lands on the one `scope` line.
    ///
    /// An `enum` is not fanned out with it, and the reason is not the same as the reason it was
    /// declined in the first place. mastodon's `Status::Visibility` declares `enum :visibility`
    /// and `statuses.visibility` is an `integer` column, so declaring the label would put a
    /// `String` and an `Integer` for one member into two generated documents with nothing able
    /// to see the pair, which is the defect the withdrawal exists to prevent. The column an
    /// `enum` re-types is a
    /// question about a *table*, and a concern claims none; that is still true with the
    /// includers in hand, because a concern included by two models re-types a column in each.
    /// It is **one call in six corpora**.
    #[test]
    fn what_a_concern_may_not_declare_it_declares_nowhere() {
        let (mut harness, _story, _uri) = models_project("");
        let concern = harness.write(
            "app/models/concerns/storyish.rb",
            "\
module Storyish
  included do
    scope :recent, -> { order(created_at: :desc) }
    enum :status, { draft: 0 }
    has_many :comments
  end
end
",
        );
        let includer = harness.write(
            "app/models/spiked.rb",
            "class Spiked < ApplicationRecord\n  include Storyish\nend\n",
        );
        harness.watch(&[&concern, &includer]);

        assert!(
            harness.has("Storyish#comments()"),
            "the instance half is still declared"
        );
        assert!(
            harness.has("Spiked::<Spiked>#recent()"),
            "the class side lands on the includer"
        );
        for absent in [
            "Storyish::<Storyish>#recent()",
            "Storyish#status()",
            "Spiked#status()",
            "Storyish#draft?()",
            "Spiked::<Spiked>#draft()",
            "Storyish::Relation#first()",
        ] {
            assert!(!harness.has(absent), "{absent} was declared");
        }
    }

    /// The includer fan-out in one expression: two includers, two relation types, one line.
    #[test]
    fn a_concerns_scope_is_a_class_method_of_every_class_that_includes_it() {
        // `scope :expired` in `Expireable` is `Poll.expired` **and** `Invite.expired`, and the
        // two answers are two different types — which is the whole reason a module body reads
        // the macro and writes nothing. The declaration goes into the *concern's* generated
        // document, once per pair, so both jumps land on the one `scope` line that said so and
        // no bookkeeping is added anywhere.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let concern = harness.write(
            "app/models/concerns/expireable.rb",
            "\
module Expireable
  extend ActiveSupport::Concern

  included do
    scope :expired, -> { where(\"expires_at < ?\", Time.now) }
  end
end
",
        );
        harness.write(
            "app/models/poll.rb",
            "class Poll < ApplicationRecord\n  include Expireable\nend\n",
        );
        harness.write(
            "app/models/invite.rb",
            "class Invite < ApplicationRecord\n  include Expireable\nend\n",
        );
        // Parenthesised on the first line only, so that each needle picks out one call.
        let source = "Poll.expired()\nInvite.expired\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // The two types, from the text: one member declared twice, once per includer, in the
        // concern's own generated document. No hover card prints a return type, and the RBS is
        // where the two `Relation`s are visible at all.
        let rbs = harness.generated_rbs("app/models/concerns/expireable.rb");
        assert!(
            rbs.contains("def self.expired: (*untyped) -> Poll::Relation"),
            "{rbs}"
        );
        assert!(
            rbs.contains("def self.expired: (*untyped) -> Invite::Relation"),
            "{rbs}"
        );

        let poll = card(&mut harness, &uri, source, "expired()");
        assert!(poll.contains("Poll.expired"), "{poll}");
        assert!(poll.contains("which `Poll` includes"), "{poll}");
        let invite = card(&mut harness, &uri, source, "expired\n");
        assert!(invite.contains("Invite.expired"), "{invite}");
        assert!(invite.contains("which `Invite` includes"), "{invite}");

        // Both jump to the one `scope` line — line 4, the only line in the file that declares
        // anything — and neither lands in the model that includes the concern.
        for needle in ["expired()", "expired\n"] {
            let definition = harness.definition_at(&uri, source, needle);
            assert_eq!(
                definition[0]["targetUri"],
                serde_json::json!(concern.as_str()),
                "{definition}"
            );
            assert_eq!(
                definition[0]["targetRange"]["start"]["line"],
                serde_json::json!(4),
                "{definition}"
            );
        }
        assert!(harness.has("Poll::<Poll>#expired()"));
        assert!(harness.has("Invite::<Invite>#expired()"));
    }

    /// The three declines, and the one that is a closure rather than a decline.
    #[test]
    fn which_classes_a_concerns_scope_reaches_and_which_it_does_not() {
        // A concern nobody includes declares nothing **and does not fall back to itself** —
        // `Orphan.forgotten` raises in Ruby, and the argument against writing it on the
        // module's own singleton is unchanged by having the includers in hand.
        //
        // An `include` naming a constant the application does not define resolves to nothing,
        // which is the decline every reader in this pass makes: `include Sidekiq::Worker` is a
        // real `include` of a class this workspace has never seen.
        //
        // A class that is not a model and owns no collection declines too, and the gate is
        // `relations` rather than a rule of its own: a `scope` fanned onto a PORO would need a
        // `Plain::Relation`, which is a class with no table behind it.
        //
        // And the closure. `ActiveSupport::Concern` hands an inner concern's `included` block
        // to whatever includes the outer one, so `Deep.forgotten` is real through two hops. No
        // corpus writes one, so this test is the only evidence for that property.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/concerns/orphan.rb",
            "module Orphan\n  included do\n    scope :forgotten, -> { all }\n  end\nend\n",
        );
        harness.write(
            "app/models/concerns/bigger.rb",
            "module Bigger\n  include Orphan\nend\n",
        );
        harness.write(
            "app/models/plain.rb",
            "class Plain\n  include Orphan\nend\n",
        );
        // An `include` that resolves to a **class** is not an edge: only a module can be
        // included, and a name that resolves to one of the application's classes says nothing
        // about where a concern's macros land.
        harness.write("app/models/widget.rb", "class Widget\nend\n");
        harness.write(
            "app/models/boxed.rb",
            "class Boxed < ApplicationRecord\n  include Widget\nend\n",
        );
        harness.write(
            "app/models/stranger.rb",
            "class Stranger < ApplicationRecord\n  include Sidekiq::Worker\nend\n",
        );
        // A module that includes itself is a `NoMethodError` at run time and an infinite loop
        // in a closure, and this walks the text rather than the run.
        harness.write(
            "app/models/concerns/knot.rb",
            "module Knot\n  include Knot\n  included do\n    scope :tied, -> { all }\n  end\nend\n",
        );
        harness.index();

        for absent in [
            "Orphan::<Orphan>#forgotten()",
            "Bigger::<Bigger>#forgotten()",
            "Plain::<Plain>#forgotten()",
            "Plain::Relation#first()",
        ] {
            assert!(!harness.has(absent), "{absent} was declared");
        }

        // Now a model two hops down, and nothing else changes.
        let deep = harness.write(
            "app/models/deep.rb",
            "class Deep < ApplicationRecord\n  include Bigger\nend\n",
        );
        harness.watch(&[&deep]);
        assert!(
            harness.has("Deep::<Deep>#forgotten()"),
            "a concern reached through another concern still lands"
        );
        assert!(!harness.has("Bigger::<Bigger>#forgotten()"));
    }

    /// The collision worth arguing before believing, and the argument is that **both are
    /// real**.
    #[test]
    fn a_scope_written_in_both_a_concern_and_its_includer_is_two_places_and_one_type() {
        // `include Expireable` runs `included do … scope :recent … end` on `Poll` and `scope
        // :recent` in `Poll`'s own body runs on `Poll` too: two lines of Ruby, both of which
        // really do install `Poll.recent`, and Ruby keeps whichever ran last. Neither is a
        // guess and neither can be preferred by any evidence a file states, so both stand —
        // and they *may* stand, because the two declarations agree about the type by
        // construction. A `scope` returns a relation of the class it is installed on, and item
        // 27 installs the concern's on the includer, which is the same class.
        //
        // That is the condition, and it is narrower than "two generators collided": where two
        // documents disagree about a type the rank has to be spent by the loser declining —
        // `Source::outranks`, or the column-versus-`enum` withdrawal. Here
        // there is nothing to decide, so the honest card is the one that names both lines.
        //
        // It measures **0** over six corpora; this test is the only place it happens.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/concerns/expireable.rb",
            "module Expireable\n  included do\n    scope :recent, -> { all }\n  end\nend\n",
        );
        harness.write(
            "app/models/poll.rb",
            "class Poll < ApplicationRecord\n  \
             include Expireable\n  \
             scope :recent, -> { order(:id) }\n\
             end\n",
        );
        let source = "Poll.recent.first\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let recent = card(&mut harness, &uri, source, "recent");
        assert!(
            recent.contains("Defined in 2 places"),
            "two places, and the card says so: {recent}"
        );
        assert!(recent.contains("which `Poll` includes"), "{recent}");
        assert!(
            recent.contains("`app/models/poll.rb`, `scope :recent`"),
            "{recent}"
        );

        // One type, and the chain is what proves it: two declarations of one member disagreeing
        // about what they return is the thing the rank exists to prevent, and here they cannot.
        let first = card(&mut harness, &uri, source, "first");
        assert!(first.contains("ActiveRecordRelation#first"), "{first}");
    }

    /// A workspace shaped like the half of Rails the concern edge is about: a concern whose class-side
    /// methods reach an includer through an `extend` no file writes.
    ///
    /// Two spellings of the convention and one module that is not it, because the gate has to
    /// be the nested `module ClassMethods` rather than `extend ActiveSupport::Concern` — 6 of
    /// the 17 such modules in six corpora hand-roll the hook and 3 write neither.
    const CONCERNS: &str = "\
module ActiveSupport
  module Concern
  end
end

module ActiveModel
  module Validations
    extend ActiveSupport::Concern

    module ClassMethods
      def validates(*names)
      end
    end
  end
end

module ActiveRecord
  module Scoping
    module Named
      extend ActiveSupport::Concern

      module ClassMethods
        def scope(name, body)
        end
      end
    end
  end

  module Associations
    extend ActiveSupport::Concern

    module ClassMethods
      def belongs_to(name, options = nil)
      end
    end
  end
end

module Plain
  def helper
  end
end

module Odd
  ClassMethods = 5
end

class ApplicationRecord
  include ActiveModel::Validations
  include ActiveRecord::Scoping::Named
  include ActiveRecord::Associations
  include Recountable
  include Countable
  include Plain
  include Odd
end
";

    /// The application's **own** concern, written where Rails puts one and installing its
    /// `ClassMethods` by hand rather than through `ActiveSupport::Concern` — 6 of the corpus'
    /// 17 do exactly this, and a gate on the `extend` would decline every one of them.
    const OWN_CONCERN: &str = "\
module Countable
  def self.included(base)
    base.extend(ClassMethods)
  end

  module ClassMethods
    LIMIT = 100

    def counts_by(column)
    end
  end
end

module Recountable
  def self.included(base)
    base.extend(ClassMethods)
  end

  module ClassMethods
    def counts_by(column)
    end
  end
end
";

    #[test]
    fn a_class_body_reaches_the_class_methods_of_every_concern_it_includes() {
        // The edge is walked at resolution rather than declared, because the
        // class it would have to be declared on is a gem's. Both spellings of the convention
        // land, `Plain#helper` does not — a module with no nested `ClassMethods` is not
        // extended onto anything — and every one of these was on the name rung before.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        // The `include` naming nothing puts an `Ancestor::Partial` in the chain, which the walk
        // has to step over rather than stop at — a class that includes one unresolvable module
        // still reaches every concern under it.
        let source = "\
class Story < ApplicationRecord
  include Nowhere::AtAll
  validates :title
  scope :recent, -> { all }
  belongs_to :author
  counts_by :author
  helper
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        for (needle, owner) in [
            (
                "validates",
                "ActiveModel::Validations::ClassMethods#validates",
            ),
            ("scope", "ActiveRecord::Scoping::Named::ClassMethods#scope"),
            (
                "belongs_to",
                "ActiveRecord::Associations::ClassMethods#belongs_to",
            ),
            ("counts_by", "Countable::ClassMethods#counts_by"),
        ] {
            let found = card(&mut harness, &uri, source, needle);
            assert!(found.contains(owner), "{found}");
            assert!(
                !found.contains("Matched on the method name alone"),
                "{needle} is resolved, not guessed: {found}"
            );
        }

        let guessed = card(&mut harness, &uri, source, "helper");
        assert!(
            guessed.contains("Matched on the method name alone"),
            "a module with no nested ClassMethods extends nothing: {guessed}"
        );
    }

    /// A constant that holds an *object* is not a class object.
    ///
    /// Upstream promotes a constant used as a receiver into a `Namespace::Todo` — rubydex's
    /// spelling for "a namespace I never saw a definition of" — which has a singleton class
    /// whose ancestors are `Class`, `Module` and `Object`. `ENV: RBS::Unnamed::ENVClass` and
    /// `URI::RFC2396_PARSER: URI::RFC2396_Parser` are both that shape, and completing against
    /// the singleton answered `alias_method` and `attr_accessor` for `ENV.` — precisely, wrongly,
    /// and *instead of* the name-based list, which had been answering. Fifteen lobsters
    /// positions, fourteen of them `ENV.fetch`.
    ///
    /// The fixture is `ENV`'s shape written out, and what it pins is both halves: the word the
    /// name rung finds is offered, and the singleton's own members are not.
    #[test]
    fn a_constant_that_holds_an_object_is_not_a_class_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/s.rbs"),
            "module Vault
  module Store
    def unlock: () -> String
  end
end

             HOLDER: Vault::Store
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write(
            "app/main.rb",
            "HOLDER.unlock
",
        );
        harness.index();
        harness.index_gems();

        let offered = harness.complete(&uri, "HOLDER.unl~");
        let labels: Vec<&str> = offered["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .collect();
        assert!(
            labels.contains(&"unlock"),
            "the name rung answers where the type cannot: {labels:?}"
        );
        let singleton_only = harness.complete(&uri, "HOLDER.attr_acc~");
        let wrong: Vec<&str> = singleton_only["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .filter(|label| *label == "attr_accessor")
            .collect();
        assert!(
            wrong.is_empty(),
            "a constant holding an object is not a Module: {wrong:?}"
        );
    }

    #[test]
    fn an_extend_is_read_wherever_it_is_written() {
        // `extend` is not unread: rubydex indexes it and attaches it to the singleton class
        // exactly as Ruby does. What
        // it loses is **one shape** — an `extend` in an `.rbs` whose module name is qualified —
        // and four probes over one workspace are what isolated it: Ruby resolves `extend Flat`,
        // `extend Ns::Fmt` and `include Ns::Fmt`; RBS resolves `extend Flat` and
        // `include Ns::Fmt`; only RBS's `extend Ns::Fmt` does not.
        //
        // The fixture is `stdlib/securerandom/0/securerandom.rbs` written out, because that is
        // the call the report made — and 15 of the 22 `extend`s in the vendored signatures are
        // qualified, so this is the commonest of them rather than the only one.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/s.rbs"),
            "module Random\n  module Formatter\n    def hex: (?Integer) -> String\n  end\nend\n\n\
             module Joined::Deep\n  def joined: () -> String\nend\n\n\
             module Flat\n  def flat: () -> String\nend\n\n\
             module SecureRandom\n  extend Random::Formatter\nend\n\n\
             module JoinExt\n  extend Joined::Deep\nend\n\n\
             module FlatExt\n  extend Flat\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        // The same edge written in Ruby, which rubydex resolves on its own and must keep doing:
        // the repair must never be the only thing answering a shape rubydex already handles.
        harness.write(
            "app/rb.rb",
            "module Ns\n  module Fmt\n    def in_ruby\n    end\n  end\nend\n\n\
             class Written\n  extend Ns::Fmt\nend\n",
        );
        let source = "SecureRandom.hex\nJoinExt.joined\nFlatExt.flat\nWritten.in_ruby\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        for (needle, owner) in [
            ("hex", "Random::Formatter#hex"),
            ("joined", "Joined::Deep#joined"),
            ("flat", "Flat#flat"),
            ("in_ruby", "Ns::Fmt#in_ruby"),
        ] {
            let found = card(&mut harness, &uri, source, needle);
            assert!(found.contains(owner), "{needle}: {found}");
            assert!(
                !found.contains("possible definitions"),
                "{needle} is still a candidate list: {found}"
            );
            assert!(
                !found.contains("Matched on the method name alone"),
                "{needle} resolves rather than guessing: {found}"
            );
        }

        // The completion side of the same edge. Resolution takes the first answer and
        // completion collects them all, so the walk is shared and
        // the list is asked here as well as the card.
        let offered = harness.complete(&uri, "SecureRandom.he~");
        let labels: Vec<&str> = offered["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .collect();
        assert!(labels.contains(&"hex"), "{labels:?}");
    }

    #[test]
    fn a_def_written_inside_a_block_does_not_shadow_a_concerns_class_method() {
        // The question is answered before the fixture: **a block-owned `def` cannot be told
        // from a true top-level one.** rubydex's nesting stack
        // holds lexical scopes, `Class.new`/`Module.new` owners and methods, and a `describe
        // "x" do` pushes none of the three — so a `def` inside one is recorded exactly as a
        // `def` at the true top level, a private method of `Object`. There is no flag, no
        // variant and no nesting id that separates them, so declining to treat such a `def` as
        // `Object`'s is closed at the graph level.
        //
        // What is left is the **order**, and it was wrong rather than approximate: a module
        // `extend`ed onto a class object sits above `Class`, `Module` and `Object` in the
        // singleton chain, so the concern edge has to be reached before them. On
        // discourse this cost seven RSpec helpers' worth of `validate` in every model.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "spec/models/story_spec.rb",
            "describe \"the counter\" do\n  def counts_by(column)\n  end\nend\n",
        );
        let source = "\
class Story < ApplicationRecord
  counts_by :author
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let found = card(&mut harness, &uri, source, "counts_by");
        assert!(
            found.contains("Countable::ClassMethods#counts_by"),
            "the concern is above `Object` in the singleton chain: {found}"
        );
        assert!(
            !found.contains("Object#counts_by"),
            "and the spec helper is not a method of `Object` at all: {found}"
        );
    }

    #[test]
    fn a_top_level_def_still_answers_for_a_class_body_below_it() {
        // The regression declining a block-owned `def` would have risked and reordering does
        // not. A `def` at the true top level of a file **is** a private method of `Object`, so it really is
        // reachable from every class body in the workspace, and the root answer is kept
        // wherever the concern edge has nothing to say.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write("app/boot.rb", "def configure_everything\nend\n");
        let source = "\
class Story < ApplicationRecord
  configure_everything
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let found = card(&mut harness, &uri, source, "configure_everything");
        assert!(found.contains("Object#configure_everything"), "{found}");
        assert!(
            !found.contains("Matched on the method name alone"),
            "it resolves rather than falling to the name rung: {found}"
        );
    }

    #[test]
    fn a_concerns_class_method_never_displaces_one_the_class_really_declares() {
        // The edge is asked **after** the ordinary ancestor search and only when it found
        // nothing, which is the same rule `resolve_typed` holds for a derived receiver: a
        // worse answer may never displace a better one.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let source = "\
class Story < ApplicationRecord
  def self.validates(*names)
  end

  validates :title
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let found = card(&mut harness, &uri, source, "validates :title");
        assert!(found.contains("Story.validates"), "{found}");
        assert!(!found.contains("ActiveModel"), "{found}");
    }

    #[test]
    fn a_call_on_a_class_object_is_never_offered_an_instance_method_of_an_unrelated_class() {
        // The fallback filter, and it is the half a user meets first. The name-based list
        // was every declaration in the graph ending in this name; a class object answers on its
        // singleton chain, which the search above already walked, so a candidate owned by a
        // `class` is provably unreachable and one owned by a `module` is exactly what has to
        // stay — a concern's `ClassMethods` is a module's instance method.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/widget.rb",
            "class Widget\n  def spin\n  end\n\n  def whirl\n  end\nend\n",
        );
        harness.write(
            "app/lib/spinner.rb",
            "module Spinner\n  def spin\n  end\nend\n",
        );
        harness.write(
            "app/models/gadget.rb",
            "class Gadget\n  def self.spin\n  end\nend\n",
        );
        let source = "\
class Story
  spin
  whirl
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        // Two of the three candidates survive: a module's instance method, because that is what
        // an `extend` installs, and another class's **singleton** method, because a class object
        // is what it would be called on. Only `Widget#spin` is dropped.
        let narrowed = card(&mut harness, &uri, source, "spin\n");
        assert!(narrowed.contains("2 possible definitions"), "{narrowed}");
        assert!(narrowed.contains("Gadget.spin"), "{narrowed}");
        assert!(narrowed.contains("Spinner#spin"), "{narrowed}");
        assert!(
            !narrowed.contains("Widget#spin"),
            "an instance method of an unrelated class is not reachable here: {narrowed}"
        );

        // Never emptied. `whirl` is an instance method of a class and nothing else, and a guess
        // is still the honest answer where the graph holds only those.
        let kept = card(&mut harness, &uri, source, "whirl");
        assert!(kept.contains("Widget#whirl"), "{kept}");
        assert!(kept.contains("Matched on the method name alone"), "{kept}");
    }

    #[test]
    fn a_callback_macro_that_declares_nothing_still_answers_nothing() {
        // A bound rather than a gap: `define_model_callbacks` writes `before_save` at run time
        // and no file anywhere holds a `def` for it, so there is nothing for either half of the
        // concern edge to find. Declaring one is `models.rs`' job, and this asserts it was not
        // quietly done here.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let source = "class Story < ApplicationRecord\n  before_save :normalize\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        assert!(
            harness.definition_at(&uri, source, "before_save").is_null(),
            "nothing declares it, so there is nowhere to go"
        );
    }

    #[test]
    fn a_class_body_completes_the_class_methods_of_every_concern_it_includes() {
        // The same cursor that *resolves* `validates` completes to an empty list without this,
        // because resolution takes one answer and completion collects: `query::completion_candidates` walks the singleton's
        // ancestors, the `extend` is on none of them, and nothing was there to be offered.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let uri = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        harness.index();

        for (typed, expected) in [
            ("valid", "validates"),
            ("sco", "scope"),
            ("belongs", "belongs_to"),
            ("counts", "counts_by"),
        ] {
            let offered = harness.declarations_at(
                &uri,
                &format!("class Story < ApplicationRecord\n  {typed}~\nend\n"),
            );
            assert!(
                offered.contains(&expected.to_owned()),
                "{typed} offers no {expected}: {offered:?}"
            );
        }

        // The decline, which is the same gate read from the collecting side: `Plain` has no
        // nested `ClassMethods`, so nothing of it is extended onto anything — its `helper` is an
        // instance method of every *record*, and this cursor is a class object. `Odd` is the
        // other shape and it has nothing observable to assert: its `ClassMethods` is a constant
        // rather than a module, so it is declined where a module would have been walked, and
        // the list is what it would be if the constant were not there at all. `ClassMethods`
        // itself *is* offered, as a constant — Ruby resolves one through the cref's ancestors
        // and rubydex says so, which has nothing to do with this edge.
        let body = harness.declarations_at(&uri, "class Story < ApplicationRecord\n  ~\nend\n");
        assert!(!body.contains(&"helper".to_owned()), "{body:?}");
    }

    /// Rails' own idiom, and the one worth measuring before believing: an `include` written
    /// **inside a `def`** in a `ClassMethods` module is for the class the macro is called on,
    /// and rubydex records it as a mixin of the enclosing module. `ActiveModel::SecurePassword`
    /// is where this is really written; the shape is copied exactly.
    const SECURABLE: &str = "\
module Validatable
  def valid?
  end
end

module Securable
  module ClassMethods
    def has_secure_password(attribute = :password)
      include Validatable
    end
  end
end

class ApplicationRecord
  include Securable
end
";

    #[test]
    fn what_a_class_methods_def_includes_is_the_records_and_not_the_class_objects() {
        // Why the walk takes the module's own members rather than its ancestors'. `extend M`
        // really does
        // install `M`'s ancestors' methods, so the ancestor walk is the right reading of Ruby —
        // and over Rails' five core gems 105 `module ClassMethods` blocks hold 2 mixins written
        // in the module body against **15 written inside a `def`**, every one of the 15 meaning
        // the class the macro was called on. `Story.valid?` raises in Ruby.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", SECURABLE);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // Written rather than opened, because `hover_at` asks about the file the workspace
        // indexed. It is also asked **before** any completion, which opens a buffer over this
        // one: two requests about one document are two requests about whatever text it holds
        // now, and asking them the other way round measures the last thing typed.
        let source = "Story.valid?\n";
        let uri = harness.write("app/models/probe.rb", source);
        harness.index();

        // Resolution reads the same walk, which is the whole reason the walk is shared: before
        // this rule `Story.valid?` resolved, precisely, to a method Ruby raises on.
        let found = card(&mut harness, &uri, source, "valid?");
        assert!(
            found.contains("Matched on the method name alone"),
            "the name rung is the honest answer here: {found}"
        );

        let offered = harness.declarations_at(&uri, "Story.vali~\n");
        assert!(
            !offered.contains(&"valid?".to_owned()),
            "an instance method of a module a macro includes into the record: {offered:?}"
        );

        // The macro itself is the member the module really declares, and it still answers.
        let macros = harness.declarations_at(&uri, "Story.has_secure~\n");
        assert!(
            macros.contains(&"has_secure_password".to_owned()),
            "{macros:?}"
        );
    }

    #[test]
    fn a_class_object_completes_what_a_concern_extends_onto_it_however_it_is_written() {
        // Three spellings of one receiver, and rubydex answers all three with the singleton —
        // which is why `class_object` hands over what the receiver already holds rather than
        // testing the syntax. `Story::validates` is legal Ruby and rare; it is here because
        // `NamespaceAccess` is the one arm that has to find the singleton for itself.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // A second document, because a buffer is what the cursor is completing *in*: writing
        // `Story.valid` into `story.rb` replaces the `class Story` that file holds, and the
        // receiver then resolves to nothing and reaches the name-based list — which offers
        // `validates` too, and would have passed this test for the wrong reason.
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        for marked in [
            "Story.valid~\n",
            "Story::valid~\n",
            "class Story\n  self.valid~\nend\n",
        ] {
            let offered = harness.declarations_at(&uri, marked);
            assert!(
                offered.contains(&"validates".to_owned()),
                "{marked:?} offers nothing a concern extends: {offered:?}"
            );
        }

        // That the receiver really was resolved, and not fallen back on: `Plain#helper` is an
        // instance method of every record and is on no class object's chain, so the name-based
        // list is the only thing that would offer it here.
        let precise = harness.declarations_at(&uri, "Story.help~\n");
        assert!(!precise.contains(&"helper".to_owned()), "{precise:?}");
    }

    #[test]
    fn one_name_two_concerns_is_one_row_and_a_constant_in_a_class_methods_is_none() {
        // Two declines the walk owes rubydex's own. A member declared by two concerns is offered
        // once, by the nearer — `include Countable` is written after `include Recountable`, so
        // Ruby's linearization puts Countable first and `collect_members`' dedup would have kept
        // exactly that one. And a **constant** nested in a `ClassMethods` is not a row at all:
        // `extend` installs methods, and `Countable::ClassMethods::LIMIT` is reachable through
        // the constant path and never through the singleton.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        let rows = harness.first_rows(&uri, "Story.counts~\n", 8);
        assert_eq!(
            rows,
            vec!["counts_by  Countable::ClassMethods#counts_by".to_owned()],
            "one row, from the nearer concern"
        );

        let constants = harness.declarations_at(&uri, "Story.LIM~\n");
        assert!(!constants.contains(&"LIMIT".to_owned()), "{constants:?}");
    }

    #[test]
    fn a_concerns_class_method_ranks_where_an_included_modules_member_ranks() {
        // The ranking half. `Distance::from_receiver` seeds only the chains
        // rubydex is about to walk, and a concern's `ClassMethods` is on none of them — so
        // every one of its members arrived at `NO_DISTANCE`: last, equally last, and *behind
        // `Object`'s own methods*, which is backwards for the name a model body is most likely
        // to be typing. The seed is `locator::Extends::step`, which counts the classes the
        // chain passes through rather than the ancestors, because the instance chain and the
        // singleton chain have different lengths.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        // An `Object` method matching the same prefix, which every class object answers because
        // `Object` is the end of every chain. It is the row the concern's has to beat.
        harness.write(
            "app/lib/patches.rb",
            "class Object\n  def counts_everything\n  end\nend\n",
        );
        let uri = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        harness.index();

        let rows = harness.first_rows(&uri, "class Story < ApplicationRecord\n  counts~\nend\n", 2);
        assert_eq!(
            rows,
            vec![
                "counts_by  Countable::ClassMethods#counts_by".to_owned(),
                "counts_everything  Object#counts_everything".to_owned(),
            ],
            "a concern's class method sits one step out, not last"
        );
    }

    #[test]
    fn a_class_that_writes_its_own_class_method_is_not_offered_a_concerns_as_well() {
        // The dedup rule, per member rather than per request: what a concern extends is only
        // ever what the ordinary walk did not answer. Two rows of one name would be the visible
        // failure; the invisible one is which of them the editor accepts on `tab`.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let source = "class Story < ApplicationRecord\n  def self.validates(*names)\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let rows = harness.first_rows(
            &uri,
            "class Story < ApplicationRecord\n  def self.validates(*names)\n  end\n\n  valid~\nend\n",
            8,
        );
        let named: Vec<&String> = rows
            .iter()
            .filter(|row| row.starts_with("validates "))
            .collect();
        assert_eq!(
            named,
            vec![&"validates  Story.validates".to_owned()],
            "the class's own, once: {rows:?}"
        );
    }

    #[test]
    fn an_instance_of_a_model_is_never_offered_what_a_concern_extends_onto_its_class() {
        // The discriminator is rubydex's rather than a syntactic test, and this is the other
        // side of it: inside a `def`, and after `Story.new.`, `self` is a record — so the
        // concern edge does not apply and `Plain#helper`, which does not apply in a class body,
        // is exactly what does.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        for marked in [
            "class Story\n  def run\n    valid~\n  end\nend\n",
            "Story.new.valid~\n",
        ] {
            let offered = harness.declarations_at(&uri, marked);
            assert!(
                !offered.contains(&"validates".to_owned()),
                "an instance is not a class object: {marked:?} {offered:?}"
            );
        }

        let instance = harness.declarations_at(&uri, "Story.new.help~\n");
        assert!(instance.contains(&"helper".to_owned()), "{instance:?}");
    }

    /// The mailer file the entry-point reader takes, and the base it inherits which the
    /// application does not define — `ActionMailer::Base` is a gem's, and is read anyway.
    const MAILERS: &str = "\
class UserMailer < ApplicationMailer
  def welcome(user)
    mail(to: user)
  end

  private

  def sender
  end
end
";

    #[test]
    fn a_mailer_action_is_a_class_method_that_jumps_to_its_def_and_chains() {
        // A mailer in one expression. Three things have to happen at once: the
        // action is a *class* method, the jump lands on the `def` that implied it, and the
        // chain runs on through `MessageDelivery` — which nothing in this workspace declares,
        // so the stub is what carries it.
        let source = "UserMailer.welcome(current_user).deliver_later\n";
        let (mut harness, _story, uri) = models_project(source);
        let mailer = harness.write("app/mailers/user_mailer.rb", MAILERS);
        harness.watch(&[&mailer]);

        assert!(
            harness.has("UserMailer::<UserMailer>#welcome()"),
            "the action is not a class method"
        );
        assert!(
            !harness.has("UserMailer::<UserMailer>#sender()"),
            "a private def is not an action"
        );

        let definition = harness.definition_at(&uri, source, "welcome");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(mailer.as_str()),
            "{definition}"
        );
        // `  def welcome(user)` on line 1, revealed whole, with the name selected past `def `.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(1),
                &serde_json::json!(2),
                &serde_json::json!(6),
            ),
            "{definition}"
        );

        let card = card(&mut harness, &uri, source, "deliver_later");
        assert!(
            card.contains("MessageDelivery#deliver_later"),
            "the chain did not reach the stub: {card}"
        );
        assert!(!card.contains("Matched on the method name alone"), "{card}");
    }

    #[test]
    fn a_hover_on_a_mailer_action_says_which_def_it_came_from() {
        // The provenance rule again, and it is the same rule: the card names the file and the
        // `def`, because the *generated RBS* carries a comment above the declaration. Nothing
        // in `hover.rs` knows the word "mailer".
        let source = "UserMailer.welcome(current_user)\n";
        let (mut harness, _story, uri) = models_project(source);
        let mailer = harness.write("app/mailers/user_mailer.rb", MAILERS);
        harness.watch(&[&mailer]);

        let card = card(&mut harness, &uri, source, "welcome");
        assert!(card.contains("app/mailers/user_mailer.rb"), "{card}");
        assert!(card.contains("def welcome"), "{card}");
    }

    #[test]
    fn a_jobs_perform_installs_both_entry_points_with_its_own_arity() {
        // The job half, and the arity is the part that has to be exact: an answer is
        // partitioned by how many positional arguments the *call* wrote, so a
        // `perform_later` claiming `()` would answer nothing for every call anybody makes.
        let source = "DigestJob.perform_later(1)\n";
        let (mut harness, _story, uri) = models_project(source);
        let job = harness.write(
            "app/jobs/digest_job.rb",
            "class DigestJob < ApplicationJob\n  \
             def perform(user_id, force = false)\n  \
             end\n\n  \
             def helper\n  end\n\
             end\n",
        );
        harness.watch(&[&job]);

        let rbs = harness.generated_rbs("app/jobs/digest_job.rb");
        assert!(
            rbs.contains("def self.perform_later: (untyped, ?untyped) -> untyped\n"),
            "{rbs}"
        );
        assert!(
            rbs.contains("def self.perform_now: (untyped, ?untyped) -> untyped\n"),
            "{rbs}"
        );
        assert!(
            !rbs.contains("helper"),
            "a job's only entry point is `perform`: {rbs}"
        );

        // Both map to the one `def perform`, which is the whole convention.
        let definition = harness.definition_at(&uri, source, "perform_later");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(job.as_str()),
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetSelectionRange"]["start"]["line"],
            serde_json::json!(1),
            "{definition}"
        );
    }

    #[test]
    fn a_sidekiq_worker_is_read_and_a_service_object_named_perform_is_not() {
        // Sidekiq is not a footnote: 241 of the corpus' 401 job classes are `include
        // Sidekiq::Worker` or `Sidekiq::Job`. And the gate is the superclass or the mixin and
        // never the `def` — 161 of chatwoot's classes define a public `def perform` with
        // nothing above them, and every one of them is a service object.
        let (mut harness, _story, _uri) = models_project("");
        let worker = harness.write(
            "app/workers/bust_cache_worker.rb",
            "class BustCacheWorker\n  \
             include Sidekiq::Worker\n\n  \
             def perform(key)\n  end\n\
             end\n",
        );
        let service = harness.write(
            "app/services/filter_service.rb",
            "class FilterService\n  def perform(scope)\n  end\nend\n",
        );
        harness.watch(&[&worker, &service]);

        for installed in ["perform_async", "perform_in", "perform_at"] {
            assert!(
                harness.has(&format!("BustCacheWorker::<BustCacheWorker>#{installed}()")),
                "{installed} was not installed"
            );
        }
        assert!(
            !harness.has("BustCacheWorker::<BustCacheWorker>#perform_later()"),
            "Sidekiq has no ActiveJob entry points"
        );
        for declined in ["perform_async", "perform_later", "perform_now"] {
            assert!(
                !harness.has(&format!("FilterService::<FilterService>#{declined}()")),
                "{declined} was declared on a service object"
            );
        }
    }

    #[test]
    fn the_message_delivery_stub_is_written_once_and_is_never_a_place() {
        // The relation class's bargain, on a name that is real: one type however many mailers
        // reach it, so exactly one file writes it — and nothing in it is mapped, so when
        // actionmailer *is* indexed the gem keeps every place there is.
        let (mut harness, _story, _uri) = models_project("");
        let first = harness.write("app/mailers/user_mailer.rb", MAILERS);
        let second = harness.write(
            "app/mailers/admin_mailer.rb",
            "class AdminMailer < ActionMailer::Base\n  def alert\n  end\nend\n",
        );
        harness.watch(&[&first, &second]);

        assert!(harness.has("UserMailer::<UserMailer>#welcome()"));
        assert!(
            harness.has("AdminMailer::<AdminMailer>#alert()"),
            "`< ActionMailer::Base` is 19 of the corpus' 53 mailers"
        );
        assert!(harness.has("ActionMailer::MessageDelivery#deliver_now()"));

        // URI order, so `admin_mailer.rb` writes it and `user_mailer.rb` does not.
        let admin = harness.generated_rbs("app/mailers/admin_mailer.rb");
        let user = harness.generated_rbs("app/mailers/user_mailer.rb");
        assert!(
            admin.contains("class ActionMailer::MessageDelivery\n"),
            "{admin}"
        );
        assert!(
            !user.contains("class ActionMailer::MessageDelivery"),
            "{user}"
        );

        // Every declaration in the stub is text this crate invented, so none of them is a
        // definition anything will offer as a place to jump to.
        let source = "AdminMailer.alert.deliver_now\n";
        let uri = harness.write("app/deliver.rb", source);
        harness.watch(&[&uri]);
        let definition = harness.definition_at(&uri, source, "deliver_now");
        assert!(
            definition.as_array().is_none_or(Vec::is_empty),
            "a generated declaration with no span is not a place: {definition}"
        );
    }

    /// A routes file with one of each of the shapes the end-to-end tests need.
    const ROUTES: &str = "\
Rails.application.routes.draw do\n\
  root to: \"home#index\"\n\
  resources :stories, only: [:index, :show] do\n\
    post :upvote, on: :member\n\
  end\n\
  draw :admin\n\
end\n";

    /// A project with a routes file, a controller, a helper and a template.
    fn routes_project(caller: &str) -> (Harness, DocUri) {
        let (mut harness, _story, uri) = models_project(caller);
        let routes = harness.write("config/routes.rb", ROUTES);
        let drawn = harness.write(
            "config/routes/admin.rb",
            "namespace :admin do\n  resources :flags, only: [:index]\nend\n",
        );
        let controller = harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController < ApplicationController\n  def show\n  end\nend\n",
        );
        let helper = harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\nend\n",
        );
        harness.watch(&[&routes, &drawn, &controller, &helper]);
        (harness, uri)
    }

    #[test]
    fn a_route_helper_is_a_method_on_every_controller_and_jumps_to_the_routing_dsl() {
        // `story_path` in a controller resolves through the `include` this pass wrote — an
        // `include` nobody's code contains, which is what `Facts::mixins` exists for — and the
        // jump lands on the
        // `resources :stories` line that named it.
        let (mut harness, _uri) = routes_project("");
        let source = "class StoriesController < ApplicationController\n  def index\n    redirect_to story_path\n  end\nend\n";
        let controller = harness.write("app/controllers/stories_controller.rb", source);
        harness.watch(&[&controller]);

        let card = card(&mut harness, &controller, source, "story_path");
        // No return type: a hover card prints the signature and the provenance, never the
        // `-> String`, which is true of every card here.
        assert!(card.contains("RouteHelpers#story_path"), "{card}");
        assert!(
            !card.contains("Matched on the method name alone"),
            "the ancestry is exact, not a name match: {card}"
        );
        assert!(card.contains("resources :stories"), "{card}");

        let jump = harness.definition_at(&controller, source, "story_path");
        let target = jump[0]["targetUri"].as_str().unwrap_or_default();
        assert!(target.ends_with("config/routes.rb"), "{jump}");
        let selected = &jump[0]["targetSelectionRange"];
        assert_eq!(selected["start"]["line"], 2, "the `resources` line: {jump}");
    }

    #[test]
    fn a_route_helper_answers_in_a_helper_module_and_in_a_template() {
        // The other two contexts a helper is called from. A helper module is a host, so the
        // answer inside one is exact. A template has no enclosing class at all, so without a
        // view context its `story_path` reaches the same module by the *name* rung — enough for
        // the jump and not for completion. It is exact there too, by a route neither convention
        // planned: the route helpers go into one module that is
        // `include`d into every `app/helpers` module, and the view context *is* those modules,
        // so the chain from a template to `resources :stories` is two conventions long and has
        // no guess in it.
        let (mut harness, _uri) = routes_project("");
        let helper = "module StoriesHelper\n  def link\n    story_path\n  end\nend\n";
        let uri = harness.write("app/helpers/stories_helper.rb", helper);
        let template = "<%= link_to \"x\", story_path %>\n";
        let view = harness.write("app/views/stories/index.html.erb", template);
        harness.watch(&[&uri, &view]);

        let inside = card(&mut harness, &uri, helper, "story_path");
        assert!(
            inside.contains("RouteHelpers#story_path"),
            "a helper module is a host: {inside}"
        );
        assert!(
            !inside.contains("Matched on the method name alone"),
            "{inside}"
        );

        let in_template = card(&mut harness, &view, template, "story_path");
        assert!(
            in_template.contains("RouteHelpers#story_path"),
            "{in_template}"
        );
        assert!(
            !in_template.contains("Matched on the method name alone"),
            "the view context reaches it through `StoriesHelper`, not by the name: {in_template}"
        );
        assert!(
            in_template.contains("Reached through the view context"),
            "and the card says which convention it came through: {in_template}"
        );
        let jump = harness.definition_at(&view, template, "story_path");
        assert!(
            jump[0]["targetUri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("config/routes.rb"),
            "and it still lands on the DSL: {jump}"
        );
    }

    #[test]
    fn a_drawn_routes_file_is_read_at_the_prefix_it_was_drawn_at() {
        // `draw :admin` is the only thing that says where `config/routes/admin.rb` sits, and
        // its declarations go into **its own** generated document, because a span is a byte
        // range with no URI and one recorded against the drawer would open the wrong line.
        let (harness, _uri) = routes_project("");
        assert!(harness.has("RouteHelpers#admin_flags_path()"));
        let drawn = harness.generated_rbs("config/routes/admin.rb");
        assert!(drawn.contains("def admin_flags_path:"), "{drawn}");
        assert!(
            drawn.contains("`resources :flags, only: [:index]`"),
            "{drawn}"
        );
        let main = harness.generated_rbs("config/routes.rb");
        assert!(!main.contains("admin_flags"), "{main}");
        assert!(main.contains("def story_path:"), "{main}");
    }

    #[test]
    fn the_helpers_are_included_once_into_every_controller_mailer_and_helper_module() {
        // Rails installs them with an `inherited` hook on `ActionController::Base`, so every
        // controller really does get its own copy — and writing one `include` per host is what
        // bounds that gap at zero: an application whose base is a *gem's* class has no
        // base this pass defines, and every one of its controllers is still a host.
        let (mut harness, _uri) = routes_project("");
        let mailer = harness.write("app/mailers/user_mailer.rb", MAILERS);
        let model = harness.write("app/models/plain.rb", "class Plain\nend\n");
        harness.watch(&[&mailer, &model]);

        let main = harness.generated_rbs("config/routes.rb");
        for host in [
            "class StoriesController\n  include RouteHelpers",
            "class UserMailer\n  include RouteHelpers",
            "module StoriesHelper\n  include RouteHelpers",
        ] {
            assert!(main.contains(host), "{main}");
        }
        assert!(
            !main.contains("class Plain\n"),
            "a class that is neither is not a host: {main}"
        );
        // One document holds them all, so a second routes file adds no second copy.
        let drawn = harness.generated_rbs("config/routes/admin.rb");
        assert!(!drawn.contains("include RouteHelpers"), "{drawn}");
        // …and a plain model still cannot see them, which is what makes the module worth having
        // instead of declaring two thousand helpers on `Object`.
        let source = "class Plain\n  def go\n    story_path\n  end\nend\n";
        let plain = harness.write("app/models/plain.rb", source);
        harness.watch(&[&plain]);
        let card = card(&mut harness, &plain, source, "story_path");
        assert!(
            card.contains("Matched on the method name alone"),
            "the name rung, not the ancestry: {card}"
        );
    }

    #[test]
    fn exactly_one_routes_file_declares_each_helper() {
        // Two routes files naming one helper would land in two generated documents, where
        // `Facts`' precedence cannot see them and RBS holds two `def story_path:` lines as an
        // overload set. First in URI order writes it, exactly as for a shared relation class.
        let (mut harness, _uri) = routes_project("");
        let engine = harness.write(
            "engines/blog/config/routes.rb",
            "Rails.application.routes.draw do\n  resources :stories, only: [:index]\n  resources :posts, only: [:index]\nend\n",
        );
        harness.watch(&[&engine]);

        assert_eq!(
            harness
                .analysis
                .graph
                .get("RouteHelpers#stories_path()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "one declaration, however many files name the route"
        );
        // Whichever file sorts first keeps it; here that is the application's own.
        let engine_rbs = harness.generated_rbs("engines/blog/config/routes.rb");
        let main = harness.generated_rbs("config/routes.rb");
        assert!(main.contains("def stories_path:"), "{main}");
        assert!(main.contains("def story_path:"), "{main}");
        assert!(engine_rbs.contains("def posts_path:"), "{engine_rbs}");
        assert!(
            !engine_rbs.contains("def stories_path:"),
            "`config/` sorts before `engines/`, so the application's own file keeps it: {engine_rbs}"
        );
    }

    #[test]
    fn an_application_that_declares_the_module_itself_keeps_it() {
        // The rule `Comment::Relation` follows, and here it costs the whole feature: a project
        // that spelled `RouteHelpers` meant something by it, and a module this pass wrote into
        // would answer with its members and ours at once.
        let (mut harness, _uri) = routes_project("");
        assert!(harness.has("RouteHelpers#story_path()"));
        let theirs = harness.write(
            "app/models/route_helpers.rb",
            "module RouteHelpers\n  def story_path\n  end\nend\n",
        );
        harness.watch(&[&theirs]);
        assert_eq!(
            harness
                .analysis
                .graph
                .get("RouteHelpers#story_path()")
                .map_or(0, |definitions| definitions.len()),
            1,
            "theirs, and only theirs"
        );
        assert!(!harness.has("RouteHelpers#admin_flags_path()"));
    }

    #[test]
    fn a_routes_file_that_stops_declaring_takes_its_helpers_with_it() {
        // The pruning rule, on the one generator whose document also carries the `include`s:
        // an empty routes file must leave neither a helper nor a host behind.
        let (mut harness, _uri) = routes_project("");
        assert!(harness.has("RouteHelpers#story_path()"));
        let routes = harness.write(
            "config/routes.rb",
            "Rails.application.routes.draw do\nend\n",
        );
        harness.watch(&[&routes]);
        assert!(!harness.has("RouteHelpers#story_path()"));
        assert!(!harness.has("RouteHelpers#admin_flags_path()"));
    }

    #[test]
    fn the_query_interface_is_read_by_arity_like_every_other_signature() {
        // The arity partition doing work it already does, on text this crate wrote rather than on
        // `vendor/rbs`. `find` requires an argument and `find_by` does not, so `Story.find(1)`
        // is a `Story` and `Story.find` is nothing — the arm that would have answered is one no
        // call reached, which is the whole of why a generated signature is safe to write.
        let source = "Story.find(1).user\n";
        let (mut harness, _story, uri) = models_project(source);
        let found = card(&mut harness, &uri, source, "user");
        assert!(found.contains("Story#user"), "{found}");
        assert!(
            !found.contains("Matched on the method name alone"),
            "{found}"
        );

        // `find_by` is declared `Story?`, and optional takes the inner type — so the chain off
        // it is the same one, which is the entry `types.md` calls the one inexact one.
        let maybe = "Story.find_by(id: 1).user\n";
        let by = harness.write("app/by.rb", maybe);
        harness.watch(&[&by]);
        let optional = card(&mut harness, &by, maybe, "user");
        assert!(optional.contains("Story#user"), "{optional}");

        let bare = "Story.find.user\n";
        let other = harness.write("app/bare.rb", bare);
        harness.watch(&[&other]);
        let missed = card(&mut harness, &other, bare, "user");
        assert!(
            missed.contains("Matched on the method name alone"),
            "a call no arm accepts is answered for by none of them: {missed}"
        );
    }

    #[test]
    fn the_query_interface_is_not_a_place_a_user_is_sent() {
        // The no-place clause, in a stronger version. A relation's members could at least
        // have been pointed at one of the `has_many`s that asked for the class; `Story.where`
        // could be pointed nowhere at all, because no line of anybody's code declares it.
        let source = "Story.first\n";
        let (mut harness, _story, uri) = models_project(source);
        assert!(
            harness.definition_at(&uri, source, "first").is_null(),
            "a declaration this crate invented must not be a place"
        );
    }

    #[test]
    fn every_model_answers_the_query_interface_and_nothing_else_does() {
        // The bound is the superclass chain and not "a class some macro made a collection",
        // which is evidence a file states and is also the wrong evidence: ActiveRecord answers `where` on a model because it is a model, and
        // `has_many` has nothing to do with it. So the bound is now the superclass chain, which
        // is evidence a file states too — and declaring the names on *everything* is still
        // how a convention table starts being wrong, which is what the last assertion holds.
        let (mut harness, _story, _uri) = models_project("");
        assert!(harness.has("Story::<Story>#where()"));

        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget < ApplicationRecord\n  belongs_to :story\nend\n",
        );
        harness.watch(&[&widget]);
        assert!(harness.has("Widget#story()"), "the association still reads");
        assert!(
            harness.has("Widget::<Widget>#where()"),
            "nothing collects a Widget and it is a model regardless"
        );
        assert!(
            harness.has("Widget::Relation"),
            "and the relation the class side returns exists"
        );

        let gadget = harness.write("app/lib/gadget.rb", "class Gadget\nend\n");
        harness.watch(&[&gadget]);
        assert!(
            !harness.has("Gadget::<Gadget>#where()"),
            "a class that inherits nothing is not a model"
        );
    }

    #[test]
    fn the_callbacks_resolve_on_a_model_and_on_nothing_else() {
        // `before_create` is a `def` in activesupport that
        // `define_model_callbacks` wrote at boot, so no file in the workspace declares it, the
        // graph correctly found nothing, and the name rung answered with the only
        // `before_create` anybody's file *does* write —
        // `Fabrication::Schematic::Evaluator#before_create`, in a gem, in a fixture library.
        //
        // The last assertion is the bound, and it is the entry points' shape: a class that is
        // not a model gets none of these, so a gem that really does define one keeps
        // every position it had.
        let (mut harness, _story, _uri) = models_project("");
        let source = "class Widget < ApplicationRecord\n                        after_initialize :a\n                        before_create :b\n                        before_save :c\n                        after_create_commit :d\n                      end\n";
        let widget = harness.write("app/models/widget.rb", source);
        let plain = "class Gadget\n  before_create :b\nend\n";
        let gadget = harness.write("app/lib/gadget.rb", plain);
        harness.watch(&[&widget, &gadget]);

        for name in [
            "after_initialize",
            "before_create",
            "before_save",
            "after_create_commit",
        ] {
            assert!(
                harness.has(&format!("Widget::<Widget>#{name}()")),
                "{name} is not on the model's singleton"
            );
            let found = card(&mut harness, &widget, source, name);
            assert!(
                found.contains("no file declares it"),
                "{name} does not carry the generated provenance: {found}"
            );
            assert!(
                !found.contains("possible definitions"),
                "{name} is still a candidate list: {found}"
            );
        }
        // Nothing is mapped, which is the no-place rule: `define_model_callbacks` is a `def` in a
        // gem that nobody's file wrote, so the honest answer to "where" is nowhere at all.
        assert!(
            harness
                .definition_at(&widget, source, "before_create")
                .is_null(),
            "a declaration this crate invented must not be a place"
        );
        assert!(
            !harness.has("Gadget::<Gadget>#before_create()"),
            "a class that inherits nothing is not a model and gets none of them"
        );
    }

    #[test]
    fn a_receiverless_call_answers_what_the_same_call_on_self_answers() {
        // Asserted as an **equality** rather than as a list of expected types, because the
        // right answer is known before the server is asked. `self.comments.first` resolves;
        // `comments.first`, one word shorter and the same Ruby, reaches the name rung and
        // offers a list unless a receiverless call is read as `self`.
        //
        // Two files rather than two lines of one, because `position_of` takes the first
        // occurrence of a word — and because that is the shape the probe which found this used:
        // the same expression, written twice, with one difference.
        let (mut harness, _story, _uri) = models_project("");
        let explicit_source = "class Widget < ApplicationRecord\n                                 has_many :comments\n                                 def a\n    self.comments.first\n  end\n                               end\n";
        let bare_source = "class Widget\n  def b\n    comments.first\n  end\nend\n";
        let explicit = harness.write("app/models/widget.rb", explicit_source);
        let bare = harness.write("app/models/widget_more.rb", bare_source);
        harness.watch(&[&explicit, &bare]);

        let with_self = card(&mut harness, &explicit, explicit_source, "first");
        let without = card(&mut harness, &bare, bare_source, "first");
        assert!(
            with_self.contains("ActiveRecordRelation#first"),
            "the twin the equality is against has to be the answer it always was: {with_self}"
        );
        assert_eq!(
            without, with_self,
            "an implicit receiver is a `self` the writer did not type, and the two must not \
             answer differently"
        );

        // The widened guard, measured on its own because it is the half that can reach a method
        // the file does not declare. `find(1)` writes an argument, so a guard that refuses
        // arguments makes it `Receiver::Unknown` and ends the chain — the guard is a bound on
        // the *guess* and must not be spent on the lookup as well.
        let with_argument = "class Gizmo < ApplicationRecord\n                               has_many :comments\n                               def self.c\n    find(1).comments.first\n  end\n                             end\n";
        let gizmo = harness.write("app/models/gizmo.rb", with_argument);
        harness.watch(&[&gizmo]);
        let chained = card(&mut harness, &gizmo, with_argument, "first");
        assert!(
            chained.contains("ActiveRecordRelation#first"),
            "a receiverless call that wrote an argument still resolves: {chained}"
        );
        assert!(
            !chained.contains("Matched on the method name alone"),
            "{chained}"
        );
    }

    #[test]
    fn the_collection_predicates_are_on_the_side_rails_puts_them_on() {
        // The collection predicates, both halves. `api_key_scopes.size` is the shape a user hits:
        // an association that types, a relation that exists, and a member of it that nothing
        // declared — so a chain which had already resolved twice fell to the name rung at its
        // third hop and offered 192 possible definitions.
        //
        // The other half is a subtraction, and it is the item's finding rather than its
        // feature. `ActiveRecord::Querying::QUERYING_METHODS` *is* the class side, and it names
        // neither `each` nor `to_a` — both a `NoMethodError` on a model in Ruby — nor `size`,
        // `length` and `empty?`, which `Relation` defines and nothing delegates. A generated
        // declaration no legal call can reach is the same defect as an inherited class side.
        let source = "Story.first.comments.size
";
        let (mut harness, _story, uri) = models_project(source);

        let counted = card(&mut harness, &uri, source, "size");
        // One copy for the project puts the whole interface onto one class the project's
        // relations inherit, so the card names that rather than `Comment::Relation` — the
        // stated cost, and it is the third hop of a chain that still resolves, which is what the
        // assertion below is about.
        assert!(counted.contains("ActiveRecordRelation#size"), "{counted}");
        assert!(
            !counted.contains("Matched on the method name alone"),
            "the third hop of the chain resolves rather than guessing: {counted}"
        );

        assert!(harness.has("ActiveRecordRelation#empty?()"));
        assert!(
            harness.has("ActiveRecordRelation#any?()"),
            "a predicate hands its block the element, which is receiver-relative now"
        );
        assert!(
            harness.has("Story::<Story>#count()"),
            "`count` is delegated and starts a chain on the model"
        );
        assert!(harness.has("Story::<Story>#exists?()"));
        assert!(
            !harness.has("Story::<Story>#size()"),
            "`QUERYING_METHODS` does not name `size`, and `Story.size` raises"
        );
        assert!(
            !harness.has("Story::<Story>#each()"),
            "nor `each`, which the model class side does not get at all"
        );
        assert!(
            harness.has("ActiveRecordRelation#each()"),
            "the relation keeps every one of them, once for the project"
        );
        assert!(
            !harness.has("Story::Relation#each()"),
            "and declares none of them itself — one copy for the project"
        );
    }

    /// The writer half of an association, on both sides of it.
    ///
    /// A collection's constructors are the **relation's** — `relation.rb` defines `create` at
    /// 155, `create!` at 170 and `new` at 126 with `alias build new` at 134 — and a singular
    /// association's are three `def`s Rails writes onto the model from the macro line, in
    /// `associations/builder/singular_association.rb`. ya-lsp declared `build` and not `new`,
    /// which is an incoherence rather than a bound since they are one method, and declared none
    /// of the singular three at all.
    #[test]
    fn an_association_answers_what_it_builds_and_creates() {
        let source = "Story.first.comments.create!.story\n\
                      Story.first.comments.new.story\n\
                      Story.first.create_user\n\
                      Story.first.create_parent_story.comments\n\
                      Story.first.build_ghost\n";
        let (mut harness, _story, uri) = models_project(source);

        // The collection half, through the relation every model's inherits.
        for word in ["create!", "new"] {
            let card = card(&mut harness, &uri, source, word);
            assert!(
                card.contains(&format!("ActiveRecordRelation#{word}")),
                "{card}"
            );
        }
        assert!(harness.has("ActiveRecordRelation#new()"));
        // …and `new` is the relation's alone, because a model gets its own from `Class` and a
        // declaration on the class side would shadow something real.
        assert!(!harness.has("Story::<Story>#new()"));

        // The singular half, on the model, from the macro line.
        assert!(harness.has("Story#create_user()"));
        assert!(harness.has("Story#build_user()"));
        assert!(harness.has("Story#create_user!()"));
        let created = card(&mut harness, &uri, source, "create_user");
        assert!(created.contains("Story#create_user"), "{created}");
        assert!(
            created.contains("`belongs_to :user`"),
            "the macro line is the place: {created}"
        );
        // It chains, and it is **not** nilable where the reader is: `belongs_to :parent_story,
        // optional: true` reads a `Story?` and `create_parent_story` makes one, so it is a
        // `Story`.
        let chained = card(&mut harness, &uri, source, "comments");
        assert!(chained.contains("Story#comments"), "{chained}");
        assert!(
            !chained.contains("Matched on the method name alone"),
            "{chained}"
        );

        // A collection macro installs none of the three, because a collection's are the
        // relation's — and `has_many :comments` is in the same file as the `belongs_to` above.
        assert!(!harness.has("Story#build_comments()"));
        assert!(!harness.has("Story#create_comment()"));
        // A polymorphic `belongs_to` names no class, so Rails writes no constructors and
        // neither does this — `owner` is the fixture's polymorphic one.
        assert!(!harness.has("Story#build_owner()"));
        // …and `belongs_to :ghost` names a class the project does not define.
        assert!(!harness.has("Story#build_ghost()"));
        // `reload_x` and `reset_x` come off the same Rails method and are declined on their
        // measurement: 1 call site and 0 in six corpora.
        assert!(!harness.has("Story#reload_user()"));
        assert!(!harness.has("Story#reset_user()"));
    }

    /// The rest of what one association line installs — Rails' own "Auto-generated methods"
    /// table, and every row of it this reader can name.
    #[test]
    fn an_association_answers_its_writer_and_the_ids_of_a_collection() {
        let source = "Story.first.user = User.first\n\
                      Story.first.comments = []\n\
                      Story.first.comment_ids = []\n\
                      Story.first.tag_ids.join\n\
                      Story.first.comments.reload.first.story\n";
        let (mut harness, _story, uri) = models_project(source);

        // The singular writer, which is nilable whatever the reader is: `belongs_to :user` is
        // required and `story.user = nil` is still ordinary Ruby — the validation is what
        // fails, at save.
        assert!(harness.has("Story#user=()"));
        let written = card(&mut harness, &uri, source, "user =");
        assert!(written.contains("Story#user="), "{written}");
        assert!(
            written.contains("`belongs_to :user`"),
            "the macro line is the place: {written}"
        );

        // The collection's three. `has_many` singularizes the **association's own name** rather
        // than the class it resolves to, which is what `has_many :tags, through: :taggings`
        // shows: it collects a `Tag` and its ids are `tag_ids`.
        assert!(harness.has("Story#comments=()"));
        assert!(harness.has("Story#comment_ids()"));
        assert!(harness.has("Story#comment_ids=()"));
        assert!(harness.has("Story#tag_ids()"));
        // …and a singular association installs neither: `has_one :draft` is a `Comment`.
        assert!(!harness.has("Story#draft_ids()"));
        assert!(!harness.has("Story#comment_ids_ids()"));

        // The name comes from the **association** and the card says which macro wrote it —
        // `has_many :tags, through: :taggings` collects a `Tag`, and `tag_ids` is singularized
        // from `tags` rather than from the class.
        let ids = card(&mut harness, &uri, source, "tag_ids");
        assert!(ids.contains("`has_many :tags`"), "{ids}");
        // `Array[untyped]` and not a bare `untyped`. No hover card prints a return type, so the
        // chain is what states it: what a primary key holds is the schema's to say and is in
        // another generated document this reader cannot ask, but the *array* is known and is
        // what the call sites want.
        let joined = card(&mut harness, &uri, source, "join");
        assert!(joined.contains("Array#join"), "{joined}");

        // `Relation#reload` hands the relation back, so a chain runs on through it. It is the
        // relation's alone — `ActiveRecord::Base#reload` is an instance method, so
        // `Story.reload` reaches `Class`, finds nothing and raises.
        assert!(harness.has("ActiveRecordRelation#reload()"));
        assert!(!harness.has("Story::<Story>#reload()"));
        let reloaded = card(&mut harness, &uri, source, "story\n");
        assert!(reloaded.contains("Comment#story"), "{reloaded}");

        // The four Rails installs that are declined on their measurement: eight call sites in
        // six applications against a name per association.
        assert!(!harness.has("Story#user_changed?()"));
        assert!(!harness.has("Story#user_previously_changed?()"));
        // A polymorphic `belongs_to` names no class, so this reader declares none of its names
        // at all — not the reader and not the writer either.
        assert!(!harness.has("Story#owner=()"));
    }

    #[test]
    fn a_relation_is_an_enumerable_and_a_select_chain_does_not_end_at_one() {
        // Rails writes `include Enumerable` in `ActiveRecord::Relation`, and leaving it out
        // costs measurable down-moves: `select` correctly answers a relation, and a relation
        // with nothing of `Enumerable` in it is a **dead end** for the `.index_by` or `.map`
        // that habitually follows one. Four of twelve
        // down-moves were this and nothing else — the first hop made right and the second made
        // impossible.
        let source = "Story.where(id: 1).select(:id).sort\n";
        let (mut harness, _story, uri) = models_project(source);

        let sorted = card(&mut harness, &uri, source, "sort");
        assert!(sorted.contains("Enumerable#sort"), "{sorted}");
        assert!(
            !sorted.contains("Matched on the method name alone"),
            "the hop after a `select` resolves rather than guessing: {sorted}"
        );
        // One `include` for the whole project, on the class every relation inherits.
        let rbs = harness.generated_rbs("app/models/story.rb");
        assert!(
            rbs.contains("class Story::Relation < ActiveRecordRelation\n"),
            "{rbs}"
        );
    }

    /// A project that declares `ActiveRecordRelation` itself keeps it, and loses the feature.
    ///
    /// The rule `Comment::Relation` and `ROUTE_HELPERS` follow, asked of the one class the
    /// query interface invents. What it withdraws is the **relations**, which is that same
    /// collision behaviour reached by a second road rather than a rule of its own: with no relation class there is nothing for a `has_many` to return, so the whole
    /// half declines together instead of leaving a `-> Comment::Relation` naming a class nothing
    /// declares, or a relation inheriting whatever the user meant by the name.
    #[test]
    fn a_project_that_declares_the_relation_base_itself_is_not_shadowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        let own = harness.write(
            "app/lib/active_record_relation.rb",
            "class ActiveRecordRelation\n  def mine\n  end\nend\n",
        );
        harness.index();

        assert!(
            harness.has("ActiveRecordRelation#mine()"),
            "their class stands"
        );
        assert!(
            !harness.has("ActiveRecordRelation#where()"),
            "and this pass writes nothing into it"
        );
        let rbs = harness.generated_rbs("app/models/story.rb");
        assert!(!rbs.contains("Relation"), "{rbs}");
        assert!(
            !rbs.contains("def comments:"),
            "the whole half declines together: {rbs}"
        );

        // Take the name away and the whole feature comes back, which is what says the decline
        // is about the collision rather than about anything else in the fixture.
        std::fs::remove_file(own.to_path().unwrap()).unwrap();
        harness.watch(&[&own]);
        assert!(harness.has("ActiveRecordRelation#where()"));
        let rbs = harness.generated_rbs("app/models/story.rb");
        assert!(
            rbs.contains("class Comment::Relation < ActiveRecordRelation\n"),
            "{rbs}"
        );
        assert!(
            rbs.contains("def comments: () -> Comment::Relation\n"),
            "{rbs}"
        );
    }

    /// A model whose superclass chain leaves the application pays for its own class side.
    ///
    /// One copy for the project puts the query interface on the **base**, and the base has
    /// to be a class this pass may declare on. forem writes `Tag < ActsAsTaggableOn::Tag` and
    /// `EmailMessage < Ahoy::Message`, which are real models whose base class is in a gem: the
    /// walk stops at the model itself, and it gets the copy every model used to get. The
    /// alternative — a base ya-lsp invents — does not work at all, because a generated
    /// superclass on a class the user's own file already gives one is silently ignored.
    #[test]
    fn a_model_whose_base_is_not_the_applications_declares_its_own_class_side() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\n  has_many :tags\nend\n",
        );
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        harness.write(
            "app/models/tag.rb",
            "class Tag < ActsAsTaggableOn::Tag\n  has_many :comments\nend\n",
        );
        harness.index();

        // The application has a base and every model under it inherits one copy.
        assert!(harness.has("ApplicationRecord::<ApplicationRecord>#where()"));
        assert!(!harness.has("Story::<Story>#where()"));
        assert!(!harness.has("Comment::<Comment>#where()"));
        // `Tag` is a collection element rather than a model by the walk — its chain leaves the
        // application at a class in a gem — so it is its own base and pays for its own copy.
        assert!(harness.has("Tag::<Tag>#where()"));
        // And the callbacks travel with it, because they are inherited for the same reason.
        assert!(harness.has("ApplicationRecord::<ApplicationRecord>#before_save()"));
        assert!(harness.has("Tag::<Tag>#before_save()"));
        assert!(!harness.has("Story::<Story>#before_save()"));
    }

    #[test]
    fn the_vocabulary_is_rails_own_list_and_the_call_decides_the_arm() {
        // The table is `ActiveRecord::Querying::QUERYING_METHODS` — all 113 — rather than the
        // names somebody could type a signature for, plus the two places
        // Rails puts a class method that is not in it. What the widening rests on is that a
        // name may still refuse a *type*: `pick` and every `async_*` are declared `untyped`, so
        // the name resolves and the chain stops: the type declines, the name never does.
        let source = "Story.first.comments.pluck(:body)\n";
        let (mut harness, _story, uri) = models_project(source);

        // `create!` is the interesting one: class-side, in `Persistence::ClassMethods`
        // and **not** in `QUERYING_METHODS`, so a table read out of that constant alone could
        // never have had it — and it is on no relation, because no relation answers it.
        assert!(harness.has("Story::<Story>#create!()"));
        // …and `relation.rb` defines it too, which a completion sweep is what catches: five of
        // that block's six names are on both sides, and
        // `instantiate` is the one that is genuinely the class's alone.
        assert!(harness.has("ActiveRecordRelation#create!()"));
        assert!(harness.has("Story::<Story>#instantiate()"));
        assert!(!harness.has("ActiveRecordRelation#instantiate()"));
        // And the traffic runs the other way for the five that raise on a model, which is why
        // the side is a three-way answer rather than a flag.
        assert!(harness.has("ActiveRecordRelation#empty?()"));
        assert!(!harness.has("Story::<Story>#empty?()"));

        // A name Rails names and this crate cannot type is declared anyway. `Story.async_count`
        // resolves to something instead of falling to the name rung; what it hands back is
        // `ActiveRecord::Promise`, which no generator here writes, so the type declines and
        // `Types::harvest` drops it.
        assert!(harness.has("Story::<Story>#async_count()"));
        assert!(harness.has("Story::<Story>#upsert_all()"));

        let plucked = card(&mut harness, &uri, source, "pluck");
        assert!(plucked.contains("ActiveRecordRelation#pluck"), "{plucked}");
        assert!(
            !plucked.contains("Matched on the method name alone"),
            "{plucked}"
        );

        // The arity split, which is the half that needed a new shape in the fact table: one
        // declaration, two arms, and the count at the call site decides. `Array` is what
        // `class_at` reads off the members offered, so this is the answer a user would see.
        assert_eq!(class_at(&mut harness, &uri, "Story.first(3).~"), "Array");
        assert_eq!(class_at(&mut harness, &uri, "Story.pluck(:id).~"), "Array");
        // `select` is the same shape decided by the block instead: with column names it is a
        // query method and with a block it is `Enumerable`'s, reached through `super`. Both
        // arms are asserted, because an overload that answered the block arm for every call
        // would pass a test that only looked at one of them.
        assert_eq!(
            class_at(&mut harness, &uri, "Story.select { |s| s }.~"),
            "Array"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "Story.select(:id).count.~"),
            "Integer",
            "with column names it is still a relation, so the chain runs on through it"
        );
    }

    #[test]
    fn where_never_answers_the_chain_a_keyword_hash_cannot_be_told_from() {
        // A bound stated as a test, because it is a decision rather than an omission. `where` with no argument returns a `QueryMethods::WhereChain`, which is
        // where `not`, `missing` and `associated` live — 843 call sites of `not` in the six
        // corpora. An arity split beside `first`'s is **not expressible** for it: `arity_of` deliberately does not count a keyword hash as a positional
        // argument, so `3.7.round(half: :up)` reaches the zero-argument arm — and so does
        // `Story.where(title: "x")`, the commonest call in Rails. An arm answering `WhereChain`
        // at arity 0 would answer it for that call too.
        //
        // So `where` answers a relation on every arm. The assertion that matters is the second
        // one: the keyword form keeps the answer it has always had.
        let source = "Story.where.not(id: 1)\nStory.where(title: \"x\").first.user\n";
        let (mut harness, _story, uri) = models_project(source);
        assert!(
            !harness.has("Story::Relation#not()") && !harness.has("ActiveRecordRelation#not()"),
            "`not` is `WhereChain`'s and putting it on a relation would make `Story.all.not` \
             resolve, which raises"
        );
        let kept = card(&mut harness, &uri, source, "user");
        assert!(kept.contains("Story#user"), "{kept}");
    }

    #[test]
    fn a_model_that_writes_no_macro_answers_on_its_own_relation_and_not_its_parents() {
        // The defect the per-model class side exists to fix, and the reason the relation set
        // is a **union** rather than a rule about abstract classes. lobsters writes `scope :select_fix` in its
        // `ApplicationRecord`; that made `ApplicationRecord` a collection element, wrote an
        // `ApplicationRecord::Relation`, and put the ten names on a singleton **every model in
        // the application inherits** — so `Category.order(...)` answered a relation of a class
        // no row is ever an instance of, and every chain off it was wrong rather than absent.
        // 107 lobsters positions name `ApplicationRecord` in their card and 64 of them resolve
        // or derive through exactly this.
        //
        // Declining to give an abstract class a relation deletes the wrong answer and supplies
        // nothing: `Category.select_fix` goes with it, because a `scope` declares nothing at
        // all when its class has no relation. That was built, measured at **64** down-moves
        // against the 11 it was meant to repair, and reverted. What fixes it is `Category`
        // owning the ten names itself, so the inherited pair is never reached.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/channel.rb", "module Channel\nend\n");
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\n  \
             self.abstract_class = true\n  \
             scope :select_fix, -> { all }\n\
             end\n",
        );
        harness.write(
            "app/models/category.rb",
            "class Category < ApplicationRecord\n  has_many :things\nend\n",
        );
        harness.write(
            "app/models/thing.rb",
            "class Thing < ApplicationRecord\nend\n",
        );
        let source = "Category.order(:id).first.things\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // **Asserted on the place rather than on the card**, which one copy per project makes
        // necessary and which is the sharper test either way. `order` is declared once on the
        // base, so the card names `ApplicationRecord.order` whatever it
        // returns; what has to be right is the *chain*, and every link of it is the defect:
        // `Category.order` is a `Category::Relation`, its `first` is a `Category`, and only a
        // `Category` has `things`. The defect is each of those answering `ApplicationRecord`
        // instead.
        let things = card(&mut harness, &uri, source, "things");
        assert!(
            things.contains("Category#things"),
            "a model answers its own relation, not the one its abstract parent owns: {things}"
        );
        assert!(
            harness.has("ApplicationRecord::<ApplicationRecord>#select_fix()"),
            "and the parent keeps the scope it really does install on every subclass"
        );
        // The other half, and the receiver-relative return types **reverse** it. Taking the
        // class side off an abstract class is the right rule while the interface names a
        // concrete class, because it is inherited — so a model whose own
        // class side was out of reach for some other reason answered `ApplicationRecord` —
        // chatwoot's `Captain::Assistant.find` is the position that measured it. The two
        // receiver-relative return types fix that at its source rather than by withholding the
        // declaration, so the interface is on the base *deliberately* now and the chain above
        // is what says it is safe. What is left of the trade is stated: `ApplicationRecord.order`
        // resolves and raises in Ruby, on a receiver nobody writes.
        assert!(harness.has("ActiveRecordRelation#order()"));
        assert!(harness.has("ApplicationRecord::<ApplicationRecord>#order()"));
        assert!(
            !harness.has("Category::<Category>#order()"),
            "and no model declares its own copy, which is the declaration count"
        );
    }

    #[test]
    fn the_chain_that_promoted_this_item_lands_on_one_target() {
        // The chain this exists for: `Story.first.comments.first.user.username`, which without
        // the generators answers with a name-based candidate list that merely happens to
        // contain the right target.
        //
        // Five links and four generators: the class side, a `has_many`, the relation it
        // returns, a `belongs_to`, and then a column — with the last two documents reopening
        // `class User` from two different files, which is the arrangement no other test here
        // puts together.
        let source = "Story.first.comments.first.user.username\n";
        let (mut harness, _story, uri) = models_project(source);
        // The shared fixture's `Comment` has no author, and lobsters' does — the link the
        // benchmark's chain turns on.
        let comment = harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\n  belongs_to :story\n  \
             belongs_to :user\n  has_many :comments\nend\n",
        );
        harness.watch(&[&comment]);
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"users\", force: :cascade do |t|\n    \
             t.string \"username\", null: false\n  end\nend\n",
        );
        harness.watch(&[&schema]);

        let card = card(&mut harness, &uri, source, "username");
        assert!(card.contains("User#username"), "{card}");
        assert!(!card.contains("guessed from the name"), "{card}");
        assert!(!card.contains("Matched on the method name alone"), "{card}");

        let definition = harness.definition_at(&uri, source, "username");
        assert_eq!(definition.as_array().map(Vec::len), Some(1), "{definition}");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str()),
            "{definition}"
        );
    }

    #[test]
    fn a_scope_is_a_class_method_and_its_body_is_never_read() {
        // The declarative rule at its hardest case. `-> { order(created_at: :desc) }` is Ruby
        // that only runs, and this reads the macro's *name* and the class it is written in and
        // nothing else — which is enough, because a scope returns a relation of its own class
        // whatever the lambda does.
        let source = "Story.recent.first.user\n";
        let (mut harness, _story, uri) = models_project(source);

        assert!(
            harness.has("Story::<Story>#recent()"),
            "a scope is a singleton method"
        );
        let card = card(&mut harness, &uri, source, "user");
        assert!(card.contains("Story#user"), "{card}");
    }

    #[test]
    fn an_association_added_after_the_index_answers_and_a_deleted_one_stops() {
        // Both directions of the same property the schema pass has: the generators run before
        // every resolve, so a macro typed into a model is answerable on the next settle and one
        // deleted from it takes its member with it.
        let (mut harness, _story, _uri) = models_project("");
        assert!(!harness.has("User#stories()"));

        let user = harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\n  has_many :stories\nend\n",
        );
        harness.watch(&[&user]);
        assert!(harness.has("User#stories()"));

        let user = harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.watch(&[&user]);
        assert!(!harness.has("User#stories()"));
    }

    #[test]
    fn a_macro_that_is_not_a_statement_of_the_class_body_is_not_read() {
        // The bounding rule, sharpened for the one block that is a host. `included do` is
        // `ActiveSupport::Concern`'s and exists on a **module**; written in a `class` body it is
        // a `NoMethodError`, and 0 of the corpus' 147 macro-bearing ones are in one. Being
        // *inside* a module does not make a class body a module body.
        let (mut harness, _story, _uri) = models_project("");
        let concern = harness.write(
            "app/models/concerns/taggable.rb",
            "module Taggable\n  class Holder\n    included do\n      has_many :tags\n    end\n  end\nend\n",
        );
        harness.watch(&[&concern]);

        assert!(!harness.has("Taggable::Holder#tags()"));
        assert!(!harness.has("Taggable#tags()"));
    }

    // ---------------------------------------------------------------------------------------
    // What a human wrote the type down as
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_sig_block_and_a_yard_tag_each_type_a_receiver_and_say_which_they_read() {
        // Both halves of an annotation. A card says which of the two it read, because a
        // comment is not a signature and the tiers already have a place to say so.
        let source = "Widget.new.name.upcase\nWidget.new.label.upcase\n";
        let (mut harness, _story, uri) = models_project(source);
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             extend T::Sig\n\n  \
             sig { returns(String) }\n  \
             def name\n    \"x\"\n  end\n\n  \
             # @return [String]\n  \
             def label\n    \"y\"\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        let sorbet = card(&mut harness, &uri, source, "name");
        assert!(sorbet.contains("a Sorbet `sig` block"), "{sorbet}");
        assert!(sorbet.contains("app/models/widget.rb"), "{sorbet}");

        let yard = card(&mut harness, &uri, source, "label");
        assert!(yard.contains("a YARD `@return` tag"), "{yard}");

        // And both type the chain, which is the only reason to read either.
        let chained = card(&mut harness, &uri, "Widget.new.name.upcase\n", "upcase");
        assert!(chained.contains("String#upcase"), "{chained}");
    }

    #[test]
    fn an_annotation_is_not_a_second_place_the_method_is_declared() {
        // The `def` is already in the graph at the offset an editor should jump to, so what item
        // 14 adds is a *type* and nothing else. A span here would put the same location in a
        // go-to-definition list twice.
        let source = "Widget.new.name\n";
        let (mut harness, _story, uri) = models_project(source);
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  # @return [String]\n  def name\n    \"x\"\n  end\nend\n",
        );
        harness.watch(&[&widget]);

        let definition = harness.definition_at(&uri, source, "name");
        assert_eq!(definition.as_array().map(Vec::len), Some(1), "{definition}");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(widget.as_str()),
            "{definition}"
        );
    }

    #[test]
    fn a_sorbet_sig_wins_over_a_yard_tag_that_disagrees_with_it() {
        // Both are "a human wrote the type down" and one of them is machine-checked. A `sig` is
        // Ruby the parser validates and `srb` checks; a comment rots quietly.
        let (mut harness, _story, _uri) = models_project("");
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             # @return [Integer]\n  \
             sig { returns(String) }\n  \
             def name\n    \"x\"\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        let rbs = harness.generated_rbs("app/models/widget.rb");
        assert!(rbs.contains("def name: () -> String"), "{rbs}");
    }

    #[test]
    fn an_annotation_naming_something_that_is_not_a_class_declares_nothing() {
        // The decline list, as behaviour. A union `Types` cannot key, a duck type that names a
        // method rather than a class, and a shape — none of them has a spelling this crate can
        // write exactly, and a method it cannot spell exactly is one it says nothing about.
        let (mut harness, _story, _uri) = models_project("");
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             sig { returns(T.any(String, Integer)) }\n  \
             def either\n  end\n\n  \
             # @return [#read]\n  \
             def duck\n  end\n\n  \
             # @return [Hash{Symbol=>String}]\n  \
             def shape\n  end\n\n  \
             # @param count [Integer] how many\n  \
             def untagged(count)\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        assert_eq!(
            harness.generated_rbs("app/models/widget.rb"),
            String::new(),
            "a type this crate cannot spell exactly is a method it says nothing about"
        );
    }

    #[test]
    fn an_annotated_method_keeps_the_arity_it_was_written_with() {
        // An answer is partitioned by how many positional arguments the *call* wrote, so a
        // generated signature that claims the wrong arity does not merely display wrongly — it
        // answers nothing, or answers for a call nobody made. Every parameter shape Ruby has,
        // rendered, and then asked the only question that matters about it.
        let source = "Widget.new.go(1, 2, 3, key: 4).upcase\n";
        let (mut harness, _story, uri) = models_project(source);
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             # @return [String]\n  \
             def go(a, b = 1, *rest, key:, opt: 2, **kw, &block)\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        let rbs = harness.generated_rbs("app/models/widget.rb");
        assert!(
            rbs.contains(
                "def go: (untyped, ?untyped, *untyped, key: untyped, ?opt: untyped, **untyped) \
                 ?{ (*untyped) -> untyped } -> String"
            ),
            "{rbs}"
        );
        let card = card(&mut harness, &uri, source, "upcase");
        assert!(card.contains("String#upcase"), "{card}");
    }

    #[test]
    fn a_local_assigned_a_chain_that_does_not_type_falls_through_to_its_own_name() {
        // The fall-through, both directions, because on its own either half is a bug.
        //
        // A chain whose *shape* is sound and whose *type* nothing states. `published` is a
        // scope this `Story` does not declare, which is what a chain through any class method
        // nobody annotated looks like — and it is deliberately not `Story.where(...).first`,
        // because the model's class side types that one. The chain here genuinely fails even
        // with the class side in place, which is how the fall-through is known to be
        // load-bearing rather than shadowed.
        //
        // Before this, that answered *nothing*, while a `story` with no assignment at all
        // reached the name rung and answered `Story`. Writing the assignment made the answer
        // worse, which is the wrong shape for a system whose whole argument is that its rungs
        // are ordered.
        let source = "story = Story.published.first\nstory.comments\n";
        let (mut harness, _story, uri) = models_project(source);

        let fell_through = card(&mut harness, &uri, source, "comments");
        assert!(
            fell_through.contains("Story#comments"),
            "the failed chain has to end where a bare `story` ends: {fell_through}"
        );
        // Wearing the label of the rung it reached. The fall-through is a *step*, not a sixth
        // rung, so it must not launder a guess into a derivation on the way past.
        assert!(
            fell_through.contains("Type guessed from the name `story` alone"),
            "{fell_through}"
        );

        // The other direction, and it is the one that makes the first safe. `comment` would
        // guess `Comment`, which really does declare `comments` — so if the spelling were
        // asked before the assignment, this card would say `Comment#comments` and be wrong
        // about a chain the code states outright.
        let resolved = "comment = Story.new.parent_story\ncomment.comments\n";
        let other = harness.write("app/other.rb", resolved);
        harness.watch(&[&other]);
        let card = card(&mut harness, &other, resolved, "comments");
        assert!(card.contains("Story#comments"), "{card}");
        assert!(
            !card.contains("Comment#comments"),
            "a chain that resolves can never be displaced by a guess: {card}"
        );
        assert!(
            !card.contains("guessed from the name"),
            "and it keeps the tier it earned: {card}"
        );
    }

    #[test]
    fn the_fall_through_goes_off_with_the_rung_it_belongs_to() {
        // It reaches `types::named`, which is the same pair of rungs a bare name reaches — so
        // `[types] guess_from_names = false` turns it off with no switch of its own. A
        // fall-through that survived the setting would be a guess the user had already
        // declined, arriving by a route they had no name for.
        let source = "story = Story.where(published: true).first\nstory.title\n";

        let mut on = Harness::new();
        on.write("app/models/story.rb", STORY);
        let uri = on.write("app/main.rb", source);
        on.index();
        let guessed = card(&mut on, &uri, source, "title");
        assert!(guessed.contains("Story#title"), "{guessed}");
        assert!(
            guessed.contains("Type guessed from the name `story` alone"),
            "{guessed}"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n\
             [types]\nguess_from_names = false\n",
        )
        .unwrap();
        let mut off = Harness::at(dir, PositionEncoding::Utf16);
        off.write("app/models/story.rb", STORY);
        let uri = off.write("app/main.rb", source);
        off.index();
        let silenced = card(&mut off, &uri, source, "title");
        assert!(!silenced.contains("guessed from the name"), "{silenced}");
        assert!(
            silenced.contains("Matched on the method name alone"),
            "with the rung off there is nothing below the failed chain: {silenced}"
        );
    }

    #[test]
    fn a_column_completes_below_a_method_the_user_wrote_and_above_ruby_s_own() {
        // The ranking `synthesized.md` says to pin with a real schema in front of it rather
        // than discover from a bug report. A generated document is not the user's
        // own code, so `completion::Locality` scores it the way it scores a gem's signature —
        // and the order that falls out is the right one: what this class was written to do,
        // then what its table says it holds, then what every object can do.
        let source = "Story.new.\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let model = harness.write(
            "app/models/story.rb",
            "class Story\n  def summary\n  end\nend\n",
        );
        harness.watch(&[&model]);

        assert_eq!(
            harness.declarations_at(&uri, "Story.new.~\n"),
            vec!["summary", "description", "id", "tags", "title", "tap"]
        );
    }

    #[test]
    fn a_table_no_class_claims_declares_nothing() {
        // The direction class → table, in the case that decides it. `widgets` is a table with
        // a column, and there is no `Widget` — so nothing is declared, rather than a class
        // being invented for it or the table being singularized onto something that exists.
        let (harness, _schema, _uri) = rails_project("Story.new\n");
        assert!(harness.has("Story#title()"));
        assert!(!harness.has("Widget#name()"));
        assert!(harness.analysis.graph.get("Widget").is_none());
    }

    #[test]
    fn a_nested_class_claims_a_table_only_when_it_is_a_model() {
        // "A nested class claims nothing" is too coarse: it claims what Rails
        // says it claims", and the superclass is the whole of the new gate. A `class Story`
        // inside `module Legacy` that is not an ActiveRecord model must still claim nothing, or
        // the schema's columns land on a service object that never reads one — which is
        // measured: the six corpora hold 43 such classes whose name inflects onto a real table.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let nested = harness.write(
            "app/models/admin/story.rb",
            // The second is two hops from the base rather than one, which is the shape
            // solidus writes 101 of and the reason `is_model` climbs rather than asks once.
            "module Admin\n  class Story < ApplicationRecord\n  end\nend\n\n\
             class Base < ApplicationRecord\nend\n\n\
             module Legacy\n  class Story < Base\n  end\nend\n\n\
             module Service\n  class Story\n  end\nend\n",
        );
        // And the spelling of "top level" that says so outright.
        let widget = harness.write("app/models/widget.rb", "class ::Widget\nend\n");
        harness.watch(&[&nested, &widget]);

        assert!(harness.has("Story#title()"));
        assert!(harness.has("Widget#name()"));
        // `Admin` declares no prefix, so `compute_table_name` is the bare plural and all three
        // models really do read `stories`; a one-claimant rule lets only the top-level one
        // answer.
        assert!(harness.has("Admin::Story#title()"));
        assert!(harness.has("Legacy::Story#title()"));
        // The one that inherits nothing is not a model, and claims nothing.
        assert!(!harness.has("Service::Story#title()"));
    }

    #[test]
    fn a_namespace_that_declares_a_prefix_moves_every_table_under_it() {
        // `full_table_name_prefix` is `module_parents.detect { |p| p.respond_to?(...) }`, so a
        // `def self.table_name_prefix` on `Admin` says every model under it reads a table that
        // begins `admin_`. It is read out of a file, which is why the inflection happens where
        // the documents are and not where the definitions are.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"admin_stories\", force: :cascade do |t|\n    \
             t.string \"headline\", null: false\n  end\nend\n",
        );
        let admin = harness.write(
            "app/models/admin.rb",
            "module Admin\n  def self.table_name_prefix\n    \"admin_\"\n  end\nend\n",
        );
        let nested = harness.write(
            "app/models/admin/story.rb",
            "module Admin\n  class Story < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&schema, &admin, &nested]);

        assert!(harness.has("Admin::Story#headline()"));
        // And the bare plural is not also its: `stories` is a table this schema does not have,
        // but the claim would still have been wrong.
        assert!(!harness.has("Admin::Story#title()"));
    }

    #[test]
    fn a_namespace_that_declares_a_suffix_moves_every_table_under_it_too() {
        // `full_table_name_suffix` is the same `module_parents.detect`, and it measures **0**
        // in six applications. It is read anyway because it is the same syntax in the same
        // walk, and ignoring it is the only way this reader can name a table that exists and is
        // not the one the class reads.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"stories_v2\", force: :cascade do |t|\n    \
             t.string \"headline\", null: false\n  end\nend\n",
        );
        let legacy = harness.write(
            "app/models/legacy.rb",
            "module Legacy\n  def self.table_name_suffix\n    \"_v2\"\n  end\nend\n",
        );
        let nested = harness.write(
            "app/models/legacy/story.rb",
            "module Legacy\n  class Story < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&schema, &legacy, &nested]);

        assert!(harness.has("Legacy::Story#headline()"));
    }

    #[test]
    fn an_engine_that_isolates_a_namespace_declares_the_same_prefix() {
        // The commoner of the two spellings by a factor of nearly four — 36 of the 46
        // declarations in six corpora — and it is a call rather than a `def` because the engine says it about a
        // module somebody else wrote. `Rails::Engine#isolate_namespace` installs
        // `table_name_prefix` as `generate_railtie_name(mod.name)` and an underscore.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"spree_orders\", force: :cascade do |t|\n    \
             t.string \"number\", null: false\n  end\nend\n",
        );
        let engine = harness.write(
            "lib/spree/core/engine.rb",
            "module Spree\n  module Core\n    class Engine < ::Rails::Engine\n      \
             isolate_namespace Spree\n    end\n  end\nend\n",
        );
        let order = harness.write(
            "app/models/spree/order.rb",
            "module Spree\n  class Order < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&schema, &engine, &order]);

        assert!(harness.has("Spree::Order#number()"));
    }

    #[test]
    fn a_class_nested_inside_a_model_claims_nothing() {
        // `compute_table_name`'s other branch is `parent_singular_child_plural`, and it needs
        // the parent's own table and then the parent's parent's. Declining it is measured
        // rather than assumed: 22 classes in six applications are nested inside a model and not
        // one of them names a table any of those applications has.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let nested = harness.write(
            "app/models/widget.rb",
            "class Widget < ApplicationRecord\n  class Story < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&nested]);

        assert!(harness.has("Widget#name()"));
        assert!(!harness.has("Widget::Story#title()"));
    }

    #[test]
    fn a_nested_model_whose_namespace_nothing_declares_claims_nothing() {
        // The namespace rule, asked by the second generator to reach the shape. A generated
        // `class Reports::Metric` where nothing declares `Reports` costs that namespace its own
        // members silently, so the claim is declined rather than spelled. It declines nothing
        // in six corpora and is here because the damage it prevents cannot be seen.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        // `class Reports::Story` and no `module Reports` anywhere: Zeitwerk conjures the
        // namespace at run time and no file in the workspace declares it.
        let conjured = harness.write(
            "app/models/reports/story.rb",
            "class Reports::Story < ApplicationRecord\nend\n",
        );
        harness.watch(&[&conjured]);

        assert!(harness.has("Story#title()"));
        assert!(!harness.has("Reports::Story#title()"));
    }

    #[test]
    fn a_class_whose_superclass_is_nobody_the_workspace_defines_is_not_a_model() {
        // The other end of the same walk: a chain that runs out is not a model, and a chain
        // that runs in a circle is not an infinite loop. Neither shape can claim a table.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let odd = harness.write(
            "app/models/odd.rb",
            "module Legacy\n  class Story < Sinatra::Base\n  end\nend\n\n\
             module Circular\n  class Story < Other\n  end\n\n  class Other < Story\n  \
             end\nend\n",
        );
        harness.watch(&[&odd]);

        assert!(harness.has("Story#title()"));
        assert!(!harness.has("Legacy::Story#title()"));
        assert!(!harness.has("Circular::Story#title()"));
    }

    #[test]
    fn a_written_table_name_meets_the_class_whose_name_implies_it() {
        // The one place an inflected claim and a written one meet, and both readings are in
        // the corpus. mastodon's throwaway `MoveUserSettings::LegacySetting` says
        // `self.table_name = "settings"` and, if a written name simply replaces an inflected
        // one, *takes* those columns off
        // the `Setting` model that reads them; discourse's three test doubles do the same to
        // `Post`. But discourse's `TopicViewItem` says `topic_views` and the `TopicView` whose
        // name implies it is a plain view object that reads no table at all.
        //
        // So the guess survives the meeting only when the class it is about is a model.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let model = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // The other half of the fixture: a top-level class named after a table it does not read.
        let widget = harness.write("app/models/widget.rb", "class Widget\nend\n");
        let migration = harness.write(
            "db/migrate/20240101000000_backfill.rb",
            "class Backfill < ActiveRecord::Migration[7.1]\n  \
             class LegacyStory < ApplicationRecord\n    self.table_name = \"stories\"\n  \
             end\n\n  class WidgetRow < ApplicationRecord\n    \
             self.table_name = \"widgets\"\n  end\nend\n",
        );
        harness.watch(&[&model, &widget, &migration]);

        assert!(harness.has("Story#title()"), "the model lost its own table");
        assert!(harness.has("Backfill::LegacyStory#title()"));
        // And the class that is not a model does not keep a table somebody else named.
        assert!(harness.has("Backfill::WidgetRow#name()"));
        assert!(!harness.has("Widget#name()"));
    }

    #[test]
    fn an_anonymous_class_is_not_a_model_however_it_is_written() {
        // A defect only measurement finds, and it is reachable from two different
        // generators. rubydex names an anonymous `Class.new(ApplicationRecord)`
        // `<hash>:<offset><anonymous>`; that class is an ActiveRecord model by every rule this
        // crate has, so it asked for a relation — and `class ` + that name is not RBS, so
        // `Synthesized::record`'s parse gate threw away **the whole document**, taking every
        // real declaration in the file with it. Solidus writes 38 such specs.
        //
        // The assertion is on a *neighbour*: what proves the document survived is that the
        // class written beside the anonymous one still declares its own members.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "spec/models/thing_spec.rb",
            "class Thing < ApplicationRecord\n  has_many :parts\nend\n\n\
             thrown = -> { Class.new(ApplicationRecord) { def spin; end } }\n",
        );
        harness.write(
            "app/models/part.rb",
            "class Part < ApplicationRecord\nend\n",
        );
        harness.index();

        assert!(
            harness.has("Thing#parts()"),
            "the macro beside the anonymous class still declares"
        );
        assert!(
            harness.has("Thing::Relation"),
            "and so does the relation the same document writes"
        );
    }

    #[test]
    fn a_block_parameter_is_typed_by_what_the_method_says_it_yields() {
        // The block parameter, end to end. `Story::Relation#each` is declared
        // `() { (Story) -> void } -> Story::Relation`, and reading only the return **throws
        // the block half away**: a block parameter was typed only where its own name happened to
        // camelize onto a class, so `.each do |story|` answered by a *guess* and
        // `.each do |instance|` — which is what mastodon writes — answered nothing at all.
        let source = "Story.where(id: 1).each do |instance|\n  instance.title\nend\n";
        let (mut harness, schema, uri) = rails_project(source);
        let author = harness.write(
            "app/models/author.rb",
            "class Author < ApplicationRecord\n  has_many :stories\nend\n",
        );
        harness.watch(&[&author]);

        // The member is found, the card names the column's own file, and the jump lands on the
        // line of the schema that declared it — the same three things asked of a column
        // reached any other way.
        let card = card(&mut harness, &uri, source, "title");
        assert!(card.contains("Story#title"), "{card}");
        assert!(card.contains("db/schema.rb"), "{card}");
        assert!(
            !card.contains("Matched on the method name alone"),
            "still on the name rung: {card}"
        );

        let definition = harness.definition_at(&uri, source, "title");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str())
        );
    }

    #[test]
    fn a_model_reopened_to_nest_something_under_it_keeps_its_table() {
        // `claims` is filled per *definition*, so a model reopened in a second file pushed its
        // own name twice and "a table two classes claim is claimed by neither" then declined it
        // — losing every column on a model nothing was ambiguous about. The shape is the
        // ordinary Ruby idiom for namespacing a helper under a model: forem writes
        // `class AuditLog` again in `app/queries/audit_log/unpublish_alls_query.rb`, and
        // discourse writes `class Reviewable < ActiveRecord::Base` in **six** `lib/reviewable/`
        // files. **Three models in six corpora** were in that state, discourse's `Post` among
        // them, and none of the three is a collision.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let query = harness.write(
            "app/queries/story/recent_query.rb",
            "class Story\n  class RecentQuery\n  end\nend\n",
        );
        harness.watch(&[&query]);

        assert!(harness.has("Story#title()"));
    }

    #[test]
    fn two_nested_names_that_reach_one_table_claim_neither() {
        // The narrowed ambiguity rule, in the case it was built for. Several claimants are kept
        // when they demodulize alike — the same convention applied twice — and two *different*
        // names landing on one table is the inflector having got one of them wrong.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let rivals = harness.write(
            "app/models/rivals.rb",
            "module Legacy\n  class Storie < ApplicationRecord\n  end\nend\n",
        );
        harness.watch(&[&rivals]);

        assert!(!harness.has("Story#title()"), "an ambiguous table answered");
        assert!(!harness.has("Legacy::Storie#title()"));
    }

    #[test]
    fn two_classes_that_pluralize_to_one_table_claim_neither() {
        // An ambiguous answer is not an answer. Both classes are top level, both are the user's
        // own, and both name `stories` — so the columns go to neither of them.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let rival = harness.write("app/models/storie.rb", "class Storie\nend\n");
        harness.watch(&[&rival]);

        assert!(!harness.has("Story#title()"), "an ambiguous table answered");
        assert!(!harness.has("Storie#title()"));
    }

    #[test]
    fn a_model_that_names_its_own_table_reads_that_one_and_not_the_other() {
        // The documented escape, and the half of it that is easy to get wrong: a class that
        // says `self.table_name` must *stop* claiming the table its name implies, or one model
        // answers with two schemas at once.
        let (mut harness, _schema, _uri) = rails_project("Story.new\n");
        let model = harness.write(
            "app/models/story.rb",
            "class Story\n  self.table_name = \"widgets\"\nend\n",
        );
        harness.watch(&[&model]);

        assert!(harness.has("Story#name()"), "the named table was not read");
        assert!(
            !harness.has("Story#title()"),
            "and the table its name implies was read as well"
        );
    }

    #[test]
    fn a_model_added_after_the_index_makes_its_table_answer() {
        // The reason the generation runs before every resolve rather than when the schema file
        // changes: the *other* input is which classes exist, and `rails generate model` writes
        // a file that has nothing to do with `db/schema.rb`'s mtime.
        let (mut harness, _schema, _uri) = rails_project("Widget.new\n");
        assert!(!harness.has("Widget#name()"));

        let widget = harness.write("app/models/widget.rb", "class Widget\nend\n");
        harness.watch(&[&widget]);

        assert!(harness.has("Widget#name()"), "a new model was not noticed");
    }

    #[test]
    fn editing_the_schema_replaces_what_it_declared() {
        // The same failure the side table exists to prevent, now through the generator that
        // fills it: a column that is renamed must stop answering under its old name, and it is
        // silent when it does not.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        harness.open(&schema, SCHEMA_RB);
        harness.change(&schema, &SCHEMA_RB.replace("\"title\"", "\"headline\""));

        assert!(harness.has("Story#headline()"));
        assert!(
            !harness.has("Story#title()"),
            "the column that was renamed is still answering"
        );
        assert!(harness.definition_at(&uri, source, "title").is_null());
    }

    #[test]
    fn a_project_with_no_schema_generates_nothing_at_all() {
        // The cost a non-Rails project pays for all of the above, which has to be nothing: one
        // hash lookup per settle, no document, no entry in the table.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "class Story\nend\n");
        harness.index();

        assert!(harness.analysis.synthesized.is_empty());
        assert!(
            harness.definition_at(&uri, "class Story\nend\n", "Story")[0]["targetUri"].is_string()
        );
    }

    #[test]
    fn a_keystroke_in_a_file_the_pass_does_not_read_does_not_run_the_pass() {
        // `settle` runs this pass before every `resolve` and a forced settle sits in front of
        // every graph-reading request, so without a gate one keystroke in a file with no macro
        // in it pays for a whole-workspace regeneration — 262 ms on discourse, in front of
        // completion's own 10.
        //
        // Three assertions, and the middle one is the gate: nothing is skipped until a pass has
        // run, a keystroke in a file no generator opens skips it, and every answer is still
        // there afterwards.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        let plain = "class Plain\n  def a\n  end\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        let before = harness.analysis.passes;

        harness.open(&uri, plain);
        harness.change(&uri, "class Plain\n  def ab\n  end\nend\n");
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.passes, before,
            "the generators ran for a file none of them opens"
        );
        assert!(
            harness.has("Story#title()"),
            "and the columns are still there"
        );

        // The half a naive "does *this* document declare anything" test would get wrong: a new
        // class name is a new answer for every macro anywhere that names one, so the projection
        // moves and the pass runs.
        harness.change(&uri, "class Renamed\n  def a\n  end\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.passes > before,
            "a class the workspace did not have before is not nothing"
        );
    }

    #[test]
    fn a_keystroke_in_a_file_the_pass_does_not_read_does_not_walk_the_workspace_either() {
        // The property the per-document gate rests on: the fingerprint has to be a function of
        // **what the document contributes** and not of the document, proven by changing a body
        // without changing what it contributes.
        //
        // `passes` cannot see this — the outer gate already stops the generators, and what is
        // left is the projection they are handed being rebuilt in full to decide that. So
        // `walks` is the instrument, and the two assertions are the point: an edit that
        // rewrites most of a file walks nothing, and an edit that renames the class it defines
        // walks everything.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);

        let plain = "class Plain\n  def a\n  end\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        let walks = harness.analysis.walks;

        harness.open(&uri, plain);
        // A comment, a rename of a method, a local variable, a whole second method and a string
        // literal — everything a file is mostly made of, and not one of them is read by any
        // field of a `Contribution`.
        harness.change(
            &uri,
            "# what this class is for\nclass Plain\n  def ab\n    here = \"and gone\"\n    here\n  end\n\n  def second\n  end\nend\n",
        );
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.walks, walks,
            "the workspace was walked for a change no projection of it can see"
        );
        assert!(
            harness.has("Story#title()"),
            "and the columns are still there"
        );

        // The other half, and it is the same file: the one line in it the walk *does* read.
        harness.change(&uri, "class Renamed\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.walks > walks,
            "a class the workspace did not have before is not nothing"
        );
    }

    #[test]
    fn a_keystroke_in_a_model_file_runs_the_generators_and_does_not_walk_the_workspace() {
        // **The two gates must not share a clause.** "Would the walk produce the projection
        // already held" and "has a file some generator reads changed" are different questions,
        // and the second is right about the generators and says
        // nothing about the walk: `has_many :comments` becoming `has_many :tags` changes what
        // the file says and not one thing the projection is made of, so the generators must run
        // and the walk must not. On discourse the walk is 105 ms of every such keystroke.
        //
        // `walks` and `passes` together are the only instrument that can say so, and the two
        // assertions have to be made in the same breath: a pass that did not run would satisfy
        // the first one for the wrong reason.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        let comment = harness.write("app/models/comment.rb", "class Comment\nend\n");
        let tag = harness.write("app/models/tag.rb", "class Tag\nend\n");
        harness.watch(&[&story, &comment, &tag]);
        harness.open(
            &story,
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        assert!(harness.has("Story#comments()"));

        let (walks, passes) = (harness.analysis.walks, harness.analysis.passes);
        harness.change(
            &story,
            "class Story < ApplicationRecord\n  has_many :tags\nend\n",
        );

        assert_eq!(
            harness.analysis.walks, walks,
            "the workspace was walked for an edit that changed nothing the walk reads"
        );
        assert!(
            harness.analysis.passes > passes,
            "and the generators did not run for an edit that changed what a file declares"
        );
        assert!(harness.has("Story#tags()"), "the new macro took effect");
        assert!(
            !harness.has("Story#comments()"),
            "and the one it replaced stopped answering"
        );
    }

    #[test]
    fn a_keystroke_in_one_model_file_reads_that_file_and_no_other() {
        // The parse memo: a keystroke in a model file costs the reading of *that* file and
        // nothing else. Without it, one changed file makes the pass re-read and re-parse every
        // file on every list to learn what all but one of them said last time — 1,527 files and
        // 104 ms of Prism on discourse, every settle.
        //
        // `reads` is the instrument for the reason `passes` and `walks` are: the memo changes
        // no answer at all, and a file re-parsed into exactly the tree it already had is
        // indistinguishable from one that was not opened.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        let comment = harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\n  belongs_to :story\nend\n",
        );
        harness.watch(&[&story, &comment]);
        assert!(harness.has("Story#comments()"));

        // A settle over a workspace nobody touched reads nothing at all, which is the wider
        // claim: the memo is keyed by the file rather than by which file was edited.
        let reads = harness.analysis.reads;
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert_eq!(
            harness.analysis.reads, reads,
            "a settle over an unchanged workspace re-read files"
        );

        // Opening it is not a keystroke and does cost one read of one file: a stamp and a
        // buffer cannot be compared, so the authority changing hands is a miss by construction.
        // It happens once per open, of one file, and the alternative is hashing what is on disk
        // to find out — which is the read.
        harness.open(
            &story,
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        let reads = harness.analysis.reads;

        // And *then* one keystroke in one of them is one read. The edit changes what the file
        // declares, so the pass is not gated out — this is the pass running in full and opening
        // exactly one file.
        harness.change(
            &story,
            "class Story < ApplicationRecord\n  has_many :comments\n  has_many :tags\nend\n",
        );
        assert_eq!(
            harness.analysis.reads,
            reads + 1,
            "a keystroke in one model file read more than that file"
        );
        assert!(harness.has("Story#tags()"), "and the new macro took effect");
        assert!(
            harness.has("Comment#story()"),
            "and the file that was not re-read still declares what it declared"
        );
    }

    #[test]
    fn a_file_that_changes_on_disk_with_nobody_watching_is_read_again() {
        // The guarantee the memo is most able to lose: this pass claims the answer is a
        // function of what is **on disk**, and it earns that by re-reading everything every
        // time. A memo is exactly the thing that stops doing that.
        //
        // What replaces the re-read is the `stat` — `Fresh::Disk` is the gate's own stamp and
        // not a second mechanism — so a `git checkout` that rewrites a schema takes effect at the
        // very next settle, with no watcher notification anywhere in it. Nothing here calls
        // `watch`, which is the whole test.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        // Sleeping is not an option and does not have to be: `stamp_of` reads the length as
        // well as the modification time, precisely because a filesystem's clock is coarse and
        // two writes inside one tick are a real edit.
        std::fs::write(
            schema.to_path().expect("a file uri"),
            SCHEMA_RB.replace("\"title\"", "\"headline\""),
        )
        .expect("rewrite the schema");

        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert!(
            harness.has("Story#headline()"),
            "a schema rewritten behind the server's back never took effect"
        );
        assert!(
            !harness.has("Story#title()"),
            "and the column it replaced is still answering"
        );

        // And a file that is *gone* takes its parse with it rather than leaving one nothing can
        // refresh — the same guarantee at the other end, and the one that would leave a column
        // answering forever.
        std::fs::remove_file(schema.to_path().expect("a file uri")).expect("delete the schema");
        harness.analysis.dirty = true;
        harness.analysis.settle();
        assert!(
            !harness.has("Story#headline()"),
            "a schema deleted behind the server's back is still declaring columns"
        );
    }

    #[test]
    fn a_file_whose_name_ends_schema_rb_and_is_not_a_schema_is_never_read_as_one() {
        // The suffix is the cheap half of the test and `rails::is_schema` is the rule: a dump
        // lives in `db/`, so a file called `legacy_schema.rb` anywhere else is somebody's own
        // code that happens to end in those nine characters. It is on `List::Schemas` — the
        // `Wants` row matches the name — and the reader is never handed it, which is why the
        // filter sits where the pass decides what to *open* rather than where it declares.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        //
        // The decoy is a **copy of the real schema**, which is what makes one assertion enough:
        // a table two schema sources declare is declared by neither, so reading it would take
        // `stories` away from both and `Story#title` would stop answering. Asserting that some
        // table *only* the decoy holds is absent would prove nothing — nothing declares a class
        // to claim it either way.
        let decoy = harness.write("lib/legacy_schema.rb", SCHEMA_RB);
        harness.watch(&[&decoy]);

        assert!(
            harness.has("Story#title()"),
            "a file named like a schema and living outside db/ was read as one"
        );
    }

    #[test]
    fn a_file_that_joins_a_list_without_changing_is_read_for_the_reader_it_joined_for() {
        // The memo compares the readers an entry was built for and not only its text, and
        // this is the one shape that needs it. Every list but one is a function of a document's
        // own content; `List::Models` is not, because `Analysis::walk` adds to it, after the
        // walk, every **model** that writes no macro at all — and whether a class is a model
        // depends on a superclass chain that runs through *other files*. So `class Widget < Base`
        // joins the model list the moment a different file makes `Base` a model, with not one
        // byte of `widget.rb` changed.
        //
        // A memo keyed on the text alone would then serve an entry that was read for the
        // `@return` tag and never ran the model reader, and `Widget` would silently get no
        // relation class.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);
        let base = harness.write("app/models/base.rb", "class Base\nend\n");
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget < Base\n  # @return [String]\n  def label\n    \"x\"\n  end\nend\n",
        );
        harness.watch(&[&base, &widget]);
        assert!(harness.has("Widget#label()"), "the tag declared its method");
        assert!(
            !harness.has("Widget::Relation#first()"),
            "and nothing has made Widget a model yet"
        );

        harness.open(&base, "class Base\nend\n");
        let reads = harness.analysis.reads;
        harness.change(&base, "class Base < ApplicationRecord\nend\n");

        assert!(
            harness.has("Widget::Relation"),
            "the file that joined the model list was not read for the reader it joined for"
        );
        assert!(
            harness.has("Widget#label()"),
            "and what it was already read for is still declared"
        );
        // Two files: the one that was edited, and the one that joined a list because of it.
        assert_eq!(harness.analysis.reads, reads + 2);
    }

    #[test]
    fn a_document_the_walk_never_visits_is_never_skipped_by_it() {
        // The clause that makes eight bytes per document enough, and the one case it cannot
        // cover. `Analysis::contribution` declines an `.rbs` outright — a signature file has
        // `def`s and doc comments and would be handed to a reader that parses Ruby — but
        // `bundle_namespaces` reads **every** definition in the graph, so a `module` written in
        // the project's own `sig/` can still move a `Context`. A document with no contribution
        // is therefore not a document with an unchanged one, and the gate refuses it.
        let source = "Story.new.title\n";
        let (mut harness, _schema, _uri) = rails_project(source);

        let signature = "class Plain\nend\n";
        let uri = harness.write("sig/plain.rbs", signature);
        harness.index();
        harness.analysis.settle();
        let walks = harness.analysis.walks;

        harness.open(&uri, signature);
        harness.change(&uri, "class Plain\n  def a: () -> String\nend\n");
        harness.analysis.settle();
        assert!(
            harness.analysis.walks > walks,
            "a signature file was treated as contributing nothing rather than as unreadable"
        );
    }

    #[test]
    fn the_gate_still_looks_at_the_disk_rather_than_trusting_a_notification() {
        // The guarantee the gate must not weaken, and the suite is what catches it: this
        // pass claims the answer is a function of what is on disk, and it earns that by
        // re-reading everything every time. A `git checkout` that deletes a `db/schema.rb`
        // sends no notification until the watcher gets round to it, so a gate that trusted
        // notifications alone would keep answering with columns of a file that is gone.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        let plain = "class Plain\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();

        // Behind the server's back, and then a keystroke somewhere unrelated.
        std::fs::remove_file(schema.to_path().unwrap()).unwrap();
        harness.open(&uri, plain);
        harness.change(&uri, "class Plain\n  def a\n  end\nend\n");
        harness.analysis.settle();

        assert!(
            !harness.has("Story#title()"),
            "a schema that is gone still declares its columns"
        );
    }

    #[test]
    fn a_structure_dump_that_appears_is_not_a_document_and_is_looked_for_anyway() {
        // The one input to the pass that is **not** a projection of the graph —
        // `db/*structure.sql` — and therefore the one thing the fingerprints cannot cover:
        // a `.sql` is not a document, so no `WANTS` row can reach one and no contribution can
        // move when one appears. A `git checkout` that brings one in sends no notification the
        // gate may trust, exactly as the deletion in the test above sends none.
        let source = "Story.new.title\n";
        let (mut harness, schema, _uri) = rails_project(source);
        std::fs::remove_file(schema.to_path().unwrap()).unwrap();

        let plain = "class Plain\nend\n";
        let uri = harness.write("app/lib/plain.rb", plain);
        harness.index();
        harness.analysis.settle();
        assert!(!harness.has("Story#title()"), "the schema is gone");

        // Behind the server's back, and then a keystroke in a file that declares nothing.
        std::fs::write(
            harness.root.path().join("db/structure.sql"),
            "CREATE TABLE stories (\n  title character varying\n);\n",
        )
        .unwrap();
        harness.open(&uri, plain);
        harness.change(&uri, "class Plain\n  def a\n  end\nend\n");
        harness.analysis.settle();

        assert!(
            harness.has("Story#title()"),
            "a dump that appeared was never looked for"
        );
    }

    #[test]
    fn a_schema_that_vanished_under_the_index_declares_nothing() {
        // Indexed and readable are two different questions, and the gap between them is real:
        // a `git checkout` removes the file, and the next settle runs before the watcher
        // notification does. Nothing to read means nothing to say — not the last thing it said.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = rails_project(source);
        assert!(harness.has("Story#title()"));

        std::fs::remove_file(schema.to_path().unwrap()).unwrap();
        harness.open(&uri, source);
        harness.change(&uri, source);

        assert!(harness.analysis.synthesized.is_empty());
        assert!(!harness.has("Story#title()"));
    }

    #[test]
    fn a_schema_the_configuration_excludes_is_not_read() {
        // Not indexed and not read are the same sentence. A project that narrowed
        // `index.include` has said what it wants looked at, and this is not the place to
        // overrule it.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\nenabled = false\n\n[index]\ninclude = [\"app/**/*.rb\"]\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write("db/schema.rb", SCHEMA_RB);
        harness.index();

        assert!(harness.analysis.synthesized.is_empty());
        assert!(!harness.has("Story#title()"));
    }

    #[test]
    fn ruby_keeps_initialize_private_however_it_was_declared() {
        let mut harness = Harness::new();
        let item = harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // `rb_add_method` privatises five names at the point of definition, so `def initialize`
        // is private whatever its class said — and nothing in the graph records that. rbs is not
        // even consistent about it: `Kernel#initialize_copy` is marked private and
        // `String#initialize_copy` public, so `"hi".` offered `initialize` and `initialize_copy`
        // while `count.` offered only the first.
        let outside = harness.declarations_at(&uri, "Store::Item.new.~\n");
        assert!(outside.contains(&"price".to_owned()), "{outside:?}");
        assert!(!outside.contains(&"initialize".to_owned()), "{outside:?}");

        // And the guard, so this cannot pass by banning the name outright: Ruby 2.7 onwards
        // permits a private call on a receiver written `self`, `initialize` included.
        let inside =
            harness.declarations_at(&item, &ANCESTRY.replace("      audit\n", "      self.~\n"));
        assert!(inside.contains(&"initialize".to_owned()), "{inside:?}");
    }

    #[test]
    fn a_private_method_needs_a_receiver_written_self() {
        let mut harness = Harness::new();
        let item = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // rubydex passes a private method whenever the caller's `self` is the same *class* as
        // the receiver. Ruby's exemption is for the receiver being *written* `self`, so this
        // offered `stash` from inside `Item` where a real interpreter raises `NoMethodError`.
        let other = harness.declarations_at(
            &item,
            &ANCESTRY.replace("      audit\n", "      Store::Item.new.~\n"),
        );
        assert!(other.contains(&"price".to_owned()), "{other:?}");
        assert!(!other.contains(&"stash".to_owned()), "{other:?}");

        // `::` is a method call too, and Ruby exempts it on the same terms — both halves
        // checked against a real interpreter rather than assumed.
        for marked in ["      self.~\n", "      self::~\n"] {
            let found = harness.declarations_at(&item, &ANCESTRY.replace("      audit\n", marked));
            assert!(found.contains(&"stash".to_owned()), "{marked}: {found:?}");
        }
    }

    #[test]
    fn visibility_is_the_callers_visibility_not_the_methods() {
        // Private is free from rubydex, but only if the `self` it is handed is the caller's.
        // Left unstated it defaults to nothing, every call site becomes an outsider, and a
        // class stops being able to see its own private methods.
        let source = "\
class Person
  def self.build
  end

  class << self
    private

    def secret_factory
    end
  end
end
";
        let mut harness = Harness::new();
        let person = harness.write("app/person.rb", source);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let outside = harness.declarations_at(&uri, "Person.~\n");
        assert!(outside.contains(&"build".to_owned()), "{outside:?}");
        assert!(
            !outside.contains(&"secret_factory".to_owned()),
            "{outside:?}"
        );

        let inside = harness.declarations_at(
            &person,
            &source.replace("  def self.build\n", "  def self.build\n    self.~\n"),
        );
        assert!(inside.contains(&"secret_factory".to_owned()), "{inside:?}");
    }

    #[test]
    fn a_singleton_method_body_completes_against_the_singleton() {
        // Inside `def self.build` the lexical scope is `Person` but `self` is `Person`'s
        // singleton class, and the two produce different lists. Getting this wrong offers
        // instance methods that would raise `NoMethodError` if accepted.
        let mut harness = Harness::new();
        let uri = harness.write("app/hr.rb", OFFICE);
        harness.index();

        let found =
            harness.declarations_at(&uri, &OFFICE.replace("      new\n", "      new\n      ~\n"));
        assert!(found.contains(&"build".to_owned()), "{found:?}");
        assert!(!found.contains(&"shout".to_owned()), "{found:?}");
        // Constants still come from the lexical scope, which has not moved.
        assert!(found.contains(&"NAME_LIMIT".to_owned()), "{found:?}");
    }

    #[test]
    fn a_class_body_completes_against_the_class_and_not_an_instance_of_it() {
        // Where the entire Rails DSL lives. `self` in a class body is the class object, so what
        // can be written there is its *singleton* methods — `validates`, `has_many`, `scope`.
        // Completing against the instance side offers `valid?` and omits every macro.
        const MODEL: &str = "\
class Base
  def self.validates(*names)
  end

  def valid?
  end
end

class Post < Base
end
";
        let mut harness = Harness::new();
        let uri = harness.write("app/model.rb", MODEL);
        harness.index();

        let body = harness.declarations_at(
            &uri,
            &MODEL.replace("class Post < Base\n", "class Post < Base\n  vali~\n"),
        );
        assert!(body.contains(&"validates".to_owned()), "{body:?}");
        assert!(!body.contains(&"valid?".to_owned()), "{body:?}");

        // And an instance method body is the other way round, which is the whole distinction.
        let method = harness.declarations_at(
            &uri,
            &MODEL.replace(
                "class Post < Base\n",
                "class Post < Base\n  def go\n    vali~\n  end\n",
            ),
        );
        assert!(method.contains(&"valid?".to_owned()), "{method:?}");
        assert!(!method.contains(&"validates".to_owned()), "{method:?}");
    }

    #[test]
    fn an_argument_list_offers_the_keywords_the_method_takes() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // In the order the signature declares them, not alphabetically: a signature is a list
        // and its order is information, unlike a namespace's members.
        let found = harness.declarations_at(&uri, "HR::Person.build(~)\n");
        assert_eq!(&found[..2], ["name:", "age:"], "{found:?}");
    }

    #[test]
    fn keyword_arguments_are_never_guessed_from_a_name() {
        // `person` is a local, so the call resolves by name alone and could be any `build` in
        // the project. Offering `name:` there would be a syntactically valid wrong answer, so
        // the argument list degrades to a plain expression instead.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "person = nil\nperson.build(~)\n");
        assert!(!found.contains(&"name:".to_owned()), "{found:?}");
    }

    #[test]
    fn a_receiver_with_no_type_falls_back_to_every_method_name() {
        // The one context ya-lsp cannot answer exactly. It answers with names rather than
        // nothing, because the editor's own word list cannot see a method in an unopened file.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "person = nil\nperson.sh~\n");
        assert_eq!(found, vec!["shout"], "{found:?}");
    }

    #[test]
    fn a_name_defined_by_two_classes_is_offered_once() {
        let mut harness = Harness::new();
        harness.write(
            "app/dup.rb",
            "class One\n  def render\n  end\nend\n\nclass Two\n  def render\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "thing = nil\nthing.rend~\n");
        assert_eq!(found, vec!["render"], "{found:?}");
    }

    #[test]
    fn the_users_own_code_outranks_a_gems() {
        // The same call `workspace/symbol` makes, for the same reason: a project has thousands
        // of declarations and its bundle has a hundred times that.
        let (dir, _gem_home, env) =
            project_with_gem("class Megaphone\n  def blare_loudly\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "app/own.rb",
            "class Speaker\n  def blare_softly\n  end\nend\n",
        );
        harness.index();
        harness.index_gems();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "thing = nil\nthing.blare~\n");
        assert_eq!(found, vec!["blare_softly", "blare_loudly"], "{found:?}");
    }

    #[test]
    fn a_leading_scope_operator_offers_the_top_level_and_only_constants() {
        // rubydex's namespace walk stops before `Object`'s own members, so asking it about
        // `Object` directly answers with nothing. `::` has to be asked as an expression and
        // then filtered, and the filter is not cosmetic: a method or a keyword after `::` is
        // not valid Ruby.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.write(
            "app/top.rb",
            "TOP_LEVEL = 1\n\nclass Standalone\n  def lonely\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.suggestions(&uri, "::~\n");
        assert!(found.contains(&"HR".to_owned()), "{found:?}");
        assert!(found.contains(&"TOP_LEVEL".to_owned()), "{found:?}");
        assert!(found.contains(&"Standalone".to_owned()), "{found:?}");
        assert!(!found.contains(&"lonely".to_owned()), "{found:?}");
        assert!(!found.contains(&"def".to_owned()), "{found:?}");

        assert_eq!(
            harness.suggestions(&uri, "::Standal~\n"),
            vec!["Standalone"]
        );
    }

    #[test]
    fn a_name_meant_to_be_left_alone_sinks() {
        // Ruby writes "internal" with an underscore, and underscores sort before letters — so
        // without the rule the first thing a Rails user sees after `Model.` is `__send`.
        let mut harness = Harness::new();
        harness.write(
            "app/thing.rb",
            "class Thing\n  def self.build\n  end\n\n  def self._internal\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(
            harness.declarations_at(&uri, "Thing.~\n")[..2],
            ["build", "_internal"]
        );
        // Typing the underscore means it: someone who writes `_` is asking for exactly these.
        assert_eq!(
            harness.declarations_at(&uri, "Thing._~\n"),
            vec!["_internal"]
        );
    }

    #[test]
    fn a_name_rubydex_invented_is_never_offered() {
        // `Class.new` gets called `<uri>:<offset><anonymous>`, which is not something anyone can
        // type. Measured in a 17,557-file workspace, `::` answered with a page of them.
        let mut harness = Harness::new();
        harness.write(
            "app/dyn.rb",
            "Widget = Class.new do\n  def spin\n  end\nend\n\nclass Named\n  class << self\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.suggestions(&uri, "::~\n");
        assert!(found.contains(&"Named".to_owned()), "{found:?}");
        assert!(found.iter().all(|label| !label.contains('<')), "{found:?}");
        // And the same names are kept out of the symbol picker, which shares the test.
        assert!(
            harness
                .symbol_names("anonymous")
                .iter()
                .all(|name| !name.contains('<')),
        );
    }

    #[test]
    fn keywords_are_offered_in_an_expression_and_nowhere_else() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert!(
            harness
                .suggestions(&uri, "de~\n")
                .contains(&"def".to_owned())
        );
        // After a `.` a keyword is not a legal thing to write.
        assert!(
            !harness
                .suggestions(&uri, "HR::Person.de~\n")
                .contains(&"def".to_owned())
        );
    }

    #[test]
    fn a_comment_is_answered_with_null_and_an_empty_scope_with_a_list() {
        // Two different "nothing", and the difference is not cosmetic: `null` tells the client
        // to fall back to its own word list, an empty list tells it not to.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "");

        assert!(harness.complete(&uri, "# take ~\n").is_null());
        assert!(harness.complete(&uri, "\"a string ~\"\n").is_null());

        let empty = harness.complete(&uri, "Nowhere::~\n");
        assert_eq!(empty["items"], serde_json::json!([]));
    }

    #[test]
    fn the_half_typed_word_is_replaced_rather_than_appended_to() {
        // Without an explicit edit range the client guesses the word boundaries from its own
        // pattern, and Ruby's `?`, `!` and `@` are exactly where that guess goes wrong.
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "class Person\n  def empty?\n  end\n\n  def go\n    @name = 1\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "thing = nil\nthing.empty?~\n");
        let edit = &found["items"][0]["textEdit"];
        assert_eq!(edit["newText"], "empty?");
        assert_eq!(
            edit["range"],
            serde_json::json!({
                "start": { "line": 1, "character": 6 },
                "end": { "line": 1, "character": 12 },
            }),
            "{found}"
        );
    }

    #[test]
    fn a_list_the_cap_did_not_touch_is_complete() {
        // The flag costs the client a whole request per keystroke, so it is set where it is
        // load-bearing rather than everywhere: only the cap can drop a row that a longer prefix
        // would have reached, because every filter on the way here is a subsequence match.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(harness.complete(&uri, "HR::~\n")["isIncomplete"], false);
        assert_eq!(harness.complete(&uri, "sh~\n")["isIncomplete"], false);
    }

    #[test]
    fn a_list_with_nothing_in_it_is_still_incomplete() {
        // Not the same statement as the one above read twice. A route that answers with no rows
        // does so because there was nothing to say, and "the complete answer is nothing" would
        // have the client stop asking as the word grows.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "Undefined::~\n");
        assert_eq!(found["items"].as_array().map(Vec::len), Some(0), "{found}");
        assert_eq!(found["isIncomplete"], true, "{found}");
    }

    #[test]
    fn a_completion_list_is_capped() {
        let mut harness = Harness::new();
        let mut source = String::new();
        for index in 0..MAX_COMPLETION_ITEMS + 100 {
            source.push_str(&format!("class Thing{index}\nend\n"));
        }
        harness.write("app/many.rb", &source);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "Thing~\n");
        assert_eq!(
            found["isIncomplete"], true,
            "a list the cap cut is the one case the client must re-ask about"
        );
        assert_eq!(
            found["items"].as_array().map(Vec::len),
            Some(MAX_COMPLETION_ITEMS)
        );
    }

    #[test]
    fn resolving_an_item_fills_in_its_documentation() {
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "class Person\n  # Says it loudly.\n  def shout(volume)\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "thing = nil\nthing.shou~\n");
        let item = found["items"][0].clone();
        // The list itself carries no documentation: five hundred rows, one of them read.
        assert!(item["documentation"].is_null(), "{item}");

        let resolved = harness.ask("completionItem/resolve", item);
        let markdown = resolved["documentation"]["value"]
            .as_str()
            .unwrap_or_default();
        assert!(markdown.contains("Says it loudly"), "{resolved}");
        assert!(markdown.contains("shout(volume)"), "{resolved}");
    }

    #[test]
    fn resolving_an_item_the_graph_no_longer_holds_answers_the_item_it_was_given() {
        // A list is built, the configuration reloads, the graph is dropped and rebuilt, and the
        // user then arrows down onto a row from the old list. The `data` on that row is a
        // declaration id nothing answers to any more — which arrives from the client, so it is
        // handed to the server rather than produced by it, and rubydex ids are hashes with no
        // way to tell a stale one from a wrong one. The protocol says the item comes back
        // either way; enriching it is the optional half.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", "class Person\nend\n");
        harness.index();

        for data in [
            // A well-formed row naming a declaration that is gone.
            serde_json::json!({ "declaration": "1234567890123456789", "precise": true }),
            // And the shapes a client can send that are not rows at all.
            serde_json::json!("1234567890123456789"),
            serde_json::json!({ "declaration": "1234567890123456789" }),
            serde_json::Value::Null,
        ] {
            let resolved = harness.ask(
                "completionItem/resolve",
                serde_json::json!({ "label": "shout", "data": data }),
            );

            assert_eq!(resolved["label"], "shout", "the item comes back regardless");
            assert!(
                resolved["documentation"].is_null(),
                "and carries nothing invented: {resolved}"
            );
        }
    }

    #[test]
    fn a_definition_whose_file_is_gone_answers_nothing_rather_than_a_dead_link() {
        // The graph still holds the declaration and still knows which file it was written in.
        // Turning that into a `LocationLink` needs the text, to place two ranges in it, and a
        // file deleted since indexing has none — the same situation
        // `diagnostics_are_skipped_for_a_file_that_has_gone_from_disk` covers from the other
        // end. Every site failing leaves an empty list, which must be answered as `null`: a
        // client handed `[]` opens an empty peek window instead of saying nothing was found.
        let mut harness = Harness::new();
        let person = harness.write("app/person.rb", "class Person\nend\n");
        let source = "Person.new\n";
        let main = harness.write("app/main.rb", source);
        harness.index();
        assert!(
            !harness.definition_at(&main, source, "Person").is_null(),
            "the jump works while the file is there"
        );

        std::fs::remove_file(person.to_path().expect("a path")).unwrap();

        assert!(
            harness.definition_at(&main, source, "Person").is_null(),
            "and answers nothing once it is not"
        );
    }

    // -----------------------------------------------------------------------
    // Signature help
    // -----------------------------------------------------------------------

    /// One class carrying every parameter kind, for the one request that is *about* parameters.
    ///
    /// `initialize` rather than an ordinary method, so that `Person.new(` — the call a user
    /// makes far more often than any other — is pinned by the same fixture that pins the
    /// rendering. `shout` exists to be called on a receiver nothing can type, which is the
    /// answer that has to be `null`.
    const CALLS: &str = "\
class Person
  # Make one.
  def initialize(name, age = 18, *nicknames, admin: false, **extra, &block)
  end

  # Build one.
  def self.build(name, sep:)
  end

  def self.locate(x, y)
  end

  def self.tag(**attributes)
  end

  class << self
    attr_reader :registry
  end

  def shout(volume)
  end
end
";

    fn calling(marked: &str) -> String {
        format!("{CALLS}{marked}")
    }

    #[test]
    fn a_call_shows_the_method_it_reaches_with_the_argument_being_written_underlined() {
        // The whole card, drawn: the label, the span under the parameter, and the comment. Two
        // separate things are pinned by the underline sitting where it does — that `render`
        // spells each parameter kind the way Ruby writes it, and that the offsets it hands back
        // land on the piece of the label they were computed for. Asserting the numbers instead
        // would pass just as happily with the underline three characters to the left.
        //
        // And `Person.new` is answered with `Person#initialize`. `Class#new` is the exact
        // answer and a useless one — the parameters the call actually takes are the
        // constructor's — which is the redirect `locator` already makes for hover and
        // navigation, reaching signature help through the same door.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert_eq!(
            harness.signature_card(&uri, &calling("Person.new(~)\n")),
            "Person#initialize(name, age = ..., *nicknames, admin: ..., **extra, &block)\n\
             \u{20}                 ~~~~\n\
             Make one."
        );
    }

    #[test]
    fn the_underline_follows_the_cursor_from_one_argument_to_the_next() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        let underline = |harness: &mut Harness, call: &str| {
            harness
                .signature_card(&uri, &calling(call))
                .lines()
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };

        // `(name, age = ..., *nicknames, admin: ..., **extra, &block)` from column 17.
        assert_eq!(
            underline(&mut harness, "Person.new(~)\n"),
            " ".repeat(18) + "~~~~"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", ~)\n"),
            " ".repeat(24) + "~~~~~~~~~",
            "the second argument is `age = ...`"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", 30, ~)\n"),
            " ".repeat(35) + "~~~~~~~~~~",
            "and the third is the splat"
        );
        // The rule the splat exists for: everything positional after it goes into it, so
        // counting straight through would walk off the end of a method that cannot be
        // over-called. This is the fifth argument and it is still `*nicknames`.
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", 30, \"a\", \"b\", ~)\n"),
            " ".repeat(35) + "~~~~~~~~~~",
            "and so is the fifth"
        );
    }

    #[test]
    fn a_keyword_argument_is_found_by_name_and_an_unknown_one_lands_in_the_splat() {
        // Keywords are written in any order, so the count that answers a positional argument
        // answers the wrong parameter for a keyword the moment anybody reorders two. The name
        // is the only thing that identifies one — and a name the method does not declare is
        // what `**extra` is for, which is where it is shown going.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        let underline = |harness: &mut Harness, call: &str| {
            harness
                .signature_card(&uri, &calling(call))
                .lines()
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };

        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", admin: ~)\n"),
            " ".repeat(47) + "~~~~~~~~~~",
            "`admin: ...`"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(admin: true, ~)\n"),
            " ".repeat(47) + "~~~~~~~~~~",
            "still `admin:`, one argument later"
        );
        assert_eq!(
            underline(&mut harness, "Person.new(\"ada\", nickname: ~)\n"),
            " ".repeat(59) + "~~~~~~~",
            "a keyword the method never declared is `**extra`'s"
        );
        // With no `**opts` to fall into, an undeclared keyword still belongs to the keyword
        // half of the signature rather than to a positional parameter it cannot be passed as.
        assert_eq!(
            harness.signature_card(&uri, &calling("Person.build(\"ada\", bogus: ~)\n")),
            "Person.build(name, sep:)\n\u{20}                  ~~~~\nBuild one."
        );
        // And a method whose only keyword is the splat is where an unnamed one lands too.
        assert_eq!(
            harness.signature_card(&uri, &calling("Person.tag(id: 1, ~)\n")),
            "Person.tag(**attributes)\n\u{20}          ~~~~~~~~~~~~"
        );
    }

    #[test]
    fn a_singleton_method_is_named_the_way_it_is_called() {
        // `Person::<Person>#build()` is rubydex's spelling and nobody's Ruby. Signature help
        // goes through `render::qualified_name` for the same reason hover and the outline do:
        // a construct that reads one way in one card and another way in the next is a bug.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert_eq!(
            harness.signature_card(&uri, &calling("Person.build(~)\n")),
            "Person.build(name, sep:)\n\
             \u{20}            ~~~~\n\
             Build one."
        );
    }

    #[test]
    fn the_innermost_call_is_the_one_being_written() {
        // A cursor inside a nested call's parentheses belongs to the inner call, and it costs
        // nothing here: the walk that finds the enclosing argument list is pre-order, so the
        // innermost claimant is the last to write itself down.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert_eq!(
            harness.signature_card(&uri, &calling("Person.new(Person.build(~))\n")),
            "Person.build(name, sep:)\n\
             \u{20}            ~~~~\n\
             Build one."
        );
        // And back out again, with the inner call now one finished argument.
        assert_eq!(
            harness
                .signature_card(
                    &uri,
                    &calling("Person.new(Person.build(\"ada\", sep: \",\"), ~)\n")
                )
                .lines()
                .next()
                .unwrap_or_default(),
            "Person#initialize(name, age = ..., *nicknames, admin: ..., **extra, &block)"
        );
    }

    #[test]
    fn a_receiver_nothing_can_name_is_answered_with_nothing() {
        // The rule keyword-argument completion already applies, for the reason the README
        // states: `person.shout` matches on the name alone, and another class's parameter list
        // under the cursor while the user types into it is a syntactically valid wrong answer.
        // Absent beats wrong here — the editor falls back to showing nothing at all.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        for marked in [
            "person = whatever\nperson.shout(~)\n",
            // The graph names a *constant* receiver and nothing else, so an instance of one is
            // not a name either — the same line keyword-argument completion has always drawn,
            // reached through the same `locator::precise_call`. Widening it means typing an
            // expression, which is deliberately out of scope.
            "person = Person.new(\"ada\")\nperson.shout(~)\n",
            "Person.new(\"ada\").shout(~)\n",
        ] {
            assert!(
                harness.signature(&uri, &calling(marked)).is_null(),
                "{marked:?} has no receiver the graph can name"
            );
        }
        // The guard, so this cannot pass by never answering anything: the same file, the same
        // method, through a receiver that is a constant.
        assert_eq!(
            harness.signature_card(&uri, &calling("Person.build(~)\n")),
            "Person.build(name, sep:)\n\u{20}            ~~~~\nBuild one."
        );
    }

    #[test]
    fn a_keyword_a_method_has_nowhere_to_put_underlines_nothing() {
        // `locate` takes two positionals and no keywords at all, so there is no parameter a
        // keyword argument could be. The signature is still worth showing — it is what tells
        // the user why the call is wrong — and highlighting a positional parameter would be
        // claiming a keyword can be passed as one.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        for marked in [
            "Person.locate(1, missing: ~)\n",
            "Person.locate(missing: 1, ~)\n",
        ] {
            assert_eq!(
                harness.signature_card(&uri, &calling(marked)),
                "Person.locate(x, y)",
                "{marked:?}"
            );
        }
    }

    #[test]
    fn a_reader_with_no_parameters_to_show_is_answered_with_nothing() {
        // `attr_reader` declares a method rubydex records as an attribute rather than as a
        // `def`, so it carries no parameter list — and a getter takes no arguments, so there
        // is nothing a signature card could say about the call. `null` closes the popup, which
        // is the right thing for a call that should not have parentheses at all.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        assert!(
            harness
                .signature(&uri, &calling("Person.registry(~)\n"))
                .is_null()
        );
        // The guard: the same receiver and a method that does have one.
        assert!(
            !harness
                .signature(&uri, &calling("Person.locate(~)\n"))
                .is_null()
        );
    }

    #[test]
    fn a_cursor_outside_every_argument_list_is_answered_with_nothing() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", CALLS);
        harness.index();

        for marked in [
            "Person.new(\"ada\")~\n",
            "x = 1~\n",
            "Person~.new\n",
            "# a note about Person.new(~)\n",
        ] {
            assert!(
                harness.signature(&uri, &calling(marked)).is_null(),
                "{marked:?} is not inside a call"
            );
        }
    }

    #[test]
    fn a_parameter_span_is_counted_the_way_the_client_indexes_the_label() {
        // The offsets are into a string the client holds as UTF-16, and a Ruby parameter can
        // be spelled in any script — `def приветствие(имя)` is legal Ruby. Counting bytes
        // would put the span three times too far along for Cyrillic and twice for an emoji,
        // and the drawing every other test here asserts on cannot see the difference because
        // every other fixture is ASCII. So this one asserts the numbers.
        let source = "class Greeter\n  def self.hello(имя, sep)\n  end\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/greeter.rb", source);
        harness.index();

        let help = harness.signature(&uri, &format!("{source}Greeter.hello(\"a\", ~)\n"));
        let parameters = help["signatures"][0]["parameters"]
            .as_array()
            .expect("a parameter list")
            .clone();
        assert_eq!(
            help["signatures"][0]["label"].as_str(),
            Some("Greeter.hello(имя, sep)")
        );
        // `Greeter#hello(` is 14 UTF-16 units, `имя` is 3 of them however many bytes it takes.
        assert_eq!(parameters[0]["label"], serde_json::json!([14, 17]));
        assert_eq!(parameters[1]["label"], serde_json::json!([19, 22]));
        assert_eq!(help["activeParameter"], serde_json::json!(1));
    }

    #[test]
    fn every_overload_is_offered_and_the_one_being_written_is_chosen() {
        // `Signatures::Overloaded` is real — RBS declares three arms for `String#gsub` and 65
        // in `core/string.rbs` altogether — and LSP has `activeSignature` for exactly this.
        // Flattening them to the first would be a choice to know less than the signatures do.
        //
        // The arity-1 arm is written first on purpose: with the shorter arm second, every
        // cursor position fits the first one and a broken choice would pass.
        let (mut harness, uri) = with_signatures("");
        assert_eq!(
            harness.signature_card(&uri, "Coordinate.new(1, ~)\n"),
            "Coordinate#initialize(text)\n\
             Coordinate#initialize(x, y)\n\
             \u{20}                        ~\n\
             A point, from a pair or from text."
        );
        // Nothing written yet, so both arms still fit and the first is the answer — an
        // argument count cannot tell an arity-1 call from an arity-2 one before there is one.
        assert_eq!(
            harness.signature_card(&uri, "Coordinate.new(~)\n"),
            "Coordinate#initialize(text)\n\
             \u{20}                     ~~~~\n\
             Coordinate#initialize(x, y)\n\
             A point, from a pair or from text."
        );
    }

    // -----------------------------------------------------------------------
    // Document highlight
    // -----------------------------------------------------------------------

    /// One spelling — `name` — used every way a Ruby file uses one.
    ///
    /// A parameter in two methods, a block parameter shadowing one of them, a method and a call
    /// to it, an instance variable in two different objects, and the same six letters in a
    /// comment and in a string. That last pair is not decoration: matching words is what an
    /// editor does when no server answers, and lighting up the comment is exactly how it is
    /// wrong. `MAX` is here so the exact half — a constant the resolver linked — is pinned by
    /// the same file as the half that is a scope walk.
    const OCCURRENCES: &str = "\
class Person
  MAX = 10

  # A name in a comment is only a word.
  def initialize(name)
    @name = name.strip
    @limit = MAX
  end

  def greet(name)
    label = \"name\"
    [name].each { |name| label = name }
    name + label
  end

  def name
    @name
  end

  def shout
    name.upcase
  end

  def self.rename(name)
    @name = name
  end
end
";

    /// `OCCURRENCES` with the cursor at the end of `needle`, which must occur in it exactly once.
    ///
    /// Naming a position by the text around it rather than by an index is what keeps these
    /// readable while the fixture grows: `on("def greet(name")` says which of the seven `name`s
    /// it means, and `on("(name", 2)` would not.
    fn on(needle: &str) -> String {
        let at = OCCURRENCES
            .find(needle)
            .expect("the needle is in the fixture");
        assert!(
            !OCCURRENCES[at + 1..].contains(needle),
            "{needle:?} has to name one position, and names more than one"
        );
        let end = at + needle.len();
        format!("{}~{}", &OCCURRENCES[..end], &OCCURRENCES[end..])
    }

    #[test]
    fn a_local_is_highlighted_in_its_own_scope_and_in_no_other() {
        // The whole file's answer, drawn. Four things are pinned by what is *not* marked here,
        // and every one of them is a way the editor's own word matching is wrong: the `name` in
        // the comment, the `name` inside the string, the `name` that is a method, and the two
        // `name`s belonging to other scopes — `initialize`'s parameter above and the block
        // parameter that shadows this one on the line in between.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("def greet(name")),
            "  def greet(name)\n\
             \u{20}           wwww\n\
             \u{20}   [name].each { |name| label = name }\n\
             \u{20}    rrrr\n\
             \u{20}   name + label\n\
             \u{20}   rrrr"
        );
    }

    #[test]
    fn a_block_parameter_shadows_the_local_it_is_spelled_like() {
        // Prism resolved these, not us: the block parameter and the read beside it are one
        // variable at depth 0, and the `[name]` three characters to their left is another at
        // depth 1. Nothing in `scopes` says the word "shadow".
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("{ |name")),
            "    [name].each { |name| label = name }\n\
             \u{20}                  wwww          rrrr"
        );
    }

    #[test]
    fn each_method_keeps_its_own_parameter() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("def initialize(name")),
            "  def initialize(name)\n\
             \u{20}                wwww\n\
             \u{20}   @name = name.strip\n\
             \u{20}           rrrr"
        );
    }

    #[test]
    fn a_method_is_highlighted_where_it_is_defined_and_where_it_is_called() {
        // The graph's half, and the one place the two halves could have disagreed about who
        // owns a name: `name` in `shout` is a call because no local is spelled that way there,
        // and a parameter named `name` two methods up must not join it.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("  def shout\n    name")),
            "  def name\n\
             \u{20}     wwww\n\
             \u{20}   name.upcase\n\
             \u{20}   rrrr"
        );
    }

    /// One name in every sigil namespace Ruby has, chosen so each would answer the others'
    /// prefixes if a sigil were fuzzy-matched like a letter.
    ///
    /// `entry` is in all five names on purpose: it is the substring that makes every list below
    /// a real question rather than a coincidence of spelling. `$@` is here because it is the
    /// bug this fixture exists for — a global whose whole name after the sigil *is* an `@`, so
    /// a subsequence match on `@` reaches it and nothing else in Ruby does.
    ///
    /// `CURSOR` is where the cursor goes; `sigils_at` writes the line in.
    const SIGILS: &str = "\
$@ = nil
$entry_log = []

class Ledger
  ENTRY_LIMIT = 100

  @@entry_total = 0

  def initialize
    @entries = []
    @entry_note = \"\"
  end

  def record
    entry = 1
    CURSOR
  end
end
";

    /// [`SIGILS`] with `line` written into `#record`'s body, where the `~` marks the cursor.
    ///
    /// One buffer rather than two, unlike `ANCESTRY`: an instance variable belongs to the
    /// `self` it was assigned on, so the cursor has to be inside the same class that assigns it.
    fn sigils_at(line: &str) -> String {
        SIGILS.replace("CURSOR", line)
    }

    #[test]
    fn an_instance_variable_prefix_reaches_no_other_namespace() {
        // The whole list, and what is not in it: no `$@`, which is what shipped through two
        // releases. Nor `$entry_log`, `ENTRY_LIMIT` or either method — every one of them holds
        // `entry`, and none of them is something `@` can be the start of.
        //
        // `@@entry_total` *is* here, and belongs here: one `@` is on the way to two.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("@~"), 10),
            [
                "@entries  Ledger#@entries",
                "@entry_note  Ledger#@entry_note",
                "@@entry_total  Ledger#@@entry_total",
            ]
        );
    }

    #[test]
    fn a_class_variable_prefix_does_not_reach_back_to_the_instance_variables() {
        // The other direction, which is the half that must not be symmetric: `@` admits `@@`
        // because the second character may still be coming, and `@@` admits no `@name` because
        // nothing can be typed that turns one into the other.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("@@~"), 10),
            ["@@entry_total  Ledger#@@entry_total"]
        );
    }

    #[test]
    fn a_global_prefix_is_the_only_thing_that_reaches_a_global() {
        // And `$@` is a perfectly good answer *here*. The rule is not that it is a bad row, it
        // is that it belongs to one prefix.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("$~"), 10),
            ["$entry_log  $entry_log", "$@  $@"]
        );
    }

    #[test]
    fn a_prefix_with_no_sigil_still_reaches_every_namespace() {
        // Deliberately unchanged, and pinned so it stays deliberate. Someone who has typed no
        // sigil has not said which namespace they mean, and a client filters as they keep
        // typing — so `entr` offers the constant, both instance variables, the class variable
        // and the global, and accepting one of them writes the sigil in.
        //
        // What it does not offer is `entry`, the local variable one line above the cursor:
        // rubydex's graph holds no locals, which is why `scopes.rs` walks Prism itself. That is
        // a missing feature rather than a wrong answer, and it is written here because a
        // first-ten list is where an absence is visible at all.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("entr~"), 10),
            [
                "ENTRY_LIMIT  Ledger::ENTRY_LIMIT",
                "@entries  Ledger#@entries",
                "@entry_note  Ledger#@entry_note",
                "@@entry_total  Ledger#@@entry_total",
                "$entry_log  $entry_log",
            ]
        );
    }

    #[test]
    fn an_instance_variable_belongs_to_whatever_self_is() {
        // `@name` in an instance method and `@name` in `def self.rename` are two variables —
        // one hangs off an instance of `Person` and the other off `Person` itself — and Ruby
        // will happily let a file use both. Joining them is the kind of wrong that reads as
        // right, which is why `scopes` tracks what `self` is rather than only the class body.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("initialize(name)\n    @name")),
            "    @name = name.strip\n\
             \u{20}   wwwww\n\
             \u{20}   @name\n\
             \u{20}   rrrrr"
        );
        assert_eq!(
            harness.highlight_map(&uri, &on("rename(name)\n    @name")),
            "    @name = name\n\
             \u{20}   wwwww"
        );
    }

    #[test]
    fn a_constant_is_highlighted_exactly() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("@limit = MAX")),
            "  MAX = 10\n\
             \u{20} www\n\
             \u{20}   @limit = MAX\n\
             \u{20}            rrr"
        );
    }

    #[test]
    fn a_name_in_a_comment_or_a_string_is_not_an_occurrence_of_anything() {
        // The bar the whole item is measured against. Both of these are positions where an
        // editor matching words lights the file up, and both answer `null` — which is also what
        // hands the fallback back to the client for exactly the positions ya-lsp cannot speak
        // for, rather than replacing it with an empty list everywhere.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(harness.highlight_map(&uri, &on("# A name")), "null");
        assert_eq!(harness.highlight_map(&uri, &on("label = \"name")), "null");
    }

    #[test]
    fn a_selection_chain_arrives_as_a_nest_of_parents() {
        // The half `ranges` own tests cannot see: LSP spells a chain as one range carrying its
        // parent rather than as a list, the innermost is the one at the top, and the outermost
        // carries no `parent` key at all.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        assert_eq!(
            harness.selection(&uri, "puts \"he~llo\"\n"),
            serde_json::json!([{
                "range": { "start": { "line": 0, "character": 6 },
                           "end": { "line": 0, "character": 11 } },
                "parent": {
                    "range": { "start": { "line": 0, "character": 5 },
                               "end": { "line": 0, "character": 12 } },
                    "parent": {
                        "range": { "start": { "line": 0, "character": 0 },
                                   "end": { "line": 0, "character": 12 } },
                        "parent": {
                            "range": { "start": { "line": 0, "character": 0 },
                                       "end": { "line": 1, "character": 0 } }
                        }
                    }
                }
            }])
        );
    }

    #[test]
    fn one_chain_comes_back_per_position_asked_about_in_the_order_asked() {
        // The protocol pairs the two arrays by index and has no spelling for "not this one", so
        // a position that resolved to nothing still has to answer — with the buffer, which is
        // what the second of these is.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();
        harness.open(&uri, "call(1)\n\n");

        let found = harness.ask(
            "textDocument/selectionRange",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "positions": [
                    { "line": 0, "character": 5 },
                    { "line": 1, "character": 0 },
                ],
            }),
        );

        let chains = found.as_array().expect("one chain per position");
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0]["range"]["end"]["character"], 6);
        assert_eq!(
            chains[1]["range"]["end"],
            serde_json::json!({ "line": 2, "character": 0 })
        );
        assert_eq!(chains[1]["parent"], serde_json::Value::Null);
    }

    #[test]
    fn folding_ranges_are_whole_lines_and_carry_no_characters() {
        // `lineFoldingOnly` is what every client that matters sends, and a character offset it
        // has been told to ignore is a field that can only ever be wrong. The `kind` is absent
        // for the same reason: syntax folds have none, and `null` is not one of LSP's three.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        assert_eq!(
            harness.folding(&uri, "# note\n# more\ndef foo\n  1\nend\n"),
            serde_json::json!([
                { "startLine": 0, "endLine": 1, "kind": "comment" },
                { "startLine": 2, "endLine": 3 },
            ])
        );
    }

    #[test]
    fn a_code_action_arrives_as_an_edit_the_editor_can_apply_without_asking_again() {
        // No `command` and no `data`: everything the action does is in the `edit`, so there is
        // nothing to resolve and nothing for the server to be asked a second time. The `kind`
        // is what the client filters on before it asks at all.
        let mut harness = Harness::new();
        let uri = harness.write("app/story.rb", "");
        harness.index();

        assert_eq!(
            harness.actions(&uri, "def title\n  puts ~story.name~\nend\n"),
            "--- Extract into local variable `extracted` [refactor.extract] ---\n\
             def title\n  extracted = story.name\n  puts extracted\nend\n"
        );
        let answer = harness.ask(
            "textDocument/codeAction",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "range": marked_range("def title\n  puts ~story.name~\nend\n"),
                "context": { "diagnostics": [] },
            }),
        );
        assert_eq!(answer[0]["command"], serde_json::Value::Null);
        assert_eq!(answer[0]["data"], serde_json::Value::Null);
        assert_eq!(answer[0]["kind"], "refactor.extract");
        // The same `WorkspaceEdit` a rename produces, from the same builder, so the version the
        // client negotiated for arrives here too.
        assert_eq!(
            answer[0]["edit"]["documentChanges"][0]["textDocument"]["version"],
            1
        );
    }

    #[test]
    fn a_position_with_nothing_to_offer_answers_null_rather_than_an_empty_list() {
        // `[]` tells the client ya-lsp answered and had nothing; `null` tells it nothing was
        // known. Neither costs a fallback here — no editor invents Ruby refactorings — but the
        // two are different words and this one is the true one.
        let mut harness = Harness::new();
        let uri = harness.write("app/story.rb", "");
        harness.index();

        assert_eq!(harness.actions(&uri, "~x = 1\n"), "null");
    }

    #[test]
    fn a_template_is_offered_no_code_actions() {
        // Declined for a reason no other request has. Every action here writes a **line**, and
        // in a template a line belongs to the markup: `erb::ruby_view` keeps the offsets so that
        // everything which reads answers unchanged, and there is nothing it can do about a line
        // that starts with `<td>`.
        let mut harness = Harness::new();
        let uri = harness.write("app/views/stories/show.html.erb", "");
        harness.index();

        assert_eq!(
            harness.actions(&uri, "<td><%= puts ~story.name~ %></td>\n"),
            "null"
        );
    }

    #[test]
    fn nothing_inside_a_gem_is_offered_a_code_action() {
        // The same rule as a rename's, silently for the same reason: ya-lsp never proposes an
        // edit to a file that is not the user's own, and a gem is opened to be read.
        let (dir, gem_home, env) = project_with_gem(
            "module Shouty\n  def self.blast(volume)\n    volume * 2\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty.blast(1)\n");
        harness.index();
        harness.index_gems();

        let inside = DocUri::from_path(&gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .expect("a gem file");
        assert_eq!(
            harness.actions(
                &inside,
                "module Shouty\n  def self.blast(volume)\n    ~volume * 2~\n  end\nend\n"
            ),
            "null"
        );
        assert!(harness.messages().is_empty(), "nothing said, deliberately");
    }

    #[test]
    fn a_file_with_nothing_to_fold_answers_null_rather_than_an_empty_list() {
        // The one place `null`-versus-`[]` costs the user something they had: a client with a
        // folding provider stops guessing folds from indentation, so an empty array would take
        // the guess away *and* put nothing in its place. A `null` hands it back.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        assert_eq!(
            harness.folding(&uri, "x = 1\ny = 2\n"),
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_file_the_editor_never_opened_still_folds_and_still_expands() {
        // Both read through `with_text`, so both answer from disk for a file no `didOpen` ever
        // named — which is what an editor does when it asks about a file it is only previewing.
        let mut harness = Harness::new();
        let uri = harness.write("lib/b.rb", "def foo\n  1\nend\n");
        harness.index();

        assert_eq!(
            harness.ask(
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            ),
            serde_json::json!([{ "startLine": 0, "endLine": 1 }])
        );
        let found = harness.ask(
            "textDocument/selectionRange",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "positions": [{ "line": 1, "character": 2 }],
            }),
        );
        assert_eq!(
            found[0]["range"]["start"],
            serde_json::json!({ "line": 1, "character": 2 })
        );
    }

    // -----------------------------------------------------------------------
    // Type hierarchy
    // -----------------------------------------------------------------------

    /// A three-deep chain, a module included halfway up it, a module prepended at the bottom, a
    /// sibling, and a superclass nothing can resolve.
    ///
    /// Written as one file so the fixture and the answers can be read against each other. The
    /// prepend is not decoration: it is the one shape where the class is *not* the first entry
    /// of its own ancestor chain, so dropping "the head of the list" instead of "the entry that
    /// is this class" would pass every other test here.
    const HIERARCHY: &str = "\
module Greet
end

module Loud
end

class Base
end

class Middle < Base
  include Greet
end

class Leaf < Middle
  prepend Loud
end

class Other < Base
end

class Orphan < Missing::Thing
end
";

    /// The fixture indexed, with signatures and gems off — so the rows are the project's own.
    fn hierarchy_harness() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("lib/hierarchy.rb", HIERARCHY);
        harness.index();
        (harness, uri)
    }

    #[test]
    fn the_supertypes_of_a_class_are_its_ruby_ancestors_in_ruby_order() {
        // The whole list, not a `contains`: this is `Module#ancestors` and the interesting thing
        // about it is its *composition*. `Loud` above `Leaf` because a prepended module wins
        // method lookup, `Greet` between `Middle` and `Base` because that is where it was
        // included, and `Leaf` itself nowhere — a class is in its own ancestors and is not its
        // own supertype.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, HIERARCHY, "Leaf <"),
            "module Loud — hierarchy.rb\n\
             class Middle — hierarchy.rb\n\
             module Greet — hierarchy.rb\n\
             class Base — hierarchy.rb"
        );
    }

    #[test]
    fn object_and_kernel_are_in_the_chain_only_when_there_is_a_file_to_point_at() {
        // Every Ruby class inherits from `Object`, `Kernel` and `BasicObject`, and rubydex knows
        // it without any signatures — it carries the five of them as a built-in document called
        // `rubydex:built-in`, which has no file behind it. `DocUri` rejects that URI for every
        // request alike, so the rows are dropped here rather than sent as somewhere an editor
        // cannot open. With signatures indexed they come back, out of `core/*.rbs`, which is the
        // test below.
        let (mut harness, uri) = hierarchy_harness();
        let rows = harness.hierarchy_rows("typeHierarchy/supertypes", &uri, HIERARCHY, "Base\nend");
        assert!(!rows.contains("Object"), "{rows}");
        assert!(!rows.contains("Kernel"), "{rows}");
    }

    #[test]
    fn the_subtypes_of_a_class_are_every_class_below_it_and_not_just_the_next_one() {
        // The mirror of the supertypes above, and deliberately transitive to match them: `Leaf`
        // is two levels under `Base` and is listed, because a chain answered one way and a
        // single generation answered the other would be a tree whose two directions disagree
        // about what a level means. `Base` itself is not in it, and neither is `Orphan`, which
        // inherits from something else.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, HIERARCHY, "Base\nend"),
            "class Leaf — hierarchy.rb\n\
             class Middle — hierarchy.rb\n\
             class Other — hierarchy.rb"
        );
    }

    #[test]
    fn a_module_lists_the_classes_that_mix_it_in() {
        // `include` and `prepend` both put a class into a module's descendants, which is what
        // makes "who uses this concern" a question the hierarchy answers. `Greet` is included by
        // `Middle` and reaches `Leaf` through it.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, HIERARCHY, "Greet\nend"),
            "class Leaf — hierarchy.rb\n\
             class Middle — hierarchy.rb"
        );
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, HIERARCHY, "Loud\nend"),
            "class Leaf — hierarchy.rb"
        );
    }

    #[test]
    fn an_ancestor_that_did_not_resolve_is_a_row_that_says_so() {
        // The silent-degradation case, and the reason the partial arm is not simply dropped: a
        // superclass in a gem that did not install would otherwise leave a chain that reads as
        // complete and is short by everything above the gap. The row is spelled as it was
        // written, its kind comes from having been written as a superclass rather than a mixin,
        // and it carries no `data` — there is nothing to expand.
        let (mut harness, uri) = hierarchy_harness();
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, HIERARCHY, "Orphan <"),
            "class Missing::Thing — not found"
        );

        let expanded = harness.expand("typeHierarchy/supertypes", &uri, HIERARCHY, "Orphan <");
        assert!(
            expanded[0]["data"].is_null(),
            "a row for a name that resolved to nothing has nothing behind it"
        );
    }

    #[test]
    fn an_unresolved_ancestor_is_placed_where_it_is_written_even_when_that_is_another_class() {
        // A partial propagates down the chain: `Cursed`'s ancestors carry the `Missing::Thing`
        // that `Orphan` inherits from, and it is written in `Orphan`. So the whole chain is
        // searched for the mention rather than only the class being expanded — otherwise the row
        // is either missing or pointing at the wrong line, and the row exists to be clicked.
        //
        // `Cursed` carries an unresolved mixin of its own so that the chain holds *two* names
        // that resolved to nothing. That is what makes the search step over one partial on its
        // way to the mention of another, which is the ordinary case in a project with a gem
        // missing and the one a single unresolved name never reaches.
        let source = "class Orphan < Missing::Thing\nend\n\nclass Cursed < Orphan\n  \
                      include AlsoMissing\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/cursed.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Cursed <"),
            "module AlsoMissing — not found\n\
             class Orphan — cursed.rb\n\
             class Missing::Thing — not found"
        );
        let expanded = harness.expand("typeHierarchy/supertypes", &uri, source, "Cursed <");
        assert_eq!(
            expanded[2]["selectionRange"]["start"],
            serde_json::json!({ "line": 0, "character": 15 }),
            "the span of `Missing::Thing` on `class Orphan`'s own line"
        );
        assert_eq!(
            expanded[0]["selectionRange"]["start"],
            serde_json::json!({ "line": 4, "character": 10 }),
            "and `AlsoMissing` where `Cursed` writes it"
        );
    }

    #[test]
    fn a_module_says_which_of_its_own_mixins_did_not_resolve() {
        // A module's ancestors are its mixins, and rubydex keeps `include`d names on the module
        // definition rather than on the enum — so a module in the chain is a case of its own,
        // and the concern that includes a missing concern is a shape Rails code has.
        let source = "module Bag\n  include Gone::Bits\nend\n\nclass Holder\n  \
                      include Bag\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/bag.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Bag\n  include"),
            "module Gone::Bits — not found"
        );
        // And through the class that includes it, which is where the propagation and the module
        // case meet.
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Holder"),
            "module Bag — bag.rb\n\
             module Gone::Bits — not found"
        );
    }

    #[test]
    fn a_mixin_that_did_not_resolve_is_a_module_and_a_superclass_is_a_class() {
        // Nothing in the graph says what a name that resolved to nothing *was*, but the source
        // does: `include` takes a module and `<` takes a class. Both rows would otherwise have to
        // guess, and a guess here shows the user the wrong icon on the only row on the screen
        // that is about something being missing.
        let source = "class Odd < Gone::Parent\n  include Gone::Concern\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/odd.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Odd <"),
            "module Gone::Concern — not found\n\
             class Gone::Parent — not found"
        );
    }

    #[test]
    fn an_explicit_root_is_kept_in_the_name_of_a_row_that_could_not_be_found() {
        // `::Foo` failing where `Foo` would have resolved is frequently the reason, and this row
        // is read rather than clicked, so the name is the whole of what it has to offer.
        let source = "class Rooted < ::Nowhere\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/rooted.rb", source);
        harness.index();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Rooted <"),
            "class ::Nowhere — not found"
        );
    }

    #[test]
    fn the_hierarchy_is_prepared_from_a_use_of_a_name_as_well_as_from_its_definition() {
        // Two of `locator`'s three targets reach here: the `class Leaf` definition and the
        // `Middle` written after the `<`, which is a constant reference. Both are things a user
        // right-clicks, and both have to answer with the class rather than with nothing.
        let (mut harness, uri) = hierarchy_harness();
        let from_definition = harness.prepare_hierarchy(&uri, HIERARCHY, "Leaf <");
        assert_eq!(from_definition[0]["name"], serde_json::json!("Leaf"));

        let from_reference = harness.prepare_hierarchy(&uri, HIERARCHY, "Middle\n  prepend");
        assert_eq!(from_reference[0]["name"], serde_json::json!("Middle"));
        assert_eq!(
            from_reference[0]["selectionRange"]["start"]["line"],
            serde_json::json!(9),
            "the `class Middle` line, not the line the reference is on"
        );
    }

    #[test]
    fn nothing_that_is_not_a_class_or_a_module_is_offered_a_hierarchy() {
        // `null`, not an empty list, which is what makes the editor say there are no results
        // rather than open an empty tree. A method is the case that matters: `references` matches
        // a method by name, so a cursor on `shout` resolves to declarations — they are simply
        // not types, and the rejection falls out of asking the resolution for a namespace.
        let source = "class Person\n  MAX = 3\n  def shout\n    total = MAX\n  end\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", source);
        harness.index();

        for needle in ["shout", "MAX = 3", "total"] {
            assert_eq!(
                harness.prepare_hierarchy(&uri, source, needle),
                serde_json::Value::Null,
                "{needle:?} is not a type"
            );
        }
    }

    #[test]
    fn a_singleton_class_is_not_a_type_anybody_asked_about() {
        // rubydex models `class << self` as a namespace called `Person::<Person>`, and it has a
        // real ancestor chain — `Class`, `Module`, `Object`. It is not a name a person wrote, so
        // it is turned away on the same two tests the symbol picker uses, and a cursor on
        // `self` there answers `null` rather than opening a tree over ya-lsp's own spelling.
        let source = "class Person\n  class << self\n    def build; end\n  end\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", source);
        harness.index();

        assert_eq!(
            harness.prepare_hierarchy(&uri, source, "<< self"),
            serde_json::Value::Null
        );
    }

    #[test]
    fn an_item_the_client_hands_back_without_a_usable_declaration_answers_nothing() {
        // Both follow-ups take their subject from the client, which echoes back whatever the row
        // it is expanding carried. A row for an unresolved name has no `data` at all, a client
        // may send one that is not a number, and a rubydex id is a 64-bit hash — so a stale one
        // is indistinguishable from a live one and has to answer "nothing" rather than resolve
        // against whatever it collides with.
        let (mut harness, _) = hierarchy_harness();
        for data in [
            serde_json::Value::Null,
            serde_json::json!("not a number"),
            serde_json::json!("0"),
            serde_json::json!(1_234_567_890_123_456_789_u64),
            serde_json::json!("1234567890123456789"),
        ] {
            let item = serde_json::json!({
                "name": "Ghost",
                "kind": 5,
                "uri": "file:///nowhere.rb",
                "range": { "start": { "line": 0, "character": 0 },
                           "end": { "line": 0, "character": 0 } },
                "selectionRange": { "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 0 } },
                "data": data,
            });
            for method in ["typeHierarchy/supertypes", "typeHierarchy/subtypes"] {
                assert_eq!(
                    harness.ask(method, serde_json::json!({ "item": item })),
                    serde_json::Value::Null,
                    "{method} on {:?}",
                    item["data"]
                );
            }
        }
    }

    #[test]
    fn malformed_hierarchy_params_are_answered_with_null_rather_than_a_panic() {
        // Client input, like every other handler's params. All three arms, because each parses a
        // different shape and the two follow-ups do not take a position at all.
        let (mut harness, _) = hierarchy_harness();
        for method in [
            "textDocument/prepareTypeHierarchy",
            "typeHierarchy/supertypes",
            "typeHierarchy/subtypes",
        ] {
            assert_eq!(
                harness.ask(method, serde_json::json!({ "nonsense": true })),
                serde_json::Value::Null
            );
        }
    }

    #[test]
    fn a_truncated_list_of_subtypes_says_so() {
        // A short list of subtypes is indistinguishable from a complete one, so reaching the cap
        // is said out loud rather than only logged. Reached here by asking about a class with
        // more subtypes than the answer holds, which in a real project takes asking about
        // something near the root of the object model.
        let mut source = String::from("class Root\nend\n");
        for index in 0..(MAX_SUBTYPES + 3) {
            source.push_str(&format!("class Sub{index} < Root\nend\n"));
        }
        let mut harness = Harness::new();
        let uri = harness.write("lib/many.rb", &source);
        harness.index();

        let found = harness.expand("typeHierarchy/subtypes", &uri, &source, "Root\nend");
        assert_eq!(found.as_array().map(Vec::len), Some(MAX_SUBTYPES));
        assert_eq!(
            harness.messages(),
            vec![format!(
                "{} subtypes found: only the first {MAX_SUBTYPES} are shown.",
                MAX_SUBTYPES + 3
            )]
        );
    }

    #[test]
    fn the_users_own_code_is_ranked_above_the_bundle_and_the_rest_alphabetically() {
        // `search::rank`'s decision, for `search::rank`'s reason: asking a widely-subclassed
        // class for its subtypes in a real project finds hundreds in gems and a handful that are
        // the user's, and an alphabetical list would bury the handful below whatever the bundle
        // happens to spell with an `A`. Within each half the order is the name, never the
        // `DeclarationId` — that is a hash, and a tree that reshuffles between runs is
        // unreadable.
        let (dir, _gem_home, env) = project_with_gem(
            "module Shouty\n  class Base\n  end\n\n  class Middle < Base\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "class ZLocal < Shouty::Base\nend\n";
        let uri = harness.write("lib/z_local.rb", source);
        harness.index();
        harness.index_gems();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, source, "Base\nend"),
            "class ZLocal — z_local.rb\n\
             class Shouty::Middle — shouty.rb",
            "the project's own class first, though it sorts last"
        );
    }

    #[test]
    fn signatures_put_rubys_own_classes_and_modules_in_the_chain() {
        // The "a superclass in a gem" case, met with the mechanism that actually
        // delivers it: with signatures indexed, `Object` and `Comparable` come out of real
        // `.rbs` files and the chain reaches all the way up. `module Comparable` in a list of
        // supertypes is the answer's most surprising claim and its most correct one — a
        // linearized chain is what Ruby means by `ancestors`, and filtering modules out to make
        // it look like single inheritance would make it wrong.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/object.rbs"),
            "class BasicObject\nend\n\nmodule Kernel\nend\n\nclass Object < BasicObject\n  \
             include Kernel\nend\n\nmodule Comparable\nend\n\nclass Numeric < Object\n  \
             include Comparable\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let source = "class Money < Numeric\nend\n";
        let uri = harness.write("lib/money.rb", source);
        harness.index();
        harness.index_gems();

        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/supertypes", &uri, source, "Money <"),
            "class Numeric — object.rbs\n\
             module Comparable — object.rbs\n\
             class Object — object.rbs\n\
             module Kernel — object.rbs\n\
             class BasicObject — object.rbs"
        );
        // And the other direction across the same boundary: Ruby's own class knows about the
        // project's, because the reverse index is filled as the chain is linearized.
        assert_eq!(
            harness.hierarchy_rows("typeHierarchy/subtypes", &uri, source, "Numeric"),
            "class Money — money.rb"
        );
    }

    #[test]
    fn a_class_reopened_in_two_files_is_one_row_pointing_at_the_users_own_copy() {
        // One row per declaration, not one per definition: `ActiveRecord::Base` is reopened
        // hundreds of times and a chain listing each would be unreadable. Which of them the row
        // points at is `locator::preferred_definition`, shared with the symbol picker so a class
        // cannot open in one file from the outline and in another from the hierarchy.
        let (dir, _gem_home, env) = project_with_gem("module Shouty\n  class Base\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "module Shouty\n  class Base\n    def extra; end\n  end\nend\n";
        let uri = harness.write("lib/reopen.rb", source);
        harness.index();
        harness.index_gems();

        let prepared = harness.prepare_hierarchy(&uri, source, "Base");
        assert_eq!(
            prepared.as_array().map(Vec::len),
            Some(1),
            "reopened in two files, listed once: {prepared}"
        );
        assert!(
            prepared[0]["uri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("/lib/reopen.rb"),
            "the project's copy, not the gem's: {}",
            prepared[0]["uri"]
        );
    }

    // -----------------------------------------------------------------------
    // Rename
    // -----------------------------------------------------------------------

    /// One spelling, `name`, used as five different variables in one file.
    ///
    /// The point of the fixture is that a word search cannot tell any of them apart. `name` is a
    /// method parameter, a block parameter shadowing it, a lambda parameter shadowing it again,
    /// a local in an unrelated method, and a word inside a comment and a string. A rename of any
    /// one of them must leave the other four exactly as they were, and the drawing is where that
    /// is read.
    const LOCALS: &str = "\
def greet(name)
  greeting = \"Hi #{name}\" # the name goes here
  [1, 2].each { |name| puts name }
  shout = ->(name) { name.upcase }
  \"name\" + greeting + shout.call(name)
end

def unrelated
  name = 1
  name + 1
end
";

    #[test]
    fn renaming_a_local_changes_its_own_scope_and_nothing_that_merely_spells_it() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The parameter of `greet`, which is read twice: inside the interpolation, and in the
        // last line's argument. Everything else spelled `name` belongs to something else.
        assert_eq!(
            harness.renamed(&uri, LOCALS, "name)", "person"),
            "\
--- greet.rb ---
def greet(person)
  greeting = \"Hi #{person}\" # the name goes here
  [1, 2].each { |name| puts name }
  shout = ->(name) { name.upcase }
  \"name\" + greeting + shout.call(person)
end

def unrelated
  name = 1
  name + 1
end
"
        );
    }

    #[test]
    fn renaming_a_block_parameter_stops_at_the_block() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The block's own `name`, which shadows the parameter. Prism resolves the two to
        // different scopes and that indexing is the whole of the rule — nothing here knows the
        // word "shadow".
        assert_eq!(
            harness.renamed(&uri, LOCALS, "name| puts", "each_one"),
            "\
--- greet.rb ---
def greet(name)
  greeting = \"Hi #{name}\" # the name goes here
  [1, 2].each { |each_one| puts each_one }
  shout = ->(name) { name.upcase }
  \"name\" + greeting + shout.call(name)
end

def unrelated
  name = 1
  name + 1
end
"
        );
    }

    #[test]
    fn a_word_in_a_comment_or_a_string_is_not_a_position_a_rename_answers_for() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The two places an editor's own word matching would offer to rename. `null` from
        // `prepareRename` is what stops the box from opening at all, and nothing is said about
        // it: the cursor is on prose, and there is no refusal to explain.
        for needle in ["name goes here", "\"name\" +"] {
            assert!(
                harness.prepare_rename(&uri, LOCALS, needle).is_null(),
                "{needle:?} is not renameable"
            );
        }
        assert!(harness.messages().is_empty());
    }

    /// A constant in a namespace, used four ways across two files, with a second constant of the
    /// same name in another namespace that must not move.
    const HR: &str = "\
module HR
  class Person
    ROLE = \"staff\"

    def self.build
      Person.new
    end
  end

  class Boss < Person
    def peer
      HR::Person.new
    end
  end
end
";

    const ADMIN: &str = "\
class Admin
  def hire
    HR::Person.build
  end
end

module Other
  class Person
  end
end

def elsewhere
  Other::Person.new
end
";

    #[test]
    fn renaming_a_constant_follows_the_resolution_across_files_and_namespaces() {
        let mut harness = Harness::new();
        let hr = harness.write("app/hr.rb", HR);
        harness.write("app/admin.rb", ADMIN);
        harness.index();

        // Every spelling of the one constant changes: the `class` line, the bare `Person`
        // inside its own namespace, the superclass of `Boss`, and the qualified `HR::Person` in
        // both files. Only the last segment of a qualified reference moves, which is a fact
        // about how rubydex records them rather than anything this had to arrange.
        //
        // `Other::Person` and the `class Person` inside `module Other` are the control, and
        // they are in the drawing rather than in a second assertion: `admin.rb` is shown whole,
        // so the two names that did not change are as visible as the one that did.
        assert_eq!(
            harness.renamed(&hr, HR, "Person\n", "Employee"),
            "\
--- admin.rb ---
class Admin
  def hire
    HR::Employee.build
  end
end

module Other
  class Person
  end
end

def elsewhere
  Other::Person.new
end
--- hr.rb ---
module HR
  class Employee
    ROLE = \"staff\"

    def self.build
      Employee.new
    end
  end

  class Boss < Employee
    def peer
      HR::Employee.new
    end
  end
end
"
        );
    }

    #[test]
    fn renaming_a_constant_from_a_use_of_it_answers_the_same_as_from_where_it_is_written() {
        let mut harness = Harness::new();
        let hr = harness.write("app/hr.rb", HR);
        let admin = harness.write("app/admin.rb", ADMIN);
        harness.index();

        let from_the_class_line = harness.renamed(&hr, HR, "Person\n", "Employee");
        // The `Person` in `HR::Person` in the *other* file: a constant reference rather than a
        // definition, which reaches the plan down a different arm of `locate`.
        let from_a_qualified_use = harness.renamed(&admin, ADMIN, "Person.build", "Employee");
        assert_eq!(from_a_qualified_use, from_the_class_line);
    }

    #[test]
    fn a_constant_assigned_rather_than_declared_moves_with_its_namespace() {
        let mut harness = Harness::new();
        let source = "\
module HR
  MAX_STAFF = 10

  def self.room?
    HR::MAX_STAFF > 1 && MAX_STAFF < 100
  end
end
";
        let uri = harness.write("app/limits.rb", source);
        harness.index();

        // `MAX_STAFF = 10` is a constant rather than a namespace, and rubydex records no name
        // span for one — `locator::spans` falls back to the whole construct, which for this
        // kind is exactly the name and nothing else. Worth a test rather than a comment,
        // because the *next* fixture is the kind where that fallback is not the name.
        assert_eq!(
            harness.renamed(&uri, source, "MAX_STAFF = ", "MAX_HEADCOUNT"),
            "\
--- limits.rb ---
module HR
  MAX_HEADCOUNT = 10

  def self.room?
    HR::MAX_HEADCOUNT > 1 && MAX_HEADCOUNT < 100
  end
end
"
        );
    }

    #[test]
    fn a_class_made_with_class_new_is_narrowed_to_its_name_rather_than_replaced_whole() {
        let mut harness = Harness::new();
        let source = "\
module HR
  Failure = Class.new(StandardError)
  Shim = Module.new

  def self.fail!
    raise Failure
  end
end
";
        let uri = harness.write("app/errors.rb", source);
        harness.index();

        // The case that makes the confirmation step load-bearing rather than defensive.
        // rubydex promotes `Failure = Class.new(StandardError)` to a class, and the *name* span
        // it records for it is the whole assignment — so a rename that trusted the span would
        // write `raise BuildFailed` and, on the line above, replace the entire
        // `Failure = Class.new(StandardError)` with `BuildFailed`, deleting the class.
        //
        // `StandardError` is in the drawing on purpose: it ends in the very name being narrowed
        // to, and it is why the search inside the span is for a whole word.
        assert_eq!(
            harness.renamed(&uri, source, "Failure = ", "BuildFailed"),
            "\
--- errors.rb ---
module HR
  BuildFailed = Class.new(StandardError)
  Shim = Module.new

  def self.fail!
    raise BuildFailed
  end
end
"
        );
        assert!(harness.messages().is_empty(), "nothing to explain");
    }

    #[test]
    fn a_name_written_twice_inside_one_span_refuses_rather_than_guessing_which_is_which() {
        let mut harness = Harness::new();
        let source = "\
Registry = Class.new { include Registry }
";
        let uri = harness.write("app/registry.rb", source);
        harness.index();

        // The other side of the narrowing rule. Two whole-word occurrences inside the one span
        // rubydex hands back, either of which could be the one being defined, so nothing is
        // changed and the sentence says which file to look at.
        assert_eq!(
            harness.renamed(&uri, source, "Registry = ", "Catalogue"),
            "null"
        );
        assert_eq!(
            harness.messages(),
            vec![messages::rename_could_not_confirm(
                "Registry",
                "registry.rb"
            )]
        );
    }

    #[test]
    fn a_method_is_refused_out_loud_rather_than_renamed_by_a_name_match() {
        let mut harness = Harness::new();
        let source = "\
class Person
  def shout
    :loud
  end
end

class Siren
  def shout
    :louder
  end
end

Person.new.shout
";
        let uri = harness.write("app/shout.rb", source);
        harness.index();

        // Two classes define `shout`, which is the ordinary reason a method rename would go
        // wrong: `references` matches a method by name, so a rename built on it would edit
        // `Siren#shout` and the call below along with the one asked about, and would look as
        // though it had worked.
        //
        // From the `def` line and from the call site alike, since those reach the plan down
        // different arms of `locate`.
        for needle in ["shout\n    :loud", "shout\n"] {
            assert!(harness.prepare_rename(&uri, source, needle).is_null());
            assert_eq!(harness.messages(), vec![messages::rename_refuses_methods()]);
        }
    }

    #[test]
    fn an_instance_variable_is_refused_because_a_subclass_can_share_it() {
        let mut harness = Harness::new();
        let source = "\
class Person
  def initialize
    @name = \"anon\"
  end

  def name
    @name
  end
end
";
        let uri = harness.write("app/person.rb", source);
        harness.index();

        // The scope walk answers for `@name` — `documentHighlight` lights both of these up —
        // and the refusal is the plan's rather than a limit of the walk: what makes it unsafe
        // is a subclass or an included module in another file writing the same name, which one
        // file cannot see.
        assert!(harness.prepare_rename(&uri, source, "@name = ").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_instance_variables()]
        );
    }

    #[test]
    fn a_parameter_ruby_supplies_has_nowhere_to_put_a_new_name() {
        let mut harness = Harness::new();
        let source = "\
[1, 2].each { it + 1 }
[3, 4].each { _1 * 2 }
";
        let uri = harness.write("app/implicit.rb", source);
        harness.index();

        // `it` and `_1` are read everywhere and written nowhere, because the block declares
        // them rather than the file naming them. Renaming one would mean writing a parameter
        // list that is not there, which is a refactoring rather than a rename.
        for (needle, name) in [("it +", "it"), ("_1 *", "_1")] {
            assert!(harness.prepare_rename(&uri, source, needle).is_null());
            assert_eq!(
                harness.messages(),
                vec![messages::rename_refuses_implicit_parameters(name)]
            );
        }
    }

    #[test]
    fn a_variable_that_is_also_a_keyword_or_a_hash_key_is_refused() {
        let mut harness = Harness::new();
        let source = "\
def call(host:, port:)
  config = { host:, port: port }
  connect(host:)
  config
end

def other(host)
  host.to_s
end
";
        let uri = harness.write("app/call.rb", source);
        harness.index();

        // Three ordinary Ruby spellings put a name somewhere it means more than the variable,
        // and all three are in this fixture. `host:` in the parameter list is the method's
        // interface, so renaming it changes what every caller writes; `{ host:, ... }` and
        // `connect(host:)` are Ruby 3.1's shorthand, where the one word is the key *and* a read
        // of the local — replacing the span renames the key with it, which changes the hash and
        // still parses.
        for needle in ["host:, port:)", "host:, port: port", "host:)"] {
            assert!(
                harness.prepare_rename(&uri, source, needle).is_null(),
                "{needle:?}"
            );
            assert_eq!(
                harness.messages(),
                vec![messages::rename_refuses_shorthand("host")]
            );
        }

        // `port` is written out in full at its one read, and refused all the same: the keyword
        // parameter that declares it is the interface either way.
        assert!(harness.prepare_rename(&uri, source, "port }").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_shorthand("port")]
        );

        // And the control: the same spelling in a method that takes it positionally renames,
        // because nothing about a positional parameter's name reaches a caller.
        assert_eq!(
            harness.renamed(&uri, source, "host)", "hostname"),
            "\
--- call.rb ---
def call(host:, port:)
  config = { host:, port: port }
  connect(host:)
  config
end

def other(hostname)
  hostname.to_s
end
"
        );
    }

    #[test]
    fn a_local_before_a_double_colon_renames_because_that_colon_is_a_lookup() {
        let mut harness = Harness::new();
        let source = "\
def read
  source = Object
  source::NAME
end
";
        let uri = harness.write("app/read.rb", source);
        harness.index();

        // The reason the shorthand test is for one colon rather than for a colon. `source` here
        // is a local with a constant looked up on it, which is the commonest legitimate
        // spelling of a name followed by a colon and must not be caught by the rule above.
        assert_eq!(
            harness.renamed(&uri, source, "source =", "holder"),
            "\
--- read.rb ---
def read
  holder = Object
  holder::NAME
end
"
        );
    }

    #[test]
    fn a_new_name_ruby_would_not_read_as_a_name_changes_nothing() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The prepare said yes, so the position is fine and it is the *name* that is not. LSP
        // has nowhere to put a validation rule, so the client asks with whatever was typed and
        // this is the only place it can be answered.
        assert!(harness.prepare_rename(&uri, LOCALS, "name)").is_object());
        for candidate in ["Person", "nil", "shout!", "two words", ""] {
            assert_eq!(
                harness.renamed(&uri, LOCALS, "name)", candidate),
                "null",
                "{candidate:?}"
            );
            assert_eq!(
                harness.messages(),
                vec![messages::rename_needs_a_ruby_name(candidate, false)]
            );
        }
    }

    #[test]
    fn a_constant_cannot_be_renamed_to_something_that_is_not_one() {
        let mut harness = Harness::new();
        let hr = harness.write("app/hr.rb", HR);
        harness.write("app/admin.rb", ADMIN);
        harness.index();

        // The other half of the rule, and the reason the plan carries which kind it is: a
        // constant renamed to a lowercase name is not a constant any more, and every reference
        // to it would stop resolving. `HR::Employee` is refused too — it is a path rather than
        // a name, and only the last segment of a reference is ever replaced, so splicing one in
        // would write `HR::HR::Employee` at the qualified use sites.
        for candidate in ["employee", "HR::Employee", "@Employee"] {
            assert_eq!(
                harness.renamed(&hr, HR, "Person\n", candidate),
                "null",
                "{candidate:?}"
            );
            assert_eq!(
                harness.messages(),
                vec![messages::rename_needs_a_ruby_name(candidate, true)]
            );
        }
    }

    #[test]
    fn a_name_defined_in_a_gem_is_refused_from_the_project_that_uses_it() {
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty::Megaphone.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        // Renaming this would edit the gem, or edit the project and leave the gem defining the
        // old name. Both are wrong, and the sentence says which it is rather than leaving the
        // editor to report that nothing can be renamed here.
        assert!(harness.prepare_rename(&uri, source, "Megaphone").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Megaphone")]
        );
    }

    #[test]
    fn a_class_the_project_reopens_from_a_gem_is_refused_along_with_it() {
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "\
module Shouty
  class Megaphone
    def blast
      :loud
    end
  end
end
";
        let uri = harness.write("lib/reopen.rb", source);
        harness.index();
        harness.index_gems();

        // The case the rule is really about, and the one a "is any of it mine?" test would get
        // wrong: the project reopens a class the gem defines, so one of the two places the name
        // is written is a file ya-lsp will not edit. Renaming the project's half alone would
        // leave the gem defining `Megaphone` and the project defining something else.
        assert!(harness.prepare_rename(&uri, source, "Megaphone").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Megaphone")]
        );
    }

    #[test]
    fn nothing_inside_a_gem_is_renameable_even_from_a_position_that_would_be_exact() {
        let (dir, gem_home, env) = project_with_gem(
            "module Shouty\n  def self.blast(volume)\n    volume * 2\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty.blast(1)\n");
        harness.index();
        harness.index_gems();

        // `volume` is a local, which is exact wherever it is written — so the refusal is not
        // about precision, it is that ya-lsp never proposes an edit to a file that is not the
        // user's own. Silently, as every other request inside a bundle is: a gem is opened to
        // be read, and nobody pressing rename in one expects it to work.
        let inside = DocUri::from_path(&gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .expect("a gem file");
        let source = "module Shouty\n  def self.blast(volume)\n    volume * 2\n  end\nend\n";
        assert!(harness.prepare_rename(&inside, source, "volume)").is_null());
        assert!(harness.messages().is_empty(), "nothing said, deliberately");
    }

    #[test]
    fn an_edit_carries_the_version_it_was_computed_against_when_the_client_takes_one() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();
        harness.open(&uri, LOCALS);

        let answer = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(LOCALS, "name)"),
                "newName": "person",
            }),
        );
        // The richer shape, and the reason it is worth negotiating for: the version pins the
        // text the edit was computed against, so a client can reject a rename the user has
        // typed past rather than applying it to text that has moved.
        assert_eq!(answer["changes"], serde_json::Value::Null);
        assert_eq!(answer["documentChanges"][0]["textDocument"]["version"], 1);
        assert_eq!(
            answer["documentChanges"][0]["textDocument"]["uri"],
            serde_json::json!(uri.to_lsp().expect("an LSP uri")),
        );
    }

    #[test]
    fn a_client_that_did_not_ask_for_document_changes_gets_the_older_map() {
        let mut harness = Harness::new();
        harness.analysis.client.versioned_edits = false;
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // A client that did not advertise `documentChanges` may not merely ignore the shape it
        // did not ask for; it can fail to apply the edit at all. The older map has no version
        // in it, which is exactly what advertising the newer one buys.
        let answer = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(LOCALS, "name)"),
                "newName": "person",
            }),
        );
        assert_eq!(answer["documentChanges"], serde_json::Value::Null);
        let edits = &answer["changes"][uri.to_lsp().expect("an LSP uri").as_str()];
        assert_eq!(edits.as_array().map(Vec::len), Some(3), "{answer}");
    }

    #[test]
    fn the_prepare_range_is_the_word_under_the_cursor_and_not_the_span_it_was_found_in() {
        let mut harness = Harness::new();
        let source = "Failure = Class.new(StandardError)\n";
        let uri = harness.write("app/errors.rb", source);
        harness.index();

        // The editor puts its rename box over exactly this range and pre-fills it with the text
        // inside, so the range has to be the name rather than the span rubydex recorded — which
        // for this shape is the whole assignment.
        assert_eq!(
            harness.prepare_rename(&uri, source, "Failure"),
            serde_json::json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 7 },
            })
        );
        // And a cursor inside the recorded span but outside the name answers `null` rather than
        // offering to rename something the cursor is not on. Here it lands on the `=`.
        assert!(harness.prepare_rename(&uri, source, "= Class").is_null());
    }

    #[test]
    fn a_singleton_class_is_not_a_name_anybody_typed() {
        let mut harness = Harness::new();
        let source = "\
class Person
  class << self
    def build
      new
    end
  end
end
";
        let uri = harness.write("app/person.rb", source);
        harness.index();

        // A cursor on `class << self` resolves to the singleton, whose name the graph spells
        // `Person::<Person>`. Asked of the *old* name, the same check that vets a new one rules
        // that out — and silently, because nobody meant to rename it.
        // The name span rubydex records for `class << self` is the `self`, which is where a
        // cursor has to be for this to be reached at all.
        assert!(harness.prepare_rename(&uri, source, "self\n").is_null());
        assert!(harness.messages().is_empty());
    }

    #[test]
    fn a_constant_that_resolves_to_nothing_is_nothing_to_rename() {
        let mut harness = Harness::new();
        let source = "Missing::Gone.new
";
        let uri = harness.write("app/typo.rb", source);
        harness.index();

        // Both halves of a name nothing in the graph defines — a typo, or a gem that did not
        // resolve. There is no set of places it is written to change, so there is nothing to
        // refuse either: the answer is the same `null` a comment gets.
        for needle in ["Missing", "Gone"] {
            assert!(harness.prepare_rename(&uri, source, needle).is_null());
        }
        assert!(harness.messages().is_empty());
    }

    // ----------------------------------------------------------------------- ERB templates

    /// The model a template renders, and the template that renders it.
    ///
    /// Deliberately ordinary Rails: a collection assigned to an instance variable, a block local
    /// taken out of it, a method call on that local, a constant, and markup wrapped around all of
    /// it. Every ERB test below reads one of these two files, so what any of them asserts is
    /// about the *technique* rather than about a fixture written to suit it.
    const STORY: &str = "\
class Story
  TAGLINE = \"news\"

  def title
    @title
  end
end
";

    const VIEW: &str = "\
<h1>Stories</h1>
<% @stories.each do |story| %>
  <p><%= story.title %> &mdash; <%= Story::TAGLINE %></p>
<% end %>
";

    /// What one request answered, short enough to put in a table cell.
    ///
    /// An empty array and a `null` are drawn the same way on purpose: to the user they are the
    /// same answer, and which one a handler returns is decided per request for reasons that have
    /// nothing to do with templates — except in `foldingRange`, which is why that one has a test
    /// of its own.
    fn shape(answer: &serde_json::Value) -> String {
        let count = |len: usize| match len {
            0 => "\u{2014}".to_owned(),
            n => n.to_string(),
        };
        match answer {
            serde_json::Value::Null => "\u{2014}".to_owned(),
            serde_json::Value::Array(items) => count(items.len()),
            object => match (object.get("items"), object.get("data")) {
                // A completion list.
                (Some(serde_json::Value::Array(items)), _) => count(items.len()),
                // Semantic tokens, five integers to a token.
                (_, Some(serde_json::Value::Array(data))) => count(data.len() / 5),
                _ => "yes".to_owned(),
            },
        }
    }

    /// Every request ya-lsp answers, asked at one cursor, drawn as what came back.
    ///
    /// The three that take no cursor take what the cursor produced — the item
    /// `prepareTypeHierarchy` returned and the first row `completion` offered — because asking
    /// them with something from anywhere else would be asking a different question.
    fn answers(
        harness: &mut Harness,
        uri: &DocUri,
        position: &serde_json::Value,
    ) -> Vec<(&'static str, String)> {
        let document = serde_json::json!({ "uri": uri.as_str() });
        let at = serde_json::json!({ "textDocument": document, "position": position });
        let mut drawn = Vec::new();

        for (method, params) in [
            (
                "textDocument/documentSymbol",
                serde_json::json!({ "textDocument": document }),
            ),
            ("textDocument/hover", at.clone()),
            ("textDocument/definition", at.clone()),
            (
                "textDocument/references",
                serde_json::json!({
                    "textDocument": document,
                    "position": position,
                    "context": { "includeDeclaration": false },
                }),
            ),
            ("textDocument/documentHighlight", at.clone()),
            (
                "textDocument/selectionRange",
                serde_json::json!({ "textDocument": document, "positions": [position] }),
            ),
            (
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": document }),
            ),
            (
                "textDocument/semanticTokens/full",
                serde_json::json!({ "textDocument": document }),
            ),
            ("workspace/symbol", serde_json::json!({ "query": "title" })),
            ("textDocument/signatureHelp", at.clone()),
            ("textDocument/prepareRename", at.clone()),
            (
                "textDocument/rename",
                serde_json::json!({
                    "textDocument": document,
                    "position": position,
                    "newName": "Article",
                }),
            ),
            ("textDocument/completion", at.clone()),
        ] {
            let answer = harness.ask(method, params);
            drawn.push((method, shape(&answer)));
        }

        // The type hierarchy, and the row a completion list would resolve: three requests whose
        // input is another request's output.
        let prepared = harness.ask("textDocument/prepareTypeHierarchy", at.clone());
        drawn.push(("textDocument/prepareTypeHierarchy", shape(&prepared)));
        let item = prepared.as_array().and_then(|items| items.first()).cloned();
        for method in ["typeHierarchy/supertypes", "typeHierarchy/subtypes"] {
            let answer = match &item {
                Some(item) => harness.ask(method, serde_json::json!({ "item": item })),
                None => serde_json::Value::Null,
            };
            drawn.push((method, shape(&answer)));
        }

        let offered = harness.ask("textDocument/completion", at);
        let row = offered["items"].as_array().and_then(|items| items.first());
        let resolved = match row {
            Some(row) => harness.ask("completionItem/resolve", row.clone()),
            None => serde_json::Value::Null,
        };
        drawn.push((
            "completionItem/resolve",
            match resolved {
                serde_json::Value::Null => "\u{2014}".to_owned(),
                _ => "yes".to_owned(),
            },
        ));

        drawn
    }

    /// The whole protocol, asked twice in one template: once inside a tag, once in the markup.
    ///
    /// One table rather than seventeen assertions, for the reason `GALLERY` is one document:
    /// what has to be legible is *where the answers stop*, and a per-request assertion cannot
    /// show it. The markup column is the finding — eight of the nine positional requests need no
    /// template-awareness at all, because blanked markup holds no identifier and they already
    /// answer nothing. Only `completion` needed a gate, and only `foldingRange` is declined.
    #[test]
    fn every_request_asked_inside_a_tag_and_in_the_markup_beside_it() {
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        // Three characters into `Story`, so that `completion` has a half-typed word to
        // complete and the row it offers is a real one — the same caret every other request
        // here is asked at.
        let inside = position_of(VIEW, "ry::TAGLINE");
        let markup = position_of(VIEW, "Stories</h1>");
        let mut table = vec![format!("{:<36}{:>10}{:>10}", "", "in <% %>", "in markup")];
        for ((method, ruby), (_, html)) in answers(&mut harness, &view, &inside)
            .into_iter()
            .zip(answers(&mut harness, &view, &markup))
        {
            table.push(format!("{method:<36}{ruby:>10}{html:>10}"));
        }

        assert_eq!(
            table.join("\n"),
            "                                      in <% %> in markup\n\
             textDocument/documentSymbol                  —         —\n\
             textDocument/hover                         yes         —\n\
             textDocument/definition                      1         —\n\
             textDocument/references                      1         —\n\
             textDocument/documentHighlight               1         —\n\
             textDocument/selectionRange                  1         1\n\
             textDocument/foldingRange                    —         —\n\
             textDocument/semanticTokens/full             4         4\n\
             workspace/symbol                             1         1\n\
             textDocument/signatureHelp                   —         —\n\
             textDocument/prepareRename                 yes         —\n\
             textDocument/rename                        yes         —\n\
             textDocument/completion                      1         —\n\
             textDocument/prepareTypeHierarchy            1         —\n\
             typeHierarchy/supertypes                     —         —\n\
             typeHierarchy/subtypes                       —         —\n\
             completionItem/resolve                     yes         —"
        );
    }

    /// The same call under three prefixes, one of which is prose in English.
    ///
    /// A line of markup is a line of a *document*, and a client counts its columns in UTF-16
    /// units of the text it has. Only the first and last lines here are ones where that agrees
    /// with the bytes: the middle line's quotes, accent and emoji are seven units short of
    /// their eighteen bytes.
    const WIDE: &str = "\
<p>plain <%= Story::TAGLINE %></p>
<p>\u{201c}curly\u{201d} caf\u{e9} \u{1f680} <%= Story::TAGLINE %></p>
<p><%= Story::TAGLINE %></p>
";

    /// The markup to the left of a cursor may not move it, whatever it is made of.
    ///
    /// A defect a corpus sweep finds rather than a test. An LSP position
    /// is a count of UTF-16 units of the text the **client** has, and `with_text` converted it
    /// against the blanked view — where a 3-byte `\u{201c}` in the markup has become three
    /// spaces. So the cursor was displaced left by (bytes − units) of every non-ASCII character
    /// in the markup before it on the line, and every range answered back was displaced right
    /// by the same amount. It is a *wrong* answer and not only a missing one: on a dense enough
    /// line the displaced cursor lands on a different identifier.
    ///
    /// Both directions are one bug and this asks about both at once. `definition` reads a
    /// position the client sent, and its `originSelectionRange` is a position the client will
    /// use — so the middle row is the assertion: the cursor arrives at column 30, and the span
    /// comes back naming columns 30 to 37 rather than the bytes 37 to 44 it was found at.
    ///
    /// It is the reproduction rather than an illustration. With the conversion pointed back at
    /// the view, the middle row answers **`story.rb:0`** — the seven-unit displacement puts the
    /// cursor inside `Story::`, and the jump lands on `class Story` instead of on the constant
    /// the user clicked. The other two rows are unmoved, which is what makes it a defect users
    /// only hit when they do not write their markup in English.
    #[test]
    fn a_cursor_in_a_template_is_where_the_editor_put_it() {
        /// The LSP position of `needle` on `line`, counted the way a client counts.
        fn at(source: &str, line: usize, needle: &str) -> serde_json::Value {
            let text = source.lines().nth(line).expect("the line");
            let column = text.find(needle).expect("the needle");
            serde_json::json!({
                "line": line,
                "character": text[..column].encode_utf16().count(),
            })
        }

        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", WIDE);
        harness.index();

        let mut drawn = vec![format!(
            "{:<8}{:>14}{:>16}",
            "prefix", "asked at", "jumps to"
        )];
        for (line, prefix) in ["ascii", "wide", "none"].iter().enumerate() {
            let asked = at(WIDE, line, "TAGLINE");
            let defined = harness.ask(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": { "uri": view.as_str() },
                    "position": asked.clone(),
                }),
            );
            let link = defined
                .as_array()
                .and_then(|links| links.first())
                .cloned()
                .unwrap_or_default();
            let origin = &link["originSelectionRange"];
            let span = match origin["start"]["character"].as_u64() {
                Some(_) => format!(
                    "{}-{}",
                    origin["start"]["character"], origin["end"]["character"]
                ),
                None => "\u{2014}".to_owned(),
            };
            let target = link["targetUri"].as_str().map_or_else(String::new, |uri| {
                uri.rsplit('/').next().unwrap_or_default().to_owned()
            });
            let jump = match link["targetSelectionRange"]["start"]["line"].as_u64() {
                Some(at) => format!("{target}:{at}"),
                None => "\u{2014}".to_owned(),
            };
            // The caret the editor placed, drawn beside the span it gets back: the two agree
            // only if the same text was counted on both trips.
            drawn.push(format!(
                "{prefix:<8}{:>14}{jump:>16}",
                format!("{}:{}", asked["character"], span)
            ));
        }

        assert_eq!(
            drawn.join("\n"),
            "prefix        asked at        jumps to\n\
             ascii         20:20-27      story.rb:1\n\
             wide          30:30-37      story.rb:1\n\
             none          14:14-21      story.rb:1"
        );
    }

    /// A rename drawn over the file the user actually has, markup and all.
    ///
    /// `Harness::renamed` draws what `with_text` hands out, which for a template is the blanked
    /// view — right for a Ruby file, and the thing that would hide the whole question here. The
    /// ranges a rename returns are the *template's* own offsets, so applying them to the real
    /// file is both the honest drawing and the assertion that byte-preserving blanking works.
    fn renamed_on_disk(
        harness: &mut Harness,
        uri: &DocUri,
        source: &str,
        needle: &str,
        to: &str,
    ) -> String {
        let answer = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, needle),
                "newName": to,
            }),
        );
        if answer.is_null() {
            return "null".to_owned();
        }
        let mut drawn = Vec::new();
        for (at, edits) in edits_in(&answer) {
            let at = DocUri::from_uri_str(&at).expect("a document URI");
            let on_disk = std::fs::read_to_string(at.to_path().expect("a path")).expect("readable");
            let mut text = TextDocument::new(on_disk, harness.analysis.encoding);
            for edit in edits.iter().rev() {
                text.apply(Some(edit.range), &edit.new_text);
            }
            drawn.push(format!("--- {} ---\n{}", file_name(&at), text.text()));
        }
        drawn.join("")
    }

    #[test]
    fn the_walk_indexes_a_template_nobody_opened_and_its_calls_are_references() {
        // The decision this test exists for, and the one that was reversed twice while it was
        // being made. Indexing only the templates the editor has open would pass every other
        // ERB test here and still be wrong: `references` would be complete or incomplete
        // depending on which tabs happened to be open, which is worse than a consistently
        // narrow answer.
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6", "index.html.erb:2:15"]
        );
    }

    #[test]
    fn a_template_reaching_the_graph_raw_would_record_no_references_at_all() {
        // Why the blanking is the feature rather than an optimisation. The same template under
        // an extension nothing recognises is read as Ruby, gives up in the first tag, and the
        // call sites simply are not there, which is the whole of what indexing a template
        // buys.
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        harness.write("app/views/stories/index.html.rhubarb", VIEW);
        harness.index();

        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6"]
        );
    }

    #[test]
    fn a_template_changed_on_disk_is_re_read_through_the_same_blanking() {
        // The watcher's route into the graph. `index_buffer` is the hook `didOpen`, `didChange`
        // and `didChangeWatchedFiles` all share, so a template that reached the graph raw
        // through any one of them would replace its own call sites with parse errors — the same
        // rule `.rbs` interfaces are held to, and for the same reason.
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", "<h1>none</h1>\n");
        harness.index();
        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6"]
        );

        harness.write("app/views/stories/index.html.erb", VIEW);
        harness.watch(&[&view]);
        harness.analysis.settle();

        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6", "index.html.erb:2:15"]
        );
    }

    #[test]
    fn a_non_ascii_line_of_markup_above_the_cursor_does_not_move_the_answer() {
        // Why the ERB scanner pads by byte and not by character: padding by character would let
        // the emoji on the line above shorten the buffer by three bytes, and every offset below
        // it would be wrong — silently, and only for people who do not write markup in English.
        let wide = VIEW.replace(
            "<h1>Stories</h1>",
            "<h1>\u{413}\u{43e}\u{440}\u{44f}\u{447}\u{438}\u{435} \u{1f525}</h1>",
        );
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        harness.write("app/views/stories/index.html.erb", &wide);
        harness.index();

        // The same line and the same column as the ASCII fixture above: the markup grew by
        // fourteen bytes and the Ruby did not move.
        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6", "index.html.erb:2:15"]
        );
    }

    #[test]
    fn a_local_renames_across_the_tag_it_was_declared_in() {
        // `rename` is the only module in the crate that writes, and `COVERAGE_FLOORS` holds it
        // at 100 for that reason — so the template path ships with its own fixtures or not at
        // all. The block parameter is declared in one tag and read in another, and what makes
        // the two edits land on the real file is that blanking moved no byte.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        assert_eq!(
            renamed_on_disk(&mut harness, &view, VIEW, "story| %>", "item"),
            "\
--- index.html.erb ---
<h1>Stories</h1>
<% @stories.each do |item| %>
  <p><%= item.title %> &mdash; <%= Story::TAGLINE %></p>
<% end %>
"
        );
    }

    #[test]
    fn a_constant_a_template_names_renames_with_its_declaration() {
        // The half that reaches out of the template: the
        // declaration is in a Ruby file this rename has to edit as well, at coordinates from
        // two different coordinate systems that are the same coordinate system.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        assert_eq!(
            renamed_on_disk(&mut harness, &view, VIEW, "TAGLINE %>", "STRAPLINE"),
            "\
--- story.rb ---
class Story
  STRAPLINE = \"news\"

  def title
    @title
  end
end
--- index.html.erb ---
<h1>Stories</h1>
<% @stories.each do |story| %>
  <p><%= story.title %> &mdash; <%= Story::STRAPLINE %></p>
<% end %>
"
        );
    }

    #[test]
    fn a_template_publishes_no_diagnostics_and_a_ruby_file_beside_it_still_does() {
        // What survives a correct scan is not about anything the user wrote: `<%= yield %>` in
        // a layout, which is legal in the method a template compiles to and refused by a parser
        // reading a file. Two of them over lobsters' 121 templates. A rule that fires on correct
        // input does not earn a squiggle.
        let mut harness = Harness::new();
        let broken = harness.write("app/models/story.rb", "class Story\n  def title\nend\n");
        let view = harness.write(
            "app/views/stories/index.html.erb",
            "<h1>Stories</h1>\n<% end %>\n<%= yield :head %>\n",
        );
        harness.index();

        // One drain, because reading the stream empties it: two `latest` calls would make the
        // second one answer `None` for a document that did publish.
        let published = harness.published();
        let for_uri = |uri: &DocUri| {
            published
                .iter()
                .filter(|(sent, _)| sent == uri.as_str())
                .count()
        };
        assert_eq!(for_uri(&view), 0, "{published:?}");
        assert_eq!(for_uri(&broken), 1, "{published:?}");
    }

    #[test]
    fn folding_is_declined_in_a_template_so_the_editor_keeps_its_own_guess() {
        // The walk sees the Ruby and nothing else, so what it offers is folds for the `<% %>`
        // blocks and none for the markup around them. `ranges.md` wrote the mechanism down
        // before ERB was on the table: a client that has a folding provider stops guessing from
        // indentation, so an empty array takes the fallback away *and* puts nothing in its
        // place, while a `null` can only hand it back.
        let mut harness = Harness::new();
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        let ruby = harness.write("app/models/story.rb", STORY);
        harness.index();

        let folds = |harness: &mut Harness, uri: &DocUri| {
            harness.ask(
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            )
        };
        assert!(folds(&mut harness, &view).is_null());
        // The control, and it is not decoration: the same template's Ruby *does* fold, so this
        // is a decision rather than an absence of anything to offer.
        assert!(!folds(&mut harness, &ruby).is_null());
    }

    #[test]
    fn a_namespace_nothing_writes_down_is_refused_and_what_it_holds_is_not() {
        let mut harness = Harness::new();
        let source = "\
Ghost::Thing = 1

def read
  Ghost::Thing
end
";
        let uri = harness.write("app/ghost.rb", source);
        harness.index();

        // `Ghost::Thing = 1` with no `module Ghost` anywhere leaves rubydex holding a `Ghost`
        // that is written down in no file at all. Renaming it would change every use of a name
        // whose one definition is somewhere ya-lsp cannot see — an excluded file, a gem that
        // did not resolve, a constant some metaprogramming makes — so it is refused for the
        // same reason a gem's name is, and with the same sentence.
        assert!(harness.prepare_rename(&uri, source, "Ghost").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Ghost")]
        );

        // And the control, which is what makes that a rule about the namespace rather than
        // about the line: the constant inside it is written down here, so it renames.
        assert_eq!(
            harness.renamed(&uri, source, "Thing = ", "Wraith"),
            "\
--- ghost.rb ---
Ghost::Wraith = 1

def read
  Ghost::Wraith
end
"
        );
    }

    // ------------------------------------- the view<->controller convention, and the guess

    /// The controller `app/views/stories/*` names, holding the two shapes a corpus has: an
    /// instance variable assigned something nameable, and one assigned an ActiveRecord chain.
    /// 88 of the corpus's 318 receiver sites are the first and most of the rest are the
    /// second, so a fixture with only the first would be a fixture written to flatter the
    /// feature.
    const CONTROLLER: &str = "\
class StoriesController
  def show
    @story = Story.new
  end

  def index
    @stories = Story.where(live: true)
  end
end
";

    /// A Rails application's three files, in the layout the convention reads.
    fn rails_app(harness: &Harness) -> DocUri {
        harness.write("app/models/story.rb", STORY);
        harness.write("app/controllers/stories_controller.rb", CONTROLLER);
        harness.write(
            "app/views/stories/show.html.erb",
            "<h1><%= @story.title %></h1>\n",
        )
    }

    #[test]
    fn a_templates_instance_variable_is_typed_by_the_controller_its_path_names() {
        // The view↔controller convention, end to end. A template has no enclosing class, so
        // the instance-variable machinery has nothing in the file to walk — the assignment is in another file that the template
        // never names, and what connects the two is a path.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        // The provenance, which is what makes a convention shippable: the class and the line,
        // both in a file the card is not drawn over.
        assert!(
            markdown.contains(
                "Type taken from `StoriesController`, line 3 — the controller Rails renders \
                 this template from."
            ),
            "{markdown}"
        );
        // And it is a derived answer rather than a guessed one, which the same card has to say
        // by *not* saying the other thing.
        assert!(
            !markdown.contains("guessed from the name"),
            "a convention that names a file is not a guess: {markdown}"
        );

        // A variable the controller never assigns is not this controller's to answer for, and
        // the convention says so by finding no assignment rather than by finding a wrong one.
        let missing = "<%= @missing.title %>\n";
        let other = harness.write("app/views/stories/edit.html.erb", missing);
        harness.index();
        let markdown = card(&mut harness, &other, missing, "title");
        assert!(!markdown.contains("StoriesController"), "{markdown}");

        // Completion asks the same question and must get the same answer.
        let offered = harness.declarations_at(&view, "<h1><%= @story.~ %></h1>\n");
        assert!(offered.contains(&"title".to_owned()), "{offered:?}");
        assert!(
            !offered.contains(&"show".to_owned()),
            "the controller's own methods are not the template's: {offered:?}"
        );
    }

    #[test]
    fn a_namespaced_view_reaches_the_namespaced_controller_or_nothing() {
        // `app/views/admin/stories/` is `Admin::StoriesController`, and the failure that
        // matters is not missing it — it is reaching the *top-level* `StoriesController`
        // instead, which is a different controller assigning a different variable.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/models/draft.rb",
            "class Draft\n  def slug\n  end\nend\n",
        );
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @thing = Story.new\n  end\nend\n",
        );
        harness.write(
            "app/controllers/admin/stories_controller.rb",
            "module Admin\n  class StoriesController\n    def show\n      @thing = Draft.new\n    end\n  end\nend\n",
        );
        let source = "<%= @thing.slug %>\n";
        let view = harness.write("app/views/admin/stories/show.html.erb", source);
        harness.index();

        let markdown = card(&mut harness, &view, source, "slug");
        assert!(markdown.contains("Draft#slug"), "{markdown}");
        assert!(
            markdown.contains("`Admin::StoriesController`"),
            "{markdown}"
        );
    }

    #[test]
    fn a_template_whose_controller_does_not_exist_reaches_for_no_other_one() {
        // The convention is the name and nothing like it. `app/views/comments/` names
        // `CommentsController`, which this application does not have — and the one it does have
        // assigns exactly the variable the template reads, so a lookup that fell back to
        // "something similar" would produce a confident, wrong, checkable-looking answer.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write("app/controllers/stories_controller.rb", CONTROLLER);
        let source = "<%= @story.title %>\n";
        let view = harness.write("app/views/comments/show.html.erb", source);
        harness.index();

        let markdown = card(&mut harness, &view, source, "title");
        assert!(
            !markdown.contains("StoriesController"),
            "no controller means no controller: {markdown}"
        );
        // What answers instead is the rung below, wearing its label. The two are separable and
        // this is where that shows: same file, same variable, a different tier.
        assert!(
            markdown.contains("Type guessed from the name `@story` alone"),
            "{markdown}"
        );
    }

    #[test]
    fn an_instance_variable_assigned_an_active_record_chain_is_not_typed() {
        // The common case, pinned. `Story.where(...)` is a chain whose return type nothing
        // declares, so the convention answers nothing for `@stories` — and `@stories` does not
        // spell a class either, so neither does the guess. This is the ceiling on the tier, and it
        // is the annotations rather than the machinery.
        let mut harness = Harness::new();
        rails_app(&harness);
        // The template calls a method the *model* declares, deliberately: an untyped receiver
        // and a receiver typed to the wrong class both answer `Story#title`, and what tells
        // them apart is the footnote. A call to something nothing declares would answer `null`
        // either way and pin nothing.
        let source = "<%= @stories.title %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        harness.index();

        let markdown = card(&mut harness, &view, source, "title");
        assert!(
            markdown.contains("Matched on the method name alone"),
            "{markdown}"
        );
        assert!(!markdown.contains("StoriesController"), "{markdown}");
        assert!(!markdown.contains("guessed from the name"), "{markdown}");
    }

    #[test]
    fn a_controller_edited_but_not_saved_types_the_template_it_renders() {
        // The reason the controller's text is read through the buffer accessor rather than
        // straight off disk. rubydex re-indexes an open buffer on every keystroke, so the graph
        // is already ahead of the file; reading the assignment from disk would make the one
        // answer that crosses a file boundary the one answer that lags behind it.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/models/draft.rb",
            "class Draft\n  def slug\n  end\nend\n",
        );
        let controller = harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @thing = Story.new\n  end\nend\n",
        );
        let source = "<%= @thing.slug %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        harness.open(
            &controller,
            "class StoriesController\n  def show\n    @thing = Draft.new\n  end\nend\n",
        );
        let markdown = card(&mut harness, &view, source, "slug");
        assert!(
            markdown.contains("Draft#slug"),
            "the unsaved buffer is what the controller says: {markdown}"
        );
    }

    #[test]
    fn a_controller_the_graph_holds_and_the_disk_does_not_answers_nothing() {
        // The graph and the filesystem can disagree for as long as it takes a change to reach
        // the walk, and this rung is the one place a request reads a file the cursor is not in.
        // A controller deleted since the index was built has to answer nothing rather than
        // panic or produce a stale type off a path that no longer resolves.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();
        std::fs::remove_file(
            harness
                .root
                .path()
                .join("app/controllers/stories_controller.rb"),
        )
        .unwrap();

        let source = "<h1><%= @story.title %></h1>\n";
        let markdown = card(&mut harness, &view, source, "title");
        assert!(!markdown.contains("StoriesController"), "{markdown}");
        // The rung below still answers, which is what makes this a missing file rather than a
        // broken request.
        assert!(
            markdown.contains("Type guessed from the name `@story` alone"),
            "{markdown}"
        );
    }

    #[test]
    fn a_guess_never_displaces_an_answer_the_code_states() {
        // The rung ordering, and the one thing that makes the last rung safe to ship.
        // `@user` here is assigned a `Draft` in its own class, and a class called `User` is
        // sitting in the graph waiting to be guessed at — so if the rungs were the other way
        // round, or even merely tried in parallel, this card would say `User`.
        let mut harness = Harness::new();
        harness.write("app/models/user.rb", "class User\n  def slug\n  end\nend\n");
        harness.write(
            "app/models/draft.rb",
            "class Draft\n  def slug\n  end\nend\n",
        );
        let source = "\
class Session
  def start
    @user = Draft.new
  end

  def render
    @user.slug
  end
end
";
        let uri = harness.write("app/session.rb", source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "slug");
        assert!(markdown.contains("Draft#slug"), "{markdown}");
        assert!(
            markdown.contains("Type taken from the assignment on line 3"),
            "{markdown}"
        );
        assert!(
            !markdown.contains("guessed from the name"),
            "an assignment in the same class is not a guess: {markdown}"
        );
    }

    #[test]
    fn the_guess_can_be_turned_off_and_the_convention_stays() {
        // The setting exists because this is the first answer ya-lsp gives that is allowed to
        // be wrong, and a user who wants only checkable answers should be able to have them.
        // What that must *not* do is take the derived tier with it: a controller and a line is
        // an answer somebody can go and check, which is the whole difference.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n\
             [types]\nguess_from_names = false\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let view = rails_app(&harness);
        let guessed_source = "<%= @story.title %>\n";
        let elsewhere = harness.write("app/views/comments/show.html.erb", guessed_source);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let kept = card(&mut harness, &view, source, "title");
        assert!(kept.contains("`StoriesController`"), "{kept}");

        // The same variable, the same class in the graph, and no controller to reach it
        // through: with the guess off there is nothing left to say.
        let silenced = card(&mut harness, &elsewhere, guessed_source, "title");
        assert!(
            silenced.contains("Matched on the method name alone"),
            "{silenced}"
        );
        assert!(!silenced.contains("guessed from the name"), "{silenced}");
    }

    #[test]
    fn a_completion_row_says_the_class_it_was_offered_for_was_guessed() {
        // The tier vocabulary, extended to the guess. A guessed receiver is not the
        // name-based list — the rows really are one class's members, which is a better list —
        // so `precise` stays true and a second field carries the doubt. Saying nothing would
        // present six letters of inference as a resolved type.
        let mut harness = Harness::new();
        harness.write(
            "app/models/person.rb",
            "class Person\n  def shout\n  end\nend\n",
        );
        let uri = harness.write("app/main.rb", "");
        harness.index();

        let offered = harness.complete(&uri, "person.sh~");
        let row = offered["items"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .expect("a row");
        assert_eq!(row["label"], "shout");
        let card = harness.ask("completionItem/resolve", row)["documentation"]["value"]
            .as_str()
            .unwrap_or("(nothing)")
            .to_owned();
        assert!(card.contains("Person#shout"), "{card}");
        assert!(
            card.contains("Type guessed from the name `person` alone"),
            "{card}"
        );
        assert!(
            !card.contains("Matched on the method name alone"),
            "the rows are a real class's members, and this is the other guess: {card}"
        );
    }

    #[test]
    fn a_definition_jumps_to_the_same_place_the_card_names() {
        // Both new rungs go through `resolve_typed`, which hover and go-to-definition share
        // for one reason: a card and a jump that disagreed about what
        // `@story.` is would be worse than either being absent.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let jumped = harness.definition_at(&view, source, "title");
        let target = jumped[0]["targetUri"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(target.ends_with("story.rb"), "{jumped}");
    }

    // ------------------------------------------------------ what a template can call

    /// A controller, a helper and a template, in the layout the view context reads.
    ///
    /// The controller exports one of its two methods, deliberately: `helper_method` is a
    /// permission and the whole of the gate on that half, so a fixture with one method
    /// could not tell "this template may call it" from "this project defines it once".
    fn view_context_app(harness: &Harness) -> DocUri {
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  helper_method :current_user\n\n  def current_user\n  end\n\n  def set_story\n  end\nend\n",
        );
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        harness.write("app/views/stories/show.html.erb", "<%= current_user %>\n")
    }

    #[test]
    fn a_helper_method_export_is_what_a_template_may_call() {
        // The view context, first clause. `current_user` in a template already jumps —
        // there is one method of that name in the project — and it jumped on the *name* rung,
        // which is the same answer it would give if the controller had never exported it. What
        // changes is the tier and the gate: the answer is now reached through the class the
        // path names, and the method the class did **not** export is not reachable at all.
        let mut harness = Harness::new();
        let view = view_context_app(&harness);
        harness.index();

        let source = "<%= current_user %>\n";
        let exported = card(&mut harness, &view, source, "current_user");
        assert!(
            exported.contains("StoriesController#current_user"),
            "{exported}"
        );
        assert!(
            exported.contains(
                "Reached through `helper_method` in `StoriesController` — the class Rails \
                 renders this template from."
            ),
            "{exported}"
        );
        assert!(
            !exported.contains("Matched on the method name alone"),
            "a convention that names a class is not a name match: {exported}"
        );

        let jump = harness.definition_at(&view, source, "current_user");
        assert!(
            jump[0]["targetUri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("stories_controller.rb"),
            "{jump}"
        );

        // The obvious half is the one a two-example probe can see; this is the other
        // one, and it is thirty times larger over the corpus.
        let offered = harness.declarations_at(&view, "<%= curr~ %>\n");
        assert!(offered.contains(&"current_user".to_owned()), "{offered:?}");

        // And the method nobody exported is not in the view context, in either request. It is
        // still *findable* — one `set_story` in the project, so the name rung answers — and the
        // card says so, which is the difference this gate exists to keep.
        let unexported = "<%= set_story %>\n";
        let other = harness.write("app/views/stories/edit.html.erb", unexported);
        harness.watch(&[&other]);
        let private = card(&mut harness, &other, unexported, "set_story");
        assert!(
            private.contains("Matched on the method name alone"),
            "`helper_method` is the gate, per name: {private}"
        );
        let offered = harness.declarations_at(&other, "<%= set_s~ %>\n");
        assert!(!offered.contains(&"set_story".to_owned()), "{offered:?}");
    }

    #[test]
    fn an_export_this_cannot_read_a_name_out_of_hands_over_nothing() {
        // The direction every reader in `workspace/rails` errs in, asked of the one macro whose
        // answer is a permission rather than a member. A splat and an interpolated symbol are
        // Ruby that only runs, and a bare `helper_method` is a no-op Rails accepts — so all
        // three export nothing, and `render_story` goes on answering what it answered before
        // rather than becoming callable because a call of the right name was written.
        let mut harness = Harness::new();
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  EXPORTS = [:render_story]\n  helper_method\n  \
             helper_method(*EXPORTS)\n  helper_method :\"#{prefix}_story\"\n\n  \
             def render_story\n  end\nend\n",
        );
        let source = "<%= render_story %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let card = card(&mut harness, &view, source, "render_story");
        assert!(
            card.contains("Matched on the method name alone"),
            "a name no literal spelled is a name nobody exported: {card}"
        );
    }

    #[test]
    fn every_app_helpers_module_is_in_every_template() {
        // The other half, and it needs no macro at all: Rails' `all_helpers_from_path` globs
        // `app/helpers/**/*_helper.rb` and includes every module it finds in every view
        // context. So this template's controller does not exist — `app/views/comments/` names a
        // `CommentsController` this application has never written — and the helpers answer
        // anyway, which is what 489 of the corpus' 913 partials live on.
        let mut harness = Harness::new();
        view_context_app(&harness);
        harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\n  def byline\n  end\nend\n",
        );
        // Under `app/helpers` and not named the way Rails' glob names them: this is solidus'
        // `controller_helpers/auth.rb` shape, which is reached by an `include` a controller
        // writes and is in no view context by default.
        harness.write(
            "app/helpers/legacy/auth.rb",
            "module Legacy\n  module Auth\n    def sign_out\n    end\n  end\nend\n",
        );
        let source = "<%= time_ago(1) %> <%= sign_out %>\n";
        let view = harness.write("app/views/comments/index.html.erb", source);
        harness.index();

        let globbed = card(&mut harness, &view, source, "time_ago");
        assert!(globbed.contains("ApplicationHelper#time_ago"), "{globbed}");
        assert!(
            globbed.contains(
                "Reached through the view context — Rails includes every `app/helpers` module \
                 in every template."
            ),
            "{globbed}"
        );

        let unglobbed = card(&mut harness, &view, source, "sign_out");
        assert!(
            unglobbed.contains("Matched on the method name alone"),
            "a file Rails' own glob does not name is in no view context: {unglobbed}"
        );

        // Every module, not the one whose name matches the directory: `include_all_helpers` is
        // the Rails default, and an application that turns it off is a bound this states rather
        // than reads.
        let offered = harness.declarations_at(&view, "<%= ~ %>\n");
        for name in ["time_ago", "byline"] {
            assert!(offered.contains(&name.to_owned()), "{name}: {offered:?}");
        }
        assert!(!offered.contains(&"sign_out".to_owned()), "{offered:?}");
    }

    #[test]
    fn where_the_two_halves_meet_the_export_wins_and_the_row_is_not_offered_twice() {
        // Three refusals and one precedence, in one fixture, because they are one question.
        //
        // **The export wins.** `helper_method` writes its proxy *on* `_helpers` and `helper`
        // includes a module *into* it, so where both hold a name the proxy is what runs. A third
        // of real export sites name a method that is also an `app/helpers` `def`, so this is the
        // commonest shape the two halves have together and not an edge.
        //
        // **One name is one row.** The same pair in a completion list would be the same word
        // twice, one of which jumps somewhere the call would not go.
        //
        // **An export naming nothing declares nothing.** `helper_method :missing` is a
        // permission for a method that does not exist, so the half below it answers instead.
        //
        // **A constant in a helper module is not in the view context.** `include` installs
        // methods; `ApplicationHelper::MAX` is reached by writing it out.
        let mut harness = Harness::new();
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  helper_method :current_user, :missing\n\n  \
             def current_user\n  end\nend\n",
        );
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  MAX = 5\n\n  def current_user\n  end\n\n  \
             def time_ago(at)\n  end\nend\n",
        );
        // Somewhere the view context cannot reach, so that the export declining is visible as
        // the rung below answering rather than as a hover with nothing on it.
        harness.write("lib/tools.rb", "module Tools\n  def missing\n  end\nend\n");
        let source = "<%= current_user %> <%= missing %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let shared = card(&mut harness, &view, source, "current_user");
        assert!(
            shared.contains("StoriesController#current_user"),
            "the proxy is written on `_helpers` and the module is included into it: {shared}"
        );
        assert!(
            shared.contains("`helper_method` in `StoriesController`"),
            "{shared}"
        );

        let unwritten = card(&mut harness, &view, source, "missing");
        assert!(
            unwritten.contains("Matched on the method name alone"),
            "a permission for a method nobody wrote is not an answer: {unwritten}"
        );

        let offered = harness.declarations_at(&view, "<%= ~ %>\n");
        assert_eq!(
            offered
                .iter()
                .filter(|label| *label == "current_user")
                .count(),
            1,
            "one name is one row: {offered:?}"
        );
        assert!(offered.contains(&"time_ago".to_owned()), "{offered:?}");
        assert!(
            !offered.contains(&"MAX".to_owned()),
            "`include` installs methods and nothing else: {offered:?}"
        );
    }

    #[test]
    fn an_export_written_in_a_concern_reaches_the_controllers_that_include_it() {
        // 8 of the six corpora's 60 exported names are written in a concern and 7 more in a
        // module under `app/helpers` that a controller `include`s, so a reader that walked
        // `app/controllers` and keyed by class would find 24 of solidus' exports and reach 3 of
        // its 113 sites. Nothing here knows what a concern is: the export list is keyed by the
        // body that wrote the macro, and the walk from the template's class up its own
        // ancestors is what crosses the `include` the controller already wrote.
        let mut harness = Harness::new();
        harness.write(
            "app/controllers/concerns/authentication.rb",
            "module Authentication\n  extend ActiveSupport::Concern\n\n  included do\n    \
             helper_method :current_user\n  end\n\n  def current_user\n  end\nend\n",
        );
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  include Authentication\nend\n",
        );
        let source = "<%= current_user %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let through = card(&mut harness, &view, source, "current_user");
        assert!(through.contains("Authentication#current_user"), "{through}");
        // The class the *template* names, not the module the macro is in: what a reader has to
        // be able to check is that Rails renders this template from there.
        assert!(
            through.contains("`helper_method` in `StoriesController`"),
            "{through}"
        );
    }

    #[test]
    fn a_mailer_gets_its_own_exports_and_not_the_applications_helpers() {
        // `AbstractController::Helpers` is in `ActionMailer::Base` too, so `helper_method` in a
        // mailer is real — 3 of the corpus' 60 — and the view path a mailer renders from is its
        // own name with no `Controller` on the end. `include_all_helpers` is **not**: it is
        // `ActionController::Base`'s default, and a mailer reaches an application helper only
        // by writing `helper` itself, which is a call this reader does not read. So the two
        // halves separate here and nowhere else.
        let mut harness = Harness::new();
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  helper_method :sender_name\n\n  \
             def sender_name\n  end\nend\n",
        );
        let source = "<%= sender_name %> <%= time_ago(1) %>\n";
        let view = harness.write("app/views/user_mailer/welcome.html.erb", source);
        harness.index();

        let exported = card(&mut harness, &view, source, "sender_name");
        assert!(exported.contains("UserMailer#sender_name"), "{exported}");
        assert!(
            exported.contains("`helper_method` in `UserMailer`"),
            "{exported}"
        );

        let helper = card(&mut harness, &view, source, "time_ago");
        assert!(
            helper.contains("Matched on the method name alone"),
            "a mailer's views are not a controller's: {helper}"
        );

        // And the way back in is the one Rails gives: `helper :application` names the module,
        // and a mailer that names it has said the only thing that puts an application helper in
        // front of a mailer template. Most real `helper` calls are in a mailer, and they are the
        // whole of the gap.
        let mailer = harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  helper :application\n  \
             helper_method :sender_name\n\n  def sender_name\n  end\nend\n",
        );
        harness.watch(&[&mailer]);
        let named = card(&mut harness, &view, source, "time_ago");
        assert!(named.contains("ApplicationHelper#time_ago"), "{named}");
        assert!(
            named.contains("Reached through the view context"),
            "{named}"
        );

        // And the directory has to name a **mailer**: `mailer_of` spells whatever the path
        // spells, so the gate is the application's own superclass table and not the path.
        let shared = "<%= sender_name %>\n";
        let other = harness.write("app/views/shared/_footer.html.erb", shared);
        harness.watch(&[&other]);
        let elsewhere = card(&mut harness, &other, shared, "sender_name");
        assert!(
            elsewhere.contains("Matched on the method name alone"),
            "`app/views/shared/` names `Shared`, which renders nothing: {elsewhere}"
        );
    }

    #[test]
    fn the_view_context_is_additive_and_reaches_no_further_than_a_template() {
        // Two refusals in one fixture, and both are the item's safety rather than its feature.
        //
        // **Additive.** The view context is not the two halves — it is the two halves plus
        // every helper module ActionView ships, which this crate does not model. Lobsters
        // writes `def tag` in `ApplicationHelper` deliberately, to shadow
        // `ActionView::Helpers::TagHelper#tag`, and a template's `tag` should reach the
        // application's. A template's `link_to`, which the application does not redefine, has
        // to go on answering exactly what it answered before.
        //
        // **A template and nothing else.** The same bare call in a `.rb` file is an ordinary
        // receiverless call whose `self` is `main`, and Rails puts no helpers on that.
        let mut harness = Harness::new();
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def tag(name)\n  end\nend\n",
        );
        harness.write(
            "lib/markup.rb",
            "module Markup\n  def link_to(text, url)\n  end\nend\n",
        );
        let source = "<%= tag(:p) %> <%= link_to(\"x\", \"/\") %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        let script = "tag(:p)\n";
        let plain = harness.write("bin/report.rb", script);
        harness.index();

        let own = card(&mut harness, &view, source, "tag");
        assert!(own.contains("ApplicationHelper#tag"), "{own}");
        assert!(!own.contains("Matched on the method name alone"), "{own}");

        let through = card(&mut harness, &view, source, "link_to");
        assert!(
            through.contains("Matched on the method name alone"),
            "a name the view context does not hold falls through, it is not swallowed: {through}"
        );

        let outside = card(&mut harness, &plain, script, "tag");
        assert!(
            outside.contains("Matched on the method name alone"),
            "nothing outside a template changes: {outside}"
        );
    }

    // The generator seam

    /// What one pass over the graph collects, including the two projections nothing reads yet.
    ///
    /// The registry is the point: four lists, three predicates and one loop, so an item that
    /// wants a fifth list adds a row rather than a walk. `modules` and `superclasses` are here
    /// because the concern, entry-point and routes readers need them and they cost the same
    /// loop — a projection added later is a second walk over every document in the workspace
    /// that nobody notices.
    #[test]
    fn what_one_pass_over_the_graph_collects() {
        let mut harness = Harness::new();
        harness.write("db/schema.rb", "ActiveRecord::Schema[8.0].define do\nend\n");
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\nend\n",
        );
        harness.write("app/models/concerns/storyish.rb", "module Storyish\nend\n");
        harness.write(
            "app/models/legacy.rb",
            "class Legacy < ActiveRecord::Base\n  self.table_name = \"old\"\nend\n",
        );
        // Both annotation shapes in one file, which is the case the single loop exists for: a
        // `sig` *and* a YARD tag put it on the annotated list once, and twice would generate it
        // twice.
        harness.write(
            "app/lib/widget.rb",
            "class Widget\n  sig { returns(String) }\n  def go\n  end\n\n  \
             # @return [Integer]\n  def size\n  end\nend\n",
        );
        harness.index();

        let context = harness.analysis.context();
        let named = |list: synthesize::List| -> Vec<String> {
            context
                .documents(list)
                .iter()
                .map(|uri| {
                    uri.rsplit('/')
                        .take(2)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .collect()
        };
        assert_eq!(named(synthesize::List::Schemas), ["db/schema.rb"]);
        // `legacy.rb` writes no macro at all and is on the list anyway: it defines a model, so
        // it is where that model's relation class and class side belong.
        // The only membership decided after the walk rather than during it.
        assert_eq!(
            named(synthesize::List::Models),
            ["models/legacy.rb", "models/story.rb"]
        );
        assert_eq!(named(synthesize::List::Renamed), ["models/legacy.rb"]);
        assert_eq!(named(synthesize::List::Annotated), ["lib/widget.rb"]);

        // Every class *and* module, because that is the bound on what a macro may name; and the
        // modules on their own, because a module is a different thing to declare on.
        assert!(
            ["Story", "Storyish", "Legacy", "Widget"]
                .iter()
                .all(|name| context.classes.contains(*name)),
            "{:?}",
            context.classes
        );
        assert_eq!(
            context
                .modules
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["Storyish"]
        );

        // Spelled as written and not qualified: the test applied to it is a suffix, and
        // a superclass a class does not define is still read — `ApplicationRecord` is the base
        // and `ActiveRecord::Base` is a gem's.
        assert_eq!(
            context.superclasses.get("Story").map(String::as_str),
            Some("ApplicationRecord")
        );
        assert_eq!(
            context.superclasses.get("Legacy").map(String::as_str),
            Some("ActiveRecord::Base")
        );
        assert_eq!(context.superclasses.get("Widget"), None);

        // A table claimed by exactly one top-level class, which is what the schema generator
        // then filters for ambiguity.
        assert_eq!(
            context.claims.get("stories").map(Vec::as_slice),
            Some(["Story".to_owned()].as_slice())
        );
    }

    /// A class written inside a `class << self` body is not a class this pass can name.
    ///
    /// Its lexical nesting is the singleton class, whose own name is `ParentScope::Attached` —
    /// not a constant anybody writes — so `qualified_name` answers `None` and the walk moves on.
    /// The rule it protects is the one every generator inherits: a class the application does
    /// not define declares nothing, and "cannot be spelled" and "is not defined" have to reach
    /// the same answer.
    #[test]
    fn a_class_inside_a_singleton_class_body_is_not_a_class_this_pass_names() {
        let mut harness = Harness::new();
        harness.write(
            "app/models/outer.rb",
            "class Outer\n  class << self\n    class Inner\n    end\n\n    module Deeper\n    \
             end\n  end\nend\n",
        );
        harness.index();

        let context = harness.analysis.context();
        assert!(context.classes.contains("Outer"), "{:?}", context.classes);
        assert!(
            !context
                .classes
                .iter()
                .any(|name| name.contains("Inner") || name.contains("Deeper")),
            "{:?}",
            context.classes
        );
        assert!(context.modules.is_empty(), "{:?}", context.modules);
    }

    /// A class Ruby accepts and Rails' inflector does not claims no table.
    ///
    /// `class Ünicode` is legal Ruby — a constant must begin with an uppercase letter, and Ruby
    /// means Unicode's uppercase — and `underscore` deliberately requires an *ASCII* capital,
    /// because there is no acronym table here and no way to guess what such a class's table
    /// would be called. The failure direction is the one the schema is built to have: no claim,
    /// rather than a claim on a table that does not exist.
    #[test]
    fn a_class_whose_name_is_not_ascii_claims_no_table() {
        let mut harness = Harness::new();
        harness.write("app/models/unicode.rb", "class Ünicode\nend\n");
        harness.index();

        let context = harness.analysis.context();
        assert!(context.classes.contains("Ünicode"), "{:?}", context.classes);
        assert!(context.claims.is_empty(), "{:?}", context.claims);
    }

    /// A workspace that stops declaring anything altogether still gets pruned.
    ///
    /// The clause is `synthesize`'s first line: it returns early only when there is nothing to
    /// read **and** nothing it wrote last time. Deleting every model file makes the first true
    /// and the second false, which is the one shape where an early return would leave a
    /// `Comment::Relation` in the graph for the life of the process — with no file left that
    /// could ever be edited to correct it.
    #[test]
    fn a_workspace_that_stops_declaring_anything_is_still_pruned() {
        let mut harness = Harness::new();
        let story = harness.write(
            "app/models/story.rb",
            "class Story\n  has_many :comments\nend\n",
        );
        let comment = harness.write("app/models/comment.rb", "class Comment\nend\n");
        harness.index();
        assert!(harness.has("Comment::Relation"), "nothing generated");

        std::fs::remove_file(story.to_path().unwrap()).unwrap();
        std::fs::remove_file(comment.to_path().unwrap()).unwrap();
        harness.watch(&[&story, &comment]);
        assert!(!harness.has("Comment::Relation"), "left behind");
        assert!(!harness.has("Story#comments()"), "left behind");
    }

    /// A file that vanishes between the walk and the read costs its declarations and nothing
    /// else.
    ///
    /// `synthesize` runs before every resolve and reads from the graph's document list, which a
    /// watcher event has not necessarily caught up with. There is no notification to wait for
    /// and nothing to recover: the file is skipped, the rest of the pass runs, and the next
    /// event prunes it properly.
    #[test]
    fn a_file_that_vanished_under_the_pass_is_skipped() {
        let mut harness = Harness::new();
        let widget = harness.write(
            "app/lib/widget.rb",
            "class Widget\n  # @return [String]\n  def go\n  end\nend\n",
        );
        harness.write(
            "app/lib/gadget.rb",
            "class Gadget\n  # @return [Integer]\n  def size\n  end\nend\n",
        );
        harness.index();
        assert!(harness.has("Widget#go()"));

        // Deleted on disk and *not* announced, so the document is still in the graph and its
        // text is not.
        std::fs::remove_file(widget.to_path().unwrap()).unwrap();
        harness.analysis.synthesize();
        harness.analysis.resolve();
        assert!(harness.has("Gadget#size()"), "the pass stopped early");
    }

    // ---------------------------------------------------------------------------------------
    // `delegate`, and the second phase
    // ---------------------------------------------------------------------------------------

    /// A schema, two models and a `delegate` between them — the two hops, end to end.
    ///
    /// `Story#user` is a `belongs_to` the model generator writes, `User#username` is a column
    /// the *schema* generator writes into a different file's generated document, and nothing has
    /// resolved when either of them is asked. That is the two-phase seam in a fixture.
    fn delegates_project(caller: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  \
             belongs_to :user\n  \
             delegate :username, :description, to: :user\n  \
             delegate :username, to: :user, prefix: true\n  \
             delegate :title, to: :user\n  \
             delegate :name=, to: :user\n  \
             delegate :anything, to: :@config\n\
             end\n",
        );
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 2024_01_01_000000) do\n  \
             create_table \"stories\", force: :cascade do |t|\n    \
             t.string \"title\", null: false\n  \
             end\n\n  \
             create_table \"users\", force: :cascade do |t|\n    \
             t.string \"username\", null: false\n  \
             end\n\
             end\n",
        );
        let uri = harness.write("app/main.rb", caller);
        harness.index();
        harness.index_gems();
        (harness, story, uri)
    }

    #[test]
    fn a_delegate_types_through_two_hops_and_jumps_to_its_own_symbol() {
        // A `delegate` in one expression, and the three things that have to happen at
        // once are the same three every generator in this half is asked for: the member exists,
        // the chain off it is typed — which needs *both* hops, an association in this file and a
        // column in another — and the jump lands on the `:username` symbol inside the call
        // rather than anywhere else in the class.
        let source = "Story.new.username.upcase\n";
        let (mut harness, story, uri) = delegates_project(source);

        assert!(
            harness.has("Story#username()"),
            "the delegated name is not a member"
        );

        let card = card(&mut harness, &uri, source, "upcase");
        assert!(card.contains("String#upcase"), "{card}");

        let definition = harness.definition_at(&uri, source, "username");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(story.as_str()),
            "{definition}"
        );
        // `  delegate :username, :description, to: :user` on line 2, revealed whole, with the
        // one name of the two that was asked for selected past its colon.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(2),
                &serde_json::json!(2),
                &serde_json::json!(12),
            ),
            "{definition}"
        );
    }

    #[test]
    fn a_hover_on_a_delegated_name_says_which_file_and_which_call_it_came_from() {
        // The provenance rule again, and it is load-bearing here in a way it is not for a
        // column: a delegated type is two derivations deep, so a card that did not say so would
        // be presenting the *target's* schema as though this class declared it.
        let source = "Story.new.username\n";
        let (mut harness, _story, uri) = delegates_project(source);

        let card = card(&mut harness, &uri, source, "username");
        assert!(card.contains("Story#username"), "{card}");
        assert!(card.contains("app/models/story.rb"), "{card}");
        assert!(card.contains("delegate :username"), "{card}");
        assert!(card.contains("to: :user"), "{card}");
    }

    #[test]
    fn what_a_delegate_declares_and_what_it_declines_to_type() {
        // The decline direction, which is the one thing this reader does differently from every
        // other one in the directory: what is declined is the **type** and never the member.
        // Rails defines all four of these methods whatever `to:` holds at run time, so all four
        // exist — and the two that cannot be typed simply carry no return type, which
        // `Types::harvest` drops rather than believing.
        let source = "Story.new.username.upcase\nStory.new.description.length\n";
        let (mut harness, _story, uri) = delegates_project(source);

        assert!(harness.has("Story#username()"), "both hops answered");
        assert!(harness.has("Story#user_username()"), "prefix: true");
        assert!(
            harness.has("Story#description()"),
            "a name the target's schema does not hold is still a member"
        );
        assert!(
            harness.has("Story#anything()"),
            "an ivar target is still a member"
        );
        assert!(
            harness.has("Story#name=()"),
            "a setter is a name RBS takes, and one this document would be refused whole for"
        );

        // The chain is what tells the two apart, and it is the whole reason an untyped
        // declaration is safe: `untyped` is dropped from the return table, so `.upcase` off the
        // one that could not be typed falls to the name rung exactly as it would have with no
        // declaration at all — while the one that could be typed resolves.
        let derived = card(&mut harness, &uri, source, "upcase");
        assert!(derived.contains("String#upcase"), "{derived}");
        assert!(
            !derived.contains("Matched on the method name alone"),
            "{derived}"
        );
        let guessed = card(&mut harness, &uri, source, "length");
        assert!(
            guessed.contains("Matched on the method name alone"),
            "an untyped delegation must add no entry to the return table: {guessed}"
        );
    }

    #[test]
    fn a_column_outranks_a_delegate_of_the_same_name() {
        // Rank 3 over rank 8, and the reason a rank has to be spendable from *outside* one
        // document. `Story` has a `title` column and a `delegate :title, to: :user`; the column
        // is what the database is and the delegation is a claim about another class that does
        // not even hold the name. The two land in two different generated documents, where
        // `Facts`' own precedence can never see the pair — so without the decline this is two
        // `def title:` lines, one place too many in the card, and a type decided by whichever
        // document `Types::harvest` read last.
        let source = "Story.new.title.upcase\n";
        let (mut harness, _story, uri) = delegates_project(source);

        let column = card(&mut harness, &uri, source, "title");
        assert!(
            column.contains("db/schema.rb"),
            "the delegate won: {column}"
        );
        assert!(!column.contains("delegate :title"), "{column}");
        assert!(!column.contains("Defined in"), "two declarations: {column}");
        // And the type is the column's, rather than the `untyped` the delegation would have
        // carried into the same key.
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(chained.contains("String#upcase"), "{chained}");
    }

    #[test]
    fn a_second_pass_does_not_derive_from_what_the_first_one_delegated() {
        // The phase boundary, asserted *across settles* rather than within one — which is the
        // shape that could rot silently. `synthesize` runs before every resolve, so if the union
        // ever held phase two's own output the answer would change on the second keystroke and
        // keep changing: a `delegate` through a `delegate` would type on run two, a chain of
        // three on run three. It cannot, because the union is built from a map that is fresh
        // every call and is built before anything is merged into it — and this is what says so.
        //
        // `Story#profile` is a delegation that *does* type, through `belongs_to :user` and
        // `has_one :profile`. `Story#bio` delegates through it, and must stay untyped however
        // many times the pass runs — which only a chain can show, because a hover card names no
        // return type.
        let mut harness = Harness::new();
        // Every one of them writes `< ApplicationRecord`, because of the host test: a class
        // that inherits nothing is not an ActiveRecord model and its association macros declare
        // nothing at all.
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  belongs_to :user\n  \
             delegate :profile, to: :user\n  delegate :bio, to: :profile\nend\n",
        );
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\n  has_one :profile\nend\n",
        );
        harness.write(
            "app/models/profile.rb",
            "class Profile < ApplicationRecord\n  has_one :bio\nend\n",
        );
        harness.write(
            "app/models/bio.rb",
            "class Bio < ApplicationRecord\n  has_one :photo\nend\n",
        );
        harness.write(
            "app/models/photo.rb",
            "class Photo < ApplicationRecord\nend\n",
        );
        let source = "Story.new.profile.bio\nStory.new.bio.photo\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let typed = card(&mut harness, &uri, source, "bio\n");
        assert!(
            typed.contains("Profile#bio"),
            "the delegation that can type did not: {typed}"
        );
        let through = card(&mut harness, &uri, source, "photo");
        assert!(
            through.contains("Matched on the method name alone"),
            "phase two derived from itself: {through}"
        );

        let documents = harness.analysis.synthesized.len();
        harness.analysis.synthesize();
        harness.analysis.resolve();
        assert_eq!(harness.analysis.synthesized.len(), documents);
        assert_eq!(card(&mut harness, &uri, source, "bio\n"), typed);
        assert_eq!(card(&mut harness, &uri, source, "photo"), through);
    }
}
