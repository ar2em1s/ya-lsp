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
pub mod hover;
pub mod locator;
pub mod position;
pub mod progress;
pub mod references;
pub mod render;
pub mod requires;
pub mod search;
pub mod symbols;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_server::{ErrorCode, Message, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CompletionItem, CompletionItemKind, CompletionItemTag, CompletionList,
    CompletionResponse, CompletionTextEdit, DiagnosticSeverity, DocumentSymbolResponse,
    Documentation, GotoDefinitionResponse, Hover, HoverContents, Location, LocationLink,
    MarkupContent, MarkupKind, SymbolInformation, TextEdit, WorkspaceSymbolResponse,
};
use rubydex::{
    indexing::{self, IndexerBackend, LanguageId},
    model::{
        graph::Graph,
        ids::{DeclarationId, UriId},
    },
    resolution::Resolver,
};

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
    /// `ya-lsp.toml` changed on disk.
    ReloadConfig,
    /// The client changed the settings it sent as `initializationOptions`.
    ///
    /// Carried rather than re-read, because these never touch the filesystem: they are the
    /// editor's own settings, and the editor is the only thing that knows them.
    ChangeConfig {
        options: Option<serde_json::Value>,
    },
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
    fn take(&self, id: &RequestId) -> bool {
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
        let errors = indexing::index_files(
            &mut self.graph,
            discovery.files,
            IndexerBackend::RubyIndexer,
        );
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
                self.graph = Graph::new();
                // Whatever was still queued refers to the old configuration's gem roots, and
                // the graph it was going to be indexed into no longer exists.
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
                // Unconditionally, even with no buffers to replay: the reload may have changed
                // which rules are on, or dropped files from the index, and both change what the
                // editor should be showing. Without this a project with no open files keeps
                // displaying the diagnostics from the previous config forever.
                self.mark_dirty();
                self.queue_background_indexing();
            }
            Task::Request(request) => self.serve(request),
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
            let message = format!(
                "stopped indexing gems at [gems].max_files ({max_files}); gem intelligence is \
                 incomplete."
            );
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

    fn index_buffer(&mut self, uri: &DocUri, text: &str) {
        let language = uri
            .to_path()
            .map_or(LanguageId::Ruby, |path| LanguageId::from_path(&path));
        indexing::index_source(&mut self.graph, uri.as_str(), text, &language);
        self.mark_dirty();
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

    fn resolve(&mut self) {
        let started = Instant::now();
        Resolver::new(&mut self.graph).resolve();
        tracing::debug!("resolved in {:.2?}", started.elapsed());
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
        let Ok(params) = serde_json::to_value(params) else {
            return;
        };
        let _ = self
            .outgoing
            .send(Message::Notification(lsp_server::Notification {
                method: "textDocument/publishDiagnostics".to_owned(),
                params,
            }));
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
            let message = format!(
                "unknown diagnostic rule `{name}` in [diagnostics.rules]; it will be ignored. \
                 Known rules: {known}"
            );
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
        if self.dirty {
            self.settle();
        }

        let id = request.id.clone();
        let response = match request.method.as_str() {
            "textDocument/documentSymbol" => reply(&id, self.document_symbols(request.params)),
            "textDocument/hover" => reply(&id, self.hover(request.params)),
            "textDocument/definition" => reply(&id, self.goto_definition(request.params)),
            "textDocument/references" => reply(&id, self.references(request.params)),
            "workspace/symbol" => reply(&id, self.workspace_symbols(request.params)),
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
            let message = format!(
                "{} references found; showing the first {MAX_REFERENCES}. The list is \
                 incomplete.",
                found.len()
            );
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

        let markdown = item
            .data
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|raw| *raw != 0)
            .and_then(|raw| {
                hover::markdown(
                    &self.graph,
                    &locator::Resolution {
                        declarations: vec![DeclarationId::new(raw)],
                        precise: true,
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
        let Ok(params) = serde_json::to_value(params) else {
            return;
        };
        let _ = self
            .outgoing
            .send(Message::Notification(lsp_server::Notification {
                method: "window/showMessage".to_owned(),
                params,
            }));
    }
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
        // An open buffer shadows disk and is already indexed; never cached, because the copy
        // would go stale the moment the user types.
        if let Some(open) = self.analysis.open.get(uri) {
            return Some(open.text.range_at(start, end));
        }
        let encoding = self.analysis.encoding;
        self.read
            .entry(uri.clone())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(uri.to_path()?).ok()?;
                Some(TextDocument::new(text, encoding))
            })
            .as_ref()
            .map(|text| text.range_at(start, end))
    }
}

/// Wrap a handler's answer as a successful response.
///
/// `None` becomes JSON `null`, which is how LSP spells "there is nothing here". An error
/// response would be wrong: editors surface those to the user, and "no definition found" is
/// not something to complain about.
/// ya-lsp's own suggestion kinds, in LSP's vocabulary.
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
            let offset = marked.find('~').expect("a ~ marking the cursor");
            let line = marked[..offset].matches('\n').count();
            let character = offset - marked[..offset].rfind('\n').map_or(0, |index| index + 1);
            self.open(uri, &marked.replace('~', ""));
            self.ask(
                "textDocument/completion",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": line, "character": character },
                }),
            )
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

        let told = harness
            .messages()
            .into_iter()
            .any(|message| message.contains("incomplete"));
        assert!(told, "a truncated answer has to say so");
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
}
