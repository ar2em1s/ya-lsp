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

pub mod completion;
pub mod cursor;
pub mod diagnostics;
pub mod hierarchy;
pub mod highlight;
pub mod hover;
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
pub mod symbols;

use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_server::{ErrorCode, Message, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CompletionItem, CompletionItemKind, CompletionItemTag, CompletionList,
    CompletionResponse, CompletionTextEdit, DiagnosticSeverity, DocumentHighlight,
    DocumentSymbolResponse, Documentation, FoldingRange, GotoDefinitionResponse, Hover,
    HoverContents, Location, LocationLink, MarkupContent, MarkupKind, OneOf,
    OptionalVersionedTextDocumentIdentifier, PrepareRenameResponse, SelectionRange, SignatureHelp,
    SymbolInformation, TextDocumentEdit, TextEdit, TypeHierarchyItem, WorkspaceEdit,
    WorkspaceSymbolResponse,
};
use rubydex::{
    indexing::{self, IndexerBackend, LanguageId},
    model::{
        graph::Graph,
        ids::{DeclarationId, UriId},
    },
    resolution::Resolver,
};

use crate::messages;
use crate::workspace::{DocUri, Workspace, gems};
use locator::Site;
use position::{PositionEncoding, TextDocument};
use progress::Progress;

/// How long to wait for typing to settle before running global resolution.
///
/// Per-document indexing is *not* debounced: `index_source` is cheap and per-buffer, so it runs
/// on every keystroke. Only `Resolver::resolve` — which links declarations across the whole
/// graph — waits. rubydex's resolver is incremental (`Graph::take_pending_work`), so this
/// coalesces bursts rather than repeating whole-graph work.
const RESOLVE_DEBOUNCE: Duration = Duration::from_millis(150);

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
/// reach before the `isIncomplete` flag makes it ask again.
const MAX_COMPLETION_ITEMS: usize = 512;

/// How many subtypes one `typeHierarchy/subtypes` answers with.
///
/// Finding them is free — rubydex maintains the reverse index as it linearizes, so the lookup is
/// the same work for three descendants as for thirty thousand. What costs is the *rows*: each one
/// has to be placed in its own file, and that is a read and a line index per file. Measured on
/// solargraph with Ruby's own signatures in: 0.07 ms median over 899 of the project's own
/// namespaces, 4.3 ms for `StandardError`'s 458 rows, 25.1 ms for `Object`'s 1,978. So the cap
/// bounds the response *and* the only part of this request that scales.
///
/// The number is the measured worst *legitimate* question plus headroom rather than a guess.
/// `StandardError`'s 458 is a real question with a real answer and is what ruled out 512;
/// `Object`, `Kernel` and `BasicObject` come to 1,978, 1,978 and 1,989, and every namespace
/// solargraph defines itself is at most 65. All of those fit. What does not is a Rails bundle,
/// where the three roots are an order of magnitude larger and the answer would be megabytes and
/// a quarter of a second; that is the case this exists for, and reaching it says so out loud.
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

/// A buffer the editor has open, which shadows whatever is on disk.
///
/// We keep our own copy of the text because rubydex's `Document` exposes a `line_index()` but
/// not the source, and incremental sync (M2) and cursor context (M5) both need it.
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
            open: HashMap::new(),
            encoding,
            client,
            workspace,
            outgoing,
            cancellations,
            dirty: false,
            foreign_prefixes: Vec::new(),
            resolve_at: None,
            gem_work: None,
            workspace_files: 0,
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
        // `index.include` is `**/*.rb` by default, so this usually finds nothing to do. It is
        // not usually: a project that keeps its own `sig/` and adds `sig/**/*.rbs` to the
        // include reaches this path and no other, and an `interface` there lands its members on
        // `Object` exactly as one in Ruby's own signatures would.
        let files = self.index_edited_signatures(discovery.files);
        let errors = indexing::index_files(&mut self.graph, files, IndexerBackend::RubyIndexer);
        let indexed = started.elapsed();

        for error in &errors {
            tracing::warn!("indexing error: {error:?}");
        }

        self.resolve();
        tracing::info!(
            "indexed {count} files in {indexed:.2?}, resolved in {:.2?} (total {:.2?})",
            started.elapsed() - indexed,
            started.elapsed()
        );
    }

    /// Main loop with a debounce timer for global resolution.
    fn run(&mut self, receiver: &Receiver<Task>) {
        loop {
            // Gem indexing runs only in the gaps. Checking the queue first is what makes
            // "background" true rather than aspirational: with work waiting, the editor's
            // request goes first and the bundle waits.
            //
            // The `continue` is also why an armed `resolve_at` is not looked at until the
            // background work runs out. An edit made during a cold start is indexed at once —
            // it is a task, and tasks come first — but the resolve its debounce armed waits for
            // the bundle, so the squiggle for what was typed waits with it. That is the same
            // trade `step_gem_indexing` states, "the next request resolves what is there", and
            // an editor asks something after nearly every keystroke; the delay is bounded by
            // the background index, a quarter of a second for a 151-gem bundle. Reversing it —
            // settling an overdue resolve before the next batch — costs one resolve per 150 ms
            // of typing against a mid-index resolve measured at ~100 ms p90, so it is a
            // decision for a measurement on a real bundle rather than a hunch.
            // `threaded_tests::push_diagnostics_for_an_edit_wait_for_the_background_index`
            // holds the current answer, so changing it fails there and nowhere else.
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

                let text = {
                    let document = self.open.get_mut(&uri).expect("inserted above if missing");
                    for change in &changes {
                        document.text.apply(change.range, &change.text);
                    }
                    document.version = version;
                    document.text.text().to_owned()
                };
                self.index_buffer(&uri, &text);
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
        let (problems, load_paths, gem_count, roots, signatures) = {
            let signatures = self.workspace.signatures().clone();
            let discovered = self.workspace.gems();
            (
                discovered.problems.clone(),
                discovered.load_paths(),
                discovered.gems.len(),
                discovered
                    .roots
                    .iter()
                    .chain(discovered.ruby_lib.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
                signatures,
            )
        };

        self.foreign_prefixes = roots
            .iter()
            .chain(signatures.origin.is_some().then_some(&signatures.root))
            .filter_map(|root| DocUri::from_path(root))
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
        for path in &load_paths {
            if files.len() - signature_files >= max_files {
                truncated = true;
                break;
            }
            files.extend(
                gems::ruby_files(std::slice::from_ref(path))
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
            let batch = self.index_edited_signatures(batch);
            let errors = indexing::index_files(&mut self.graph, batch, IndexerBackend::RubyIndexer);
            for error in &errors {
                // Debug, not warn: a bundle of a hundred gems will always contain something
                // that does not parse, and none of it is the user's problem.
                tracing::debug!("gem indexing error: {error:?}");
            }
            // Marked dirty without arming the debounce timer. The next request resolves what is
            // there; scheduling a resolve per batch would re-link the whole graph dozens of
            // times for no one's benefit.
            self.dirty = true;
        }

        if !finished {
            return true;
        }

        let work = self
            .gem_work
            .take()
            .expect("present at the top of this method");
        tracing::info!(
            "indexed {} signature files and {} files from {} gems in {:.2?}",
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
    /// The file has to be read here to know whether it holds an interface at all, so only `.rbs`
    /// paths are looked at, and only the one in five that does hold one leaves the parallel path
    /// — everything else is still a plain path for a worker thread to read.
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
        let Some(edited) = signatures::without_interfaces(&source) else {
            return false;
        };
        // The URI has to be spelled the way `index_files` would have spelled it, or this forks a
        // second document for the same file. `DocUri` is that spelling.
        let Some(uri) = DocUri::from_path(path) else {
            return false;
        };
        indexing::index_source(&mut self.graph, uri.as_str(), &edited, &LanguageId::Rbs);
        true
    }

    fn index_buffer(&mut self, uri: &DocUri, text: &str) {
        let language = uri
            .to_path()
            .map_or(LanguageId::Ruby, |path| LanguageId::from_path(&path));
        // The same rule as on the indexing path, or opening a signature file in the editor puts
        // back the declarations that path took out — and leaves them there, because nothing
        // re-indexes the file once the buffer closes.
        let edited = matches!(language, LanguageId::Rbs)
            .then(|| signatures::without_interfaces(text))
            .flatten();
        let text = edited.as_deref().unwrap_or(text);
        indexing::index_source(&mut self.graph, uri.as_str(), text, &language);
        self.mark_dirty();
    }

    /// Throw the graph away and build it again: the workspace, the open buffers, the gems.
    ///
    /// Shared by `ReloadConfig` — where the configuration decides what belongs in the index, so
    /// nothing computed under the old one can be trusted — and by the recovery in
    /// [`Analysis::resolve`], where the graph is in an unknown state and starting over is the
    /// only honest answer.
    fn rebuild(&mut self) {
        self.graph = Graph::new();
        // Whatever was still queued refers to the old configuration's gem roots, and the graph
        // it was going to be indexed into no longer exists.
        if let Some(work) = self.gem_work.take()
            && let Some(progress) = work.progress
        {
            progress.end("cancelled".to_owned());
        }
        self.foreign_prefixes.clear();
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
    fn refresh(&mut self, uris: Vec<DocUri>) {
        let started = Instant::now();
        let max_files = self.workspace.config().index.max_files;
        let (mut indexed, mut forgotten, mut full) = (0_usize, 0_usize, false);

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
            let known = self.indexed(&uri);
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
        if indexed + forgotten > 0 {
            tracing::debug!(
                "re-indexed {indexed} and dropped {forgotten} watched files in {:.2?}",
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
        self.mark_dirty();
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.resolve_at = Some(Instant::now() + RESOLVE_DEBOUNCE);
    }

    /// Run the debounced work: link the graph, then push whatever diagnostics moved.
    fn settle(&mut self) {
        self.resolve_at = None;
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.resolve();
        self.publish_diagnostics();
    }

    /// Link the graph — and survive rubydex panicking while it does.
    ///
    /// rubydex 0.2.5 panics inside `Resolver::resolve` after a document is deleted:
    /// `Graph::delete_document` invalidates first and untracks the document's strings second, so
    /// the work the invalidation queued can name a string that is no longer there, and
    /// `resolution.rs:748` unwraps it. Reproduced by deleting
    /// `lib/solargraph/yard_map/to_method.rb` from a solargraph v0.58.2 checkout; reachable in
    /// v0.2.0 already, through `didClose` on a file that is gone, and routine from v0.3.0
    /// because a `git checkout` that removes a file is an ordinary Tuesday.
    ///
    /// There is nothing to upgrade to — `=0.2.5` is the only published crate, which is why the
    /// v0.3.0 plan's finding 1 says the alternative is a fork — so it is contained here instead.
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

    /// Run `f` over the text of `uri`: the open buffer if the editor has one, otherwise the file
    /// on disk. `None` when there is no readable text.
    ///
    /// The disk read is what rubydex indexed, unless the file changed underneath us — in which
    /// case a re-index is already on its way and the ranges correct themselves.
    fn with_text<R>(&self, uri: &DocUri, f: impl FnOnce(&TextDocument) -> R) -> Option<R> {
        if let Some(open) = self.open.get(uri) {
            return Some(f(&open.text));
        }
        let text = std::fs::read_to_string(uri.to_path()?).ok()?;
        Some(f(&TextDocument::new(text, self.encoding)))
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

        // Answer against a settled graph: a stale answer is worse than a slightly slower one.
        // This is `dirty`, not `resolve_at`, because background gem indexing deliberately does
        // not arm the timer — but its files are still unlinked until something resolves them.
        //
        // Two requests are exempt, because their answer does not come from the graph at all;
        // waiting for a resolve they never read is a cost paid on every keystroke. Measured on
        // solargraph's 1,058-line `api_map.rb`, asking for folding ranges after each character
        // of a new method as an editor does: 14.3 ms a keystroke while settling, 0.1 ms without.
        if self.dirty && needs_the_graph(&request.method) {
            self.settle();
        }

        let id = request.id.clone();
        let response = match request.method.as_str() {
            "textDocument/documentSymbol" => reply(&id, self.document_symbols(request.params)),
            "textDocument/hover" => reply(&id, self.hover(request.params)),
            "textDocument/definition" => reply(&id, self.goto_definition(request.params)),
            "textDocument/references" => reply(&id, self.references(request.params)),
            "textDocument/documentHighlight" => {
                reply(&id, self.document_highlights(request.params))
            }
            "textDocument/selectionRange" => reply(&id, self.selection_ranges(request.params)),
            "textDocument/foldingRange" => reply(&id, self.folding_ranges(request.params)),
            "workspace/symbol" => reply(&id, self.workspace_symbols(request.params)),
            "textDocument/prepareTypeHierarchy" => {
                reply(&id, self.prepare_type_hierarchy(request.params))
            }
            "typeHierarchy/supertypes" => reply(&id, self.supertypes(request.params)),
            "typeHierarchy/subtypes" => reply(&id, self.subtypes(request.params)),
            "textDocument/signatureHelp" => reply(&id, self.signature_help(request.params)),
            "textDocument/prepareRename" => reply(&id, self.prepare_rename(request.params)),
            "textDocument/rename" => reply(&id, self.rename(request.params)),
            "textDocument/completion" => reply(&id, self.completion(request.params)),
            "completionItem/resolve" => reply(&id, self.resolve_completion(request.params)),
            method => Response::new_err(
                id.clone(),
                ErrorCode::MethodNotFound as i32,
                format!("ya-lsp does not handle {method} yet"),
            ),
        };

        self.cancellations.forget(&id);
        self.respond(response);
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

        self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // More than one target can share the narrowest span; take the first that has
            // something to say rather than the first that exists.
            locator::locate(&self.graph, UriId::from(uri.as_str()), offset)
                .into_iter()
                .find_map(|located| {
                    let resolution = locator::resolve(&self.graph, &located);
                    let markdown = hover::markdown(&self.graph, &resolution)?;
                    Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: markdown,
                        }),
                        range: Some(text.range_at(located.start, located.end)),
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
        let found = self.with_text(&uri, ranges::folds)?;

        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/definition`.
    fn goto_definition(&self, params: serde_json::Value) -> Option<GotoDefinitionResponse> {
        let params: lsp_types::GotoDefinitionParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let (origin, sites) = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);

            for located in locator::locate(&self.graph, UriId::from(uri.as_str()), offset) {
                let sites: Vec<Site> = locator::resolve(&self.graph, &located)
                    .declarations
                    .into_iter()
                    .flat_map(|id| locator::sites(&self.graph, id))
                    .collect();
                if !sites.is_empty() {
                    return Some((text.range_at(located.start, located.end), sites));
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
        let items = hierarchy::supertypes(&self.graph, declaration, &self.own_documents());
        self.hierarchy_items(items)
    }

    /// `typeHierarchy/subtypes`.
    fn subtypes(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchySubtypesParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let found = hierarchy::subtypes(
            &self.graph,
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
            rename::plan(&self.graph, uri.as_str(), text.text(), offset, &own)
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
    /// The list is always `isIncomplete`: it was filtered against the prefix the cursor had when
    /// it was asked for, so it stops being the right answer the moment that prefix changes. See
    /// `analysis::completion` for what is exact here and what is a guess.
    fn completion(&self, params: serde_json::Value) -> Option<CompletionResponse> {
        let params: lsp_types::CompletionParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::from_lsp(&params.text_document_position.text_document.uri)?;
        let started = Instant::now();

        let (completion, range) = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            let completion = completion::complete(
                &self.graph,
                UriId::from(uri.as_str()),
                text.text(),
                offset,
                MAX_COMPLETION_ITEMS,
                &self.own_documents(),
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
                    .map(|id| serde_json::Value::String(id.get().to_string())),
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

        let markdown = declaration_in(item.data.as_ref()).and_then(|declaration| {
            hover::markdown(
                &self.graph,
                &locator::Resolution {
                    declarations: vec![declaration],
                    precise: true,
                    redirected: false,
                },
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
        let (target_range, target_selection_range) = self.with_text(&target, |text| {
            (
                text.range_at(site.full.0, site.full.1),
                text.range_at(site.selection.0, site.selection.1),
            )
        })?;
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
/// `foldingRange` and `selectionRange` are pure functions of one buffer — `analysis::ranges`
/// never sees a `Graph` — so they are the two that do not.
fn needs_the_graph(method: &str) -> bool {
    !matches!(
        method,
        "textDocument/foldingRange" | "textDocument/selectionRange"
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
        fn has(&self, name: &str) -> bool {
            self.analysis.graph.get(name).is_some()
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
        // This is M0's done-criterion: re-indexing the same URI must not accumulate.
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
    // M1 — diagnostics
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
        // Guards M3: nobody can fix a warning inside somebody else's gem, and a Rails app would
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
    // M2 — navigation
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
        // under an empty name, and `render` used to prepend a second one — `**` came out as
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

    fn card(harness: &mut Harness, uri: &DocUri, source: &str, needle: &str) -> String {
        harness.hover_at(uri, source, needle)["contents"]["value"]
            .as_str()
            .unwrap_or_else(|| panic!("no hover on {needle:?}"))
            .to_owned()
    }

    #[test]
    fn a_core_method_hovers_as_rdoc_written_in_markdown() {
        // The whole card, not a `contains`. `ANCESTRY` pins the first ten completions because a
        // ranking is either plausible or it is not, and a hover card is the same kind of
        // object: composition, not a feature. Every part of `hover::card` was asserted by some
        // `contains` somewhere and the card itself nowhere, which is how the two shapes of one
        // answer — a guessed single match and a guessed list — drifted apart.
        //
        // What this pins on the way past: `<code>self</code>` reaching the user as markdown
        // rather than as a span a client silently eats, `[Case Mapping](rdoc-ref:…)` losing a
        // link that goes nowhere while keeping its words, the call-seq lifted out of RDoc's
        // HTML header as Ruby, and the indented example surviving both untouched.
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
             See Case Mapping.\n\
             \n\
             *Matched on the method name alone — the receiver's type is unknown.*"
        );
    }

    #[test]
    fn a_stdlib_method_hovers_the_same_way_a_core_one_does() {
        // Different directory under the rbs root, same card. `<tt>` is RDoc's other spelling of
        // `<code>` and appears 22 times in the vendored signatures; it must not be the one that
        // still leaks.
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
             Parses `argv` in place and returns what is left of it.\n\
             \n\
             *Matched on the method name alone — the receiver's type is unknown.*"
        );
    }

    #[test]
    fn every_shape_of_card_puts_what_it_knows_in_the_same_place() {
        // The four cards side by side, which is the only way the convention is visible: answer
        // first, then one italic line per thing ya-lsp knows *about* the answer. A precise hit
        // says nothing extra; a reopened class says where else it lives; a guess says it is a
        // guess; and a guess with more than one candidate says the same sentence in the same
        // place rather than in bold at the top after an em dash, which is what it used to do.
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
            search::search(&harness.analysis.graph, "Person", 1, &own).len() == 1,
            "the fixture has to match at all for a zero limit to mean anything"
        );
        assert!(search::search(&harness.analysis.graph, "Person", 0, &own).is_empty());
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
    // M3 — gem indexing
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
        // The milestone in one test: a constant defined only in an installed gem, found with no
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
        // Finding F, through the wire it actually travels. `ruby_lib_dirs` refuses to guess a
        // Ruby — rightly, because guessing put Apple's vestigial 2.6 stdlib into the graph and
        // answered `"hello".u` with `unspace` — and until v0.2.0 it refused in silence, at a
        // cost of every one of Ruby's own 727 library files. The unit test in `workspace::gems`
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
    // M4 — workspace symbols and references
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
    // M5 — completion
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
    /// it lands, which is the question v0.1.0 never asked anywhere — `"hello".` shipped opening
    /// on `DelegateClass, Digest, append_as_bytes, …` through a green suite and a benchmark that
    /// only ever measured milliseconds.
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
        // missed the first time. `index.include` is `**/*.rb` by default, so a project that keeps
        // its own `sig/` has to widen it — and then the files arrive through `index_workspace`
        // rather than through the background signature index. Filtered one way and not the other,
        // `Object#slurp` came back and was offered on every receiver in the project.
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
            "[gems]\nenabled = false\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n\
             [index]\ninclude = [\"**/*.rb\", \"sig/**/*.rbs\"]\n",
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
    fn every_list_is_incomplete() {
        // It was filtered against one prefix, so it stops being the right answer the moment the
        // prefix changes. Without the flag the client narrows a stale list instead of asking.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(harness.complete(&uri, "HR::~\n")["isIncomplete"], true);
        assert_eq!(harness.complete(&uri, "sh~\n")["isIncomplete"], true);
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

        let resolved = harness.ask(
            "completionItem/resolve",
            serde_json::json!({ "label": "shout", "data": "1234567890123456789" }),
        );

        assert_eq!(resolved["label"], "shout", "the item comes back regardless");
        assert!(
            resolved["documentation"].is_null(),
            "and carries nothing invented: {resolved}"
        );
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
    // v0.3.0 — signature help
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
        // A cursor inside a nested call's parentheses belongs to the inner call — the case
        // ruby-lsp carries an `adjust_for_nested_target` for. Here it costs nothing: the walk
        // that finds the enclosing argument list is pre-order, so the innermost claimant is
        // the last to write itself down.
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
            // expression, which §2's finding 2 puts outside this release.
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
    // v0.3.0 — document highlight
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
    // v0.3.0 — type hierarchy
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
        // The plan's "a superclass in a gem" criterion, met with the mechanism that actually
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
    // v0.3.0 — rename
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
}
