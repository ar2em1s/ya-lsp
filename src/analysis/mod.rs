//! The analysis thread: sole owner of the rubydex `Graph`.
//!
//! # Containment rule
//!
//! This is the only module allowed to name a rubydex type. rubydex is pre-1.0 and its API churns,
//! so the blast radius must be one directory.
//!
//! # Threading
//!
//! rubydex is synchronous and `Graph` has a single `&mut` writer, so there is exactly one analysis
//! thread and every request queues behind it. rubydex is fast enough for that, but it makes
//! debouncing and cancellation essential.
//!
//! # What is here and what is next door
//!
//! - **This file is the thread**: the tasks it takes, the graph and buffers it owns, the indexing
//!   lifecycle, the diagnostics it pushes, and the run loop ordering all of it.
//! - **[`requests`]** is the LSP request layer: every handler, the method table, and the bulkhead
//!   around answering.
//! - **[`synthesize`]** is the pass that writes the workspace's own RBS.
//!
//! Both siblings hold an `impl Analysis`, which Rust allows because privacy is by module
//! *descendant*. For the same reason `testing` lives here: the `Harness` every end-to-end test
//! drives holds an `Analysis`, and a child module sees its private fields. The tests below are the
//! thread's own (buffer against disk, the skip list, the gem stepper, what gets published, whose
//! code it is); the rest live with the invariant they would break.

pub mod annotations;
pub mod code_actions;
pub mod completion;
pub mod coverage;
pub mod cursor;
pub mod diagnostics;
mod environment;
// The two lists a project may replace, and the only things this private module publishes.
//
// They are *defaults a user is shown*: the VS Code manifest documents both, and a default copied
// into JSON with nothing tying it to the code rots. `tests/vscode_manifest.rs` reads them from here
// so the two cannot drift.
pub use environment::{MIGRATION_PAIR, TEST_TREES};
pub mod erb;
pub mod hierarchy;
pub mod highlight;
pub mod hints;
pub mod hover;
mod indexed;
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
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_server::{Message, Request, RequestId, Response};
use lsp_types::{ClientCapabilities, DiagnosticSeverity};
use rubydex::{indexing::LanguageId, model::ids::UriId, resolution::Resolver};
use xxhash_rust::xxh3::xxh3_64;

use crate::messages;
use crate::workspace::{DocUri, Workspace, gems, rails};
use position::{PositionEncoding, Rebase, TextDocument};
use progress::Progress;
use synthesized::Synthesized;

/// How long to wait for typing to settle before the graph catches up.
///
/// What waits is all of it: the buffer's index, the generator pass, `Resolver::resolve` (which
/// links declarations across the whole graph), and the diagnostics push. A keystroke leaves its
/// edit in `pending_index` and arms this timer; `Analysis::settle` is the only place any of that
/// runs. rubydex's resolver is incremental (`Graph::take_pending_work`), so a settle coalesces a
/// burst.
///
/// - **Why 500 ms.** Every edit renews the timer, so it fires only when the typist pauses longer
///   than it, and a settle firing mid-burst costs `debounce + settle - gap`. That is zero either
///   when the settle fits inside the gap, or when the debounce exceeds the gap. A short debounce
///   stops working once indexing is deferred, because a mid-burst settle is then the *only* thing a
///   completion waits for. Measured at a 250 ms typing pace, 500 ms keeps keystrokes cheap on every
///   corpus where 150 ms does not.
/// - **A debounce equal to the typing pace is worse than either**, because the settle then fires
///   exactly between two keystrokes.
/// - **Scaling it per workspace was tried and abandoned.** No stable signal picks the value: a
///   settle costs what the *last edit* made it cost.
///
/// The trade: **diagnostics appear about 350 ms later after typing stops**, in exchange for much
/// faster completion while typing.
const RESOLVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// [`RESOLVE_DEBOUNCE`], with the override the benchmarks set.
///
/// Not a `ya-lsp.toml` key: the value is calibrated by measurement, not preference, and a wrong
/// project setting would slow every completion without changing any answer.
fn resolve_debounce() -> Duration {
    std::env::var("YA_LSP_RESOLVE_DEBOUNCE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(RESOLVE_DEBOUNCE, Duration::from_millis)
}

/// How many gem files one background step indexes before handing the thread back.
///
/// The cost of a step is not the indexing (milliseconds) but the *resolve* the next request must
/// run over what the step added, which is what the user waits for. Measured on a real Rails bundle,
/// worst-case request latency during indexing grows with step size, while total indexing time for
/// an idle server is the same either way. 100 keeps the worst case interactive and costs the idle
/// path nothing.
const GEM_FILES_PER_STEP: usize = 100;

/// How many symbols one `workspace/symbol` answers with.
///
/// Required at gem scale: rubydex's fuzzy match is a subsequence test, so a two-letter query
/// matches much of a Rails bundle, on every keystroke. The number only sets how far down the
/// ranking a self-filtering client can reach (VS Code shows a few dozen rows), so raising it costs
/// every keystroke and buys nothing visible.
const MAX_WORKSPACE_SYMBOLS: usize = 256;

/// How many references one `textDocument/references` answers with.
///
/// - **Why:** a name-based match on `call` in a huge workspace must not send the editor a
///   multi-megabyte response.
/// - **Reaching it is logged and shown.** A silently truncated "find all references" is a wrong
///   answer that looks right. Places are ordered by file before the cap, so what is dropped is
///   whole directories, and the message names the file the list stops in.
/// - **It is reached in practice**, on name matches for common names in large workspaces.
const MAX_REFERENCES: usize = 10_000;

/// How many suggestions one `textDocument/completion` answers with.
///
/// Not a latency control: measured, the size barely moves request time, because the cost is the
/// graph work, not the rows. It bounds the *response* (a thousand items is around 200 KB of JSON
/// per keystroke) and how deep a self-filtering client can reach before `isIncomplete` makes it ask
/// again. Reaching it is the only thing that sets that flag.
const MAX_COMPLETION_ITEMS: usize = 512;

/// How many candidates the **name-based** list may have before it answers with nothing.
///
/// The admission ceiling, separate from the other two:
///
/// - [`MAX_COMPLETION_ITEMS`] bounds the *response* of a list this server believes in.
/// - [`MAX_UNTYPED_COMPLETION_ITEMS`] bounds how many rows of a **guess** are worth reading.
/// - **This** decides whether the guess is worth making. `completion::by_name` produces every
///   matching method name in the project, attached to no class, and above this many there is no
///   reason to believe the ranking put the right one near the top.
///
/// **Measured by `make audit-prefix`:** following untyped cursors outwards through their own word,
/// the word sits within the first [`MAX_UNTYPED_COMPLETION_ITEMS`] rows every time up to 512
/// candidates, and starts slipping above it:
///
/// | candidates | lists | word in the first 128 |
/// |---|---|---|
/// | 1–128 | 159 | 159 |
/// | 129–256 | 131 | 131 |
/// | 257–512 | 140 | 140 |
/// | 513–1,024 | 157 | 152 |
/// | 1,025+ | 156 | 141 |
///
/// The empty prefix is untouched: its candidate set is the whole project's name universe, tens of
/// thousands, so it declines.
const MAX_UNTYPED_CANDIDATES: usize = 512;

/// How many rows of the **name-based** list are worth reading, once it is worth offering.
///
/// - **The display ceiling.** [`MAX_UNTYPED_CANDIDATES`] decides whether to answer; this decides
///   how much to send. They are split because the ranking is good and the row count is not: a
///   512-candidate guess holds the word in its first 128 rows, and nobody reads 512 rows.
/// - **Usually nothing is offered for the first few keystrokes.** Most untyped cursors have too
///   many candidates at two or three characters, so the bounded list mostly exists mid-word.
/// - **Truncating is honest here but not at an empty prefix.** With nothing typed, `tier` is 1 and
///   `length` 0 for every row, leaving only `Locality` (which directory the name is in), so the
///   kept rows would be arbitrary. From three characters, `tier` and `length` both rank.
const MAX_UNTYPED_COMPLETION_ITEMS: usize = 128;

/// How many subtypes one `typeHierarchy/subtypes` answers with.
///
/// - **Finding them is free**: rubydex keeps the reverse index as it linearizes. What costs is the
///   *rows*, each placed in its own file (a read and a line index per file). So the cap bounds the
///   response and the only part that scales.
/// - **Sized to the worst legitimate question plus headroom.** `StandardError`'s descendants in
///   Ruby's signatures ruled out 512; `Object`, `Kernel` and `BasicObject` come to about 2,000 each
///   outside a bundle. A Rails bundle's roots are an order of magnitude larger, megabytes and a
///   quarter-second: the case this exists for, and reaching it says so.
const MAX_SUBTYPES: usize = 2048;

/// How many callers one `callHierarchy/incomingCalls` answers with.
///
/// - **Rows are buckets, not call sites**: one per calling method, however often it calls. Drawing
///   one reads its file, `MAX_SUBTYPES`' cost exactly.
/// - **Easier to reach than a wide type hierarchy.** That takes a deliberate click near the object
///   model's root; a wide call hierarchy takes a method named `call`.
/// - **Sized like `MAX_SUBTYPES`**: the widest real answers in the corpora (callers of `id`) fit
///   well inside. What does not fit is a workspace several times larger, where the response, not
///   latency, fails.
const MAX_INCOMING_CALLS: usize = 2048;

/// The `$/progress` token for the gem index. Fixed, since only one runs at a time.
const GEM_PROGRESS_TOKEN: &str = "ya-lsp/index-gems";

/// The `$/progress` token for the pipeline's last stage. Fixed for [`GEM_PROGRESS_TOKEN`]'s reason,
/// and *different* because the two streams are adjacent: one ends as the other begins, and a shared
/// token could render as one stream that never closed.
const GENERATE_PROGRESS_TOKEN: &str = "ya-lsp/generate";

/// The one command this server registers; all it does is open a document.
///
/// Namespaced on the server, because `workspace/executeCommand` is a flat namespace shared by every
/// language server in the session. Advertised in `capabilities::advertised` and answered in
/// `requests`. A client may also run it by name, which is what the code-action entry point gives a
/// client with no lightbulb.
///
/// A **prefix**, not the full name: see [`show_generated_command`].
pub const SHOW_GENERATED: &str = "ya-lsp.showGenerated";

/// The command name one server advertises, carrying the workspace root it was started for.
///
/// **Two servers in one window must not advertise one name.** The VS Code extension starts a client
/// per workspace folder, and `vscode-languageclient` registers every `executeCommandProvider` name
/// with `vscode.commands.registerCommand`, which *throws* on a taken name and kills the second
/// client's handshake. A multi-root workspace would lose every feature in its second folder.
/// Nothing else in the protocol has this shape: other capabilities are scoped to a document
/// selector, while a command id is scoped to the editor.
///
/// So the root goes in the name, hashed: the id is never shown (readers see the action's title),
/// and a path would be long and leak someone's disk layout to any extension enumerating commands.
#[must_use]
pub fn show_generated_command(root: &std::path::Path) -> String {
    format!(
        "{SHOW_GENERATED}.{:016x}",
        xxhash_rust::xxh3::xxh3_64(root.as_os_str().as_encoded_bytes())
    )
}

/// Which watcher saw a change on disk. It decides exactly one thing.
///
/// **A saved buffer yields to the disk only for the server's own watcher.** A client holding the
/// `didChangeWatchedFiles` registration reloads an unmodified file itself (VS Code always, Neovim
/// with `autoread`), then sends a `didChange` whose *ranges are measured against the text it had*.
/// If the server had already swapped in the disk's text, the ranges would land in the wrong string.
/// So the swap happens only where the server is the only watcher, which means a client that reloads
/// nothing itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watched {
    /// The client's own watcher, over `workspace/didChangeWatchedFiles`.
    ByTheClient,
    /// [`crate::server::watcher`], which runs only where the client has none.
    ByTheServer,
}

/// Work sent from the main loop to the analysis thread.
#[derive(Debug)]
pub enum Task {
    DidOpen {
        uri: DocUri,
        text: String,
        /// The editor's version of this buffer, echoed on `publishDiagnostics` so the client can
        /// drop results for text it has typed past.
        version: Option<i32>,
    },
    DidChange {
        uri: DocUri,
        /// In the client's order. Each range is measured against the text the previous change left,
        /// so they cannot be reordered or merged.
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
    /// Paths a `workspace/didChangeWatchedFiles` named, deduplicated, with `ya-lsp.toml` split off
    /// (that is [`Task::ReloadConfig`], far more work).
    ///
    /// No change *kind* is carried. Created, changed and deleted are all answered by looking: a
    /// file that exists is indexed, one that does not is dropped. During a branch switch the events
    /// and the filesystem genuinely disagree, and a `Deleted` for a path git already wrote back
    /// would otherwise drop an existing file.
    WatchedFiles {
        uris: Vec<DocUri>,
        watched: Watched,
    },
    /// `ya-lsp.toml` changed on disk.
    ReloadConfig,
    /// The client changed the settings it sent as `initializationOptions`.
    ///
    /// Carried, not re-read: these are the editor's own settings and never touch the filesystem.
    ChangeConfig {
        options: Option<serde_json::Value>,
    },
    /// Kill the analysis thread, from a test.
    ///
    /// A stand-in, like [`crash_the_next_resolve_if_asked`]: the panic [`AnalysisHandle::join`]
    /// reports is by construction the *unforeseen* one (everything foreseen is handled or caught in
    /// [`Analysis::resolve`]), so no input provokes it. What this pins is ya-lsp's half: a dead
    /// thread is noticed, not joined silently. That is the difference between one log line and a
    /// server that silently answers nothing for the rest of the session.
    #[cfg(test)]
    Panic,
}

impl Task {
    /// What one log line calls this task, and the one thing it is about.
    ///
    /// Names are the client's own (`didChange`, not `DidChange`), because the reader has the
    /// editor's LSP trace open beside the log and is matching them up.
    fn describe(&self) -> (&'static str, &str) {
        match self {
            Task::DidOpen { uri, .. } => ("didOpen", uri.as_str()),
            Task::DidChange { uri, .. } => ("didChange", uri.as_str()),
            Task::DidClose { uri } => ("didClose", uri.as_str()),
            Task::DidSave { uri } => ("didSave", uri.as_str()),
            // The count goes on the handling arm, not here: a branch switch names thousands of
            // paths, and a number is not a `&str` without allocating per notification.
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
/// A function, not the table it reads. The table is `server::capabilities`, which already reads
/// `analysis::tokens` and `analysis::position`, so naming its types here would create a cycle. This
/// side only needs the trade: prefixes in, registrations out. An empty answer for a non-empty
/// prefix list means the client takes no dynamic registration, which
/// [`Analysis::register_documents`] says once.
pub type DocumentRegistrar = Box<dyn Fn(&[String]) -> Vec<lsp_types::Registration> + Send>;

/// What a client that takes no dynamic registration gets: no registrations, behaviour unchanged.
#[must_use]
pub fn no_document_registrar() -> DocumentRegistrar {
    Box::new(|_| Vec::new())
}

/// The parts of the client's capabilities that change what the server may send back.
///
/// All default to `false` (the pre-3.10 shapes), because a client that does not advertise a
/// capability may fail to parse the richer response, not merely ignore it.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientSupport {
    /// `textDocument/documentSymbol` may answer with a nested tree instead of a flat list.
    pub hierarchical_symbols: bool,
    /// `textDocument/definition` may answer with `LocationLink`s, which carry the origin span and
    /// let the editor preview the target's name apart from its body.
    pub definition_links: bool,
    /// The same for `textDocument/implementation`, as a **separate flag**.
    ///
    /// The protocol gives each of the four gotos its own `linkSupport`, and clients differ: Claude
    /// Code declares `definition.linkSupport: true` and nothing for `implementation`, so it expects
    /// `Location[]` there. Answering with links because it takes them elsewhere reads a capability
    /// it did not send.
    pub implementation_links: bool,
    /// The same for `textDocument/typeDefinition`, a **third** flag for the same reason.
    ///
    /// Each goto is negotiated separately, and a client taking links for one may take none for
    /// another. Reading a neighbouring capability is how a response arrives in a shape the client
    /// cannot parse. `requests::goto_response` takes the right flag as an argument.
    pub type_definition_links: bool,
    /// And the **fourth**, for `textDocument/declaration`.
    ///
    /// Four gotos, four capabilities, four fields; the protocol never says they agree. Neovim
    /// advertises `linkSupport` for this and the two above; Claude Code for `definition` alone, and
    /// never sends this request.
    pub declaration_links: bool,
    /// `window/workDoneProgress`: the client will render a progress stream if one is opened.
    /// Without it, gem indexing is silent.
    pub work_done_progress: bool,
    /// A `WorkspaceEdit` may be sent as `documentChanges` instead of the older `changes` map. The
    /// richer shape carries each file's version, so a client can reject a rename the user has typed
    /// past instead of applying it to moved text.
    pub versioned_edits: bool,
    /// `workspace/inlayHint/refresh`: the client will re-ask for inlay hints when told to.
    ///
    /// The only capability here about an answer going *stale*. A client re-asks for hints on
    /// document change and on scroll, and neither happens while a user waits for a cold index, so
    /// the open file would keep its pre-types margin until touched. Where the client says no,
    /// nothing is lost: hints are right from the next keystroke.
    pub hint_refresh: bool,
    /// `window/showDocument`: the client will open a document the server names.
    ///
    /// The only way a server shows a document without an edit, and the one entry point a
    /// *generated* document can have: nothing may hand out its URI as a `Location`, so the code
    /// action that opens it is a command, and the command answers with this request.
    pub show_document: bool,
    /// `workspace/textDocumentContent`: the client will ask the server for a document's text.
    ///
    /// - **Read from the raw capabilities**, because `lsp-types` 0.97 has no field for it (the same
    ///   gap `server::capabilities::Advertised` works around on the way out).
    /// - **It gates the code action, not just the answer.** `window/showDocument` and this are
    ///   separate, and a client can have the first without the second. Neovim does: its
    ///   `window/showDocument` opens a buffer named after the URI and `bufload`s it from a disk
    ///   with no such file, and nothing in its client implements this request. So a client that
    ///   cannot read the document is never offered it, and when it can, the action appears with no
    ///   change here.
    pub generated_content: bool,
}

impl ClientSupport {
    /// What the client said it can take, read from one object in two shapes.
    ///
    /// `raw` is the same `capabilities` value as it arrived, for the one capability `lsp-types`
    /// 0.97 cannot spell. Passing both rather than re-parsing keeps one source: named fields read
    /// the typed struct, and the unnamed one reads the JSON.
    #[must_use]
    pub fn negotiate(capabilities: &ClientCapabilities, raw: &serde_json::Value) -> Self {
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
            implementation_links: text_document
                .and_then(|it| it.implementation.as_ref())
                .and_then(|it| it.link_support)
                .unwrap_or(false),
            type_definition_links: text_document
                .and_then(|it| it.type_definition.as_ref())
                .and_then(|it| it.link_support)
                .unwrap_or(false),
            declaration_links: text_document
                .and_then(|it| it.declaration.as_ref())
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
            show_document: capabilities
                .window
                .as_ref()
                .and_then(|it| it.show_document.as_ref())
                .is_some_and(|it| it.support),
            // The object's presence, not a field inside it: the client capability's only member is
            // `dynamicRegistration`, which says how a provider may be *registered*, not whether the
            // request is answered. A client sending the object has the feature;
            // vscode-languageclient sends exactly `{"dynamicRegistration": true}`.
            generated_content: raw
                .get("workspace")
                .and_then(|workspace| workspace.get("textDocumentContent"))
                .is_some_and(|content| !content.is_null()),
        }
    }
}

/// Request ids the client has cancelled.
///
/// Owned by the main thread, read by the analysis thread. Shared state, not a channel message:
/// tasks run in order, so a `$/cancelRequest` in the same queue would always arrive *after* its
/// request was answered.
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
        // A panic in another thread must not take the server down; the set is plain data and safe
        // to keep using.
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

/// The analysis thread's stack.
///
/// - **Rust's default is 2 MiB.** Real gem RBS (activerecord's, actionpack's, activesupport's) lets
///   a receiver chain reach the depth `types.rs`' bounds allow (`BODY_HOPS`, `MAX_BRANCHES`,
///   `MAX_WIDTH`), and installing a `.gem_rbs_collection` overflowed the default stack on the first
///   `textDocument/inlayHint` that reached those gems.
/// - **64 MiB is headroom.** `types`' test of a body chain at `BODY_HOPS` runs on a thread this
///   size in a debug build, whose frames are larger than a release's, and needs under 4 MiB.
pub(crate) const ANALYSIS_STACK: usize = 64 * 1024 * 1024;

/// Start the analysis thread.
///
/// `outgoing` is a clone of the LSP connection's sender: the analysis thread writes responses and
/// notifications straight to the transport, so the main loop never polls two channels.
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
        .stack_size(ANALYSIS_STACK)
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
            // The startup index is already resolved, so publish now instead of after the first
            // edit: a project with a syntax error should light up before anyone types.
            analysis.publish_diagnostics();
            // Queued, not indexed: gems go in during the run loop's idle time, so the workspace
            // answers questions while the bundle arrives.
            analysis.queue_background_indexing();
            analysis.run(&receiver);
        })
        .expect("failed to spawn analysis thread");

    AnalysisHandle { sender, thread }
}

// The real trigger for the recovery below is in rubydex and needs a couple of hundred files of a
// real project (solargraph v0.58.2 minus its own `lib/solargraph/yard_map/to_method.rb`), which is
// not worth vendoring for one test. So this pins ya-lsp's half, the half ya-lsp can get wrong: the
// panic is caught, the user is told, the graph comes back, and a rebuild that crashes again stops
// instead of recurring.
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

// The same shape for the request seam, also a stand-in.
//
// The Ruby that provoked it (a class reopened under a constant aliasing it, with the cursor on the
// `def`) was fixed upstream in `ab88ef1`, and
// `a_constant_alias_reopened_under_its_alias_answers_rather_than_crashing` still pins that. What
// has no input now is the *seam*, and the seam must keep working: `create_declaration`'s two
// unwraps are still on upstream's `main`, and every handler below `dispatch` reaches the graph. The
// hazard remains; only a reproduction is missing.
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
/// Kept as our own copy because rubydex's `Document` exposes a `line_index()` but not the source,
/// and incremental sync and cursor context both need it.
#[derive(Debug)]
struct OpenDocument {
    text: TextDocument,
    version: Option<i32>,
    /// Whether the last thing the client said about this document was `didOpen` or `didSave`.
    ///
    /// **The condition for letting the disk win.** A buffer changed since its last save holds text
    /// that exists nowhere else, and only the editor can resolve that. A saved one is a copy of the
    /// disk, which is exactly what an agent editing through a shell leaves stale. Claude Code sends
    /// `didSave` right after every `didChange`, so its buffers are always saved.
    saved: bool,
    /// The text the client last measured its changes against, when the server swapped the buffer
    /// for the disk's.
    ///
    /// **Almost always `None`.** Filled by the swap and emptied by the next `didChange`, which is
    /// applied to *it*, not to the buffer: that change's ranges were computed against the client's
    /// text, and applying them to the disk's text would corrupt it. Set only when `None`, so a
    /// second swap cannot overwrite the client's copy with the first swap's result.
    client_copy: Option<TextDocument>,
}

/// Gem files waiting to be indexed in the background, plus what the editor is being told.
#[derive(Debug)]
struct GemIndexing {
    /// Reversed, so taking the next batch off the end is a pointer move, not a shift.
    remaining: Vec<PathBuf>,
    total: usize,
    gems: usize,
    /// How many of `total` are RBS signatures rather than gem sources. Reported separately because
    /// "indexed 41,000 files from 151 gems" and "and Ruby's own core" are different claims, and the
    /// second breaks silently.
    signature_files: usize,
    progress: Option<Progress>,
    started: Instant,
}

/// Where the cold start has got to, and the only thing that says so.
///
/// **One stage at a time, each naming the next when it finishes.** Nothing else writes this field,
/// and no stage does another's work: the workspace index does not generate, the bundle does not
/// resolve for anyone, and the generator pass runs once, when nothing is left to index.
///
/// - **Why the pass runs only at the end.** Run before the bundle is in, it sees a fraction of the
///   concerns and `place_generated_members` finds none of the bundle's definitions. The later full
///   run then spends most of its time deleting what the early run wrote
///   (`rubydex::Declaration::remove_definition` is a linear scan plus `shrink_to_fit`, and the pass
///   re-records every generated document). So an early run is pure cost.
/// - **Why [`Analysis::settle`] regenerates only at [`Stage::Ready`].** `serve` settles a dirty
///   graph before dispatching, so a request arriving mid-bundle would otherwise trigger the pass,
///   possibly several times during one cold start.
enum Stage {
    /// The workspace's own files are indexed and linked; the bundle has not been queued.
    ///
    /// The entry state, which the run loop never steps: [`Analysis::index_workspace`] is called
    /// eagerly (by `spawn` before `run`, by [`Analysis::rebuild`], by the test harness), because a
    /// client whose `didOpen` is already queued would otherwise be answered against an empty graph.
    Workspace,
    /// The bundle is queued and goes in one batch per idle turn of the run loop.
    Bundle(GemIndexing),
    /// Everything is indexed and nothing has been generated yet.
    Generate,
    /// Every stage has run. Every change from here is the settle's job.
    Ready,
}

impl Stage {
    /// Whether the pipeline has finished: **"the graph is all there will be"**, which a request
    /// must know before it treats its own answer as a shortfall.
    fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Whether the bundle is queued and still arriving. Narrower than `!is_ready()` on purpose, and
    /// only for tests that pin which stage a fixture is in. Production reads `is_ready`: a request
    /// needs to know whether more is coming, not which stage produces it.
    #[cfg(test)]
    fn indexing_bundle(&self) -> bool {
        matches!(self, Self::Bundle(_))
    }
}

struct Analysis {
    graph: indexed::Indexed,
    /// What RBS says the graph's methods return.
    ///
    /// Beside the graph, because rubydex models no types (see [`types`]). Filled as signature files
    /// are indexed, and dropped with the graph.
    types: types::Types,
    /// What every document's `def`s were read to return, kept across requests.
    ///
    /// The body rung reads a method from the document declaring it, which would otherwise dominate
    /// a whole-file `inlayHint` on every keystroke, over unchanged gem and model files. Keyed by
    /// the text, so it cannot go stale. See [`types::HeldExits`], which also explains why the
    /// `Rebase` is rebuilt per request instead of kept here.
    exits: types::HeldExits,
    /// Whether the request being answered found the member under the cursor and nowhere to jump
    /// to, so settling could not help it. Cleared before each dispatch, set by `definition`, read
    /// by the deferred retry in `serve`.
    unplaced: std::cell::Cell<bool>,
    /// Where the declarations ya-lsp wrote itself were really declared.
    ///
    /// Beside the graph, like the type table. The mapping exists before any generator does, because
    /// typing `@story.title` and then jumping into a file the user does not have is worse than not
    /// typing it. See [`synthesized`].
    synthesized: Synthesized,
    /// What a template can call, rebuilt by the same pass as the RBS, for the same reason. Nothing
    /// in it is a declaration; see [`views`].
    views: views::Views,
    /// Which source files [`Analysis::synthesize`] generated from last time.
    ///
    /// The pass's own bookkeeping, not the side table's. A source that stops declaring anything
    /// sends no notification, so the only way to find stale output is to compare what this pass
    /// wrote last time with what it just wrote. Scoped to this pass on purpose: the table is
    /// shared, and pruning "everything I did not just write" would delete other recorders' entries.
    generated: HashSet<String>,
    /// Of [`Self::generated`], the sources the modules that read open buffers wrote
    /// ([`Wants::buffers`](crate::knowledge::Wants::buffers)), and the rest.
    ///
    /// Two sets, because opening a spec file re-runs only the first kind
    /// ([`Analysis::reopened_only`]). A source written by both holds both kinds' facts in its
    /// document, and re-recording one kind alone would drop the other's: see [`Self::shared`].
    buffered: HashSet<String>,
    /// The sources every other module wrote.
    unbuffered: HashSet<String>,
    /// What every other module wrote about each source the buffer-reading modules also wrote, at
    /// the last whole pass: a spec support file with a `Struct.new` beside its shared groups.
    ///
    /// A reopening merges these back in, in the whole pass's order, so it writes what the whole
    /// pass would. Nothing a reopening may skip can change them: it runs only where nothing but
    /// reopened documents moved.
    shared: crate::knowledge::Declared,
    /// The projection [`Analysis::synthesize`] last ran the generators on, for the pass gate.
    ///
    /// `None` until the first pass, the honest answer to "would it write the same thing again":
    /// nothing is known about a pass that has not happened.
    generated_from: Option<crate::knowledge::Context>,
    /// What every document the walk visited contributed to `generated_from`.
    ///
    /// **Two jobs, one map:**
    ///
    /// 1. **The gate's evidence.** The whole `Context` can only be compared after the walk. So a
    ///    keystroke re-derives the contribution of the one file it touched and compares that; the
    ///    other documents cannot have moved, since nothing else was indexed.
    /// 2. **The walk's memo.** The next walk reuses the held value for every document rubydex has
    ///    not re-indexed instead of projecting it again, most of a walk's cost on a large app.
    ///
    /// A document the walk does not visit has **no entry**, which differs from an entry with an
    /// empty contribution (see `Analysis::contribution`). `Analysis::walk` owns invalidation, using
    /// the same `touched`/`touched_all` pair the gate narrows on.
    contributions: HashMap<rubydex::model::ids::UriId, crate::knowledge::Contribution>,
    /// Every body of knowledge this build has, and the only place one is named.
    ///
    /// **Not in the pass**, which is the seam: `synthesize` reads this, never a module, so a build
    /// that registers nothing still compiles, runs and declares nothing. See [`crate::knowledge`].
    knowledge: crate::knowledge::Registry,
    /// The text each open buffer was **last handed to the indexer** as, which makes a deferred
    /// index answerable: rubydex's `Document` keeps a `content_hash` and a `LineIndex`, not the
    /// source, so the graph cannot say what its own offsets index.
    ///
    /// Written by `index_buffer`, where the text may differ from the buffer: a template is indexed
    /// as `erb::ruby_view` and an `.rbs` as `signatures::without_interfaces`. Recording what was
    /// *actually* indexed keeps the map honest for both. An `.rbs` just produces a rebase that
    /// refuses nearly everything, which falls back instead of answering in a coordinate system it
    /// does not share.
    indexed_text: HashMap<DocUri, String>,
    /// Which of the registry's [`Wants::spells`](crate::knowledge::Wants::spells) texts each
    /// indexed document's text holds, as the bits [`Self::spelling`] numbers; absent is none.
    ///
    /// Written wherever a text goes into the graph, by the indexer's workers or
    /// [`Self::index_contained`], so it always describes the version the graph holds, which is
    /// what the walk's memo is keyed on.
    spelled: HashMap<UriId, u64>,
    /// The texts [`Self::spelled`] is about, from the registry once: every build registers the
    /// same modules.
    spelling: Vec<&'static str>,
    /// Which documents were re-indexed since that pass, when exactly one is known.
    touched: HashSet<String>,
    /// Of [`Self::touched`], the documents touched only because the editor opened or closed them
    /// while a module reads them only when open ([`Analysis::touch_if_read_while_open`]). Their
    /// text in the graph did not move; an edit to one takes it out again.
    reopened: HashSet<String>,
    /// How many times the generators actually ran, rather than being gated out.
    ///
    /// The gate's instrument. Its claim is that it changes no answer, so counting the passes it
    /// prevented is the only way to see it working.
    passes: u64,
    /// How many times the **walk** ran: the same instrument one level down.
    ///
    /// `passes` cannot see this: the outer gate stops the generators but the projection they get is
    /// still rebuilt in full to decide that. Only a counter says whether a gated pass walked.
    walks: u64,
    /// How many settles re-ran only the modules that read open files
    /// ([`Analysis::reopened_only`]): the same kind of instrument, for the same reason.
    reopenings: u64,
    /// How many generated documents the placing step asked the graph about, over the session.
    placings: u64,

    /// Every file that pass read, and its on-disk state when it did.
    ///
    /// The half of the pass gate needing no notification: the pass claims its answer is a function
    /// of the disk, so the gate looks at the disk.
    stamps: Vec<(std::path::PathBuf, Option<(std::time::SystemTime, u64)>)>,
    /// Whether something was indexed without reporting a document.
    ///
    /// Every bulk route sets it (the workspace walk, a gem batch, the file watcher, a rebuild),
    /// which keeps the gate a *narrowing* of one path, not a claim about all of them. Only
    /// [`Analysis::index_buffer`] names its document, and that keystroke path is what the gate is
    /// for.
    touched_all: bool,
    open: HashMap<DocUri, OpenDocument>,
    encoding: PositionEncoding,
    client: ClientSupport,
    /// How to claim the files the server can answer about that the client's own selector did not
    /// cover.
    ///
    /// Held rather than called at startup, because its argument (the gem roots, Ruby's library, the
    /// RBS root) exists only once the bundle is discovered, on this thread, after the handshake.
    documents: DocumentRegistrar,
    /// The document registrations currently live in the client, kept for unregistering.
    ///
    /// A reload can move every prefix (`[gems] enabled`, an `[rbs] path`, a workspace root), and
    /// re-registering an id the client holds **replaces its record without disposing the old
    /// provider**, leaving the old selector answering beside the new one. So old ids are
    /// unregistered first, by name, which is why they are fixed strings.
    documents_live: Vec<lsp_types::Unregistration>,
    /// Whether the client has already been told it will not be asked again.
    ///
    /// Once per process, not per reload: a client that declines dynamic registration declines it
    /// for the session, and saving `ya-lsp.toml` five times should not repeat the sentence five
    /// times.
    documents_declined: bool,
    /// Generated documents the client has actually asked for the text of.
    ///
    /// - **Refresh only these.** `workspace/textDocumentContent/refresh` is the only way a client
    ///   learns a fileless document changed, and the only clue to which are on screen is which were
    ///   asked about. A cold index regenerates every body, and a refresh per body would be hundreds
    ///   of requests, at the busiest moment, about documents nobody opened.
    /// - **Spelled as the client asked, not as the server names it**, because they differ (see
    ///   `Synthesized::content`). A refresh with a URI the client cannot match refreshes nothing.
    /// - **Never pruned.** Closing a generated document sends no notification, so forgetting would
    ///   be guessing, and the set is bounded by what one person opened in one session.
    served: HashMap<String, String>,
    workspace: Workspace,
    outgoing: Sender<Message>,
    cancellations: Cancellations,
    /// Whether the graph holds indexed-but-unresolved work.
    ///
    /// Separate from `resolve_at` because they answer different questions: this is "would an answer
    /// be stale" (a request must check), `resolve_at` is "when to resolve unprompted". Background
    /// gem indexing sets this without touching the timer, so a burst of gem work cannot push the
    /// user's diagnostics further out.
    dirty: bool,
    /// When the debounced global resolve is due. `None` means none is scheduled.
    resolve_at: Option<Instant>,
    /// Buffers whose edit has been applied but not yet indexed.
    ///
    /// - **Why deferred.** A `didChange` applies the edit to `open` (microseconds) and indexes the
    ///   document into the graph (hundreds of milliseconds for a central model on a large app,
    ///   because rubydex's invalidation cascades over every declaration naming it). The four
    ///   requests `needs_the_graph` exempts read only the buffer, and editors send
    ///   `semanticTokens/full` after every keystroke, so they must not wait for an index they do
    ///   not read.
    /// - **Drained only by [`Analysis::settle`], which needs two things:** the map, so completion
    ///   answers from the last settled graph instead of waiting; and [`RESOLVE_DEBOUNCE`] at 500
    ///   ms, so the settle falls after a burst, not inside it. Without both, a settle armed at a
    ///   short debounce is still running when the next request arrives and cannot be interrupted.
    /// - **An eager drain at the next idle moment is worse than either**: it leaves the graph
    ///   holding a class with no members, not a stale one.
    pending_index: Vec<DocUri>,
    /// Where the cold start has got to. See [`Stage`]: one stage at a time, each naming the next.
    stage: Stage,
    /// How many of the user's own files the index holds, against `index.max_files`.
    ///
    /// The walk's cap must keep applying after the walk: a watcher can add files the walk stopped
    /// before, and the cap exists because pathological repositories exist. Counted as files come
    /// and go, not recomputed: once a bundle is in, asking the graph means walking tens of
    /// thousands of documents, per changed file on a branch switch.
    workspace_files: usize,
    /// Documents whose last indexing attempt crashed rubydex: the bulkhead's skip list.
    ///
    /// - **Not [`Analysis::rebuild`].** `resolve` rebuilds because a half-linked graph is in an
    ///   unknown state. A contained index panic leaves a known one (the graph minus one document),
    ///   and the file is still on disk, so a rebuild would re-read it and crash again. So the file
    ///   is remembered, and `rebuild` deliberately does **not** clear this set.
    /// - **Self-clearing.** A document is here exactly while its last attempt panicked. Every route
    ///   that indexes because the text may have moved (`didOpen`, `didChange`, `didClose`, the
    ///   watcher) just tries again and removes the entry on success.
    /// - **Consulted by the two routes that re-read unmoved text**: the workspace walk (which a
    ///   rebuild reruns) and the gem batch (which a rebuild re-queues).
    skipped: HashSet<DocUri>,
    /// Whether a rebuild after a resolver panic is already running.
    ///
    /// Guards the one recursion that matters: the rebuild indexes, indexing resolves, and a rebuild
    /// that panics again would rebuild forever.
    recovering: bool,
    /// Whether the user was already told the index is full, since the last reload.
    ///
    /// Said once. A `git checkout` in a workspace over the cap would otherwise raise the same
    /// notification on every branch switch.
    index_full_reported: bool,
    /// Every workspace document URI starts with this. Keeps gem diagnostics off screen without
    /// parsing a URL per diagnostic.
    workspace_prefix: String,
    /// The project's own code **outside** the root: `[index] load_paths` entries pointing outside
    /// it, as directory URI prefixes.
    ///
    /// The only other way to be the user's own code. A monorepo whose apps share a tree beside them
    /// names it here, and every "may I act on this file?" surface must say yes. Without it a shared
    /// model gets no diagnostics, cannot be renamed, and ranks as someone else's code in search, in
    /// a directory the project wrote down by hand. Gem roots stay out, which is why this is a
    /// separate list, not a wider `workspace_prefix`.
    own_prefixes: Vec<String>,
    /// URI prefixes of everything indexed that is not the user's code: the gem roots, and the RBS
    /// root of Ruby's own signatures.
    ///
    /// The workspace prefix alone is not enough: a *vendored* bundle lives at
    /// `vendor/bundle/ruby/<abi>`, inside the root, so every gem there would pass the workspace
    /// test and publish diagnostics nobody can fix. Likewise an `[rbs] path` inside the project.
    /// Kept per root, not per gem, so the test stays a handful of comparisons.
    foreign_prefixes: Vec<String>,
    /// URI prefixes of the `app/` directory of every gem that has one: a Rails engine's Ruby.
    ///
    /// A *second* list beside `foreign_prefixes`, not a hole in it, because they answer different
    /// questions. `is_own_code` still says no to these: nobody can fix a warning in someone else's
    /// engine or rename a method there. But an engine's `app/` matters to generators:
    /// `has_many :attachments` on `ActiveStorage::Blob` is a member of a class the user does name.
    /// So [`Analysis::walk`] asks [`Analysis::is_generator_source`], and every other caller asks
    /// `is_own_code`.
    ///
    /// Filled from [`gems::Gems::engine_paths`], not guessed from a path, so the two cannot
    /// disagree about which directories were walked.
    engine_prefixes: Vec<String>,
    /// The load path as document-URI prefixes, in `require`'s search order.
    ///
    /// A **third** list, and the only one that is an order, not a set: the other two ask *is this
    /// document foreign*, this one asks *which of two copies of a file would this project load*.
    /// `locator::places` is the caller; a bundle pinning `cgi` is the case (the gem's
    /// `lib/cgi/escape.rb` comes first, and Ruby's own copy is never reached).
    ///
    /// From `Workspace::load_paths`, the same list `require` resolution reads, so the two cannot
    /// disagree. Empty until the bundle is discovered, which is exactly when there is no second
    /// copy to get wrong.
    load_prefixes: Vec<String>,
    /// The last non-empty diagnostic set sent per URI.
    ///
    /// `publishDiagnostics` is stateful: the last set sent for a URI stays on screen until
    /// something replaces it. Keeping it lets the server send only changes, and, just as important,
    /// an explicit empty set for a URI whose problems went away.
    published: HashMap<DocUri, Vec<lsp_types::Diagnostic>>,
    /// The one handle that can re-point the log, held because `[log]` is re-read here.
    ///
    /// A `ya-lsp.toml` change arrives on this thread, so this thread must apply it; otherwise every
    /// setting would reload except the one saying what to log about the reload.
    logging: crate::logging::Reload,
}

/// A template's markup, for what its view blanks out: the editor's buffer where one is
/// open, else the file.
impl types::Markup for Analysis {
    fn markup(&self, uri: &str) -> Option<Rc<str>> {
        let document = DocUri::from_graph_uri(uri)?;
        self.with_source(&document, |text| Rc::from(text))
    }
}

impl Analysis {
    /// How many source files every module has read (from disk or a buffer) and parsed.
    ///
    /// The third instrument beside [`Analysis::passes`] and [`Analysis::walks`]. Each module keeps
    /// its own memo and counts its own reads; this adds them up. It counts **files, not parses**:
    /// one file on three of a module's lists is one read and up to three readers.
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
        let structs = self
            .knowledge
            .of::<crate::knowledge::structs::Structs>()
            .map_or(0, |module| module.reads);
        let defines = self
            .knowledge
            .of::<crate::knowledge::defines::Defines>()
            .map_or(0, |module| module.reads);
        let mixins = self
            .knowledge
            .of::<crate::knowledge::mixins::Mixins>()
            .map_or(0, |module| module.reads);
        rails + annotated + structs + defines + mixins
    }

    /// Every body of knowledge this build has, in the order they run.
    ///
    /// **The one line core says about Rails**, and deliberately not in the pass: a build that
    /// registers nothing compiles, runs and declares nothing, which makes the seam real rather
    /// than asserted. See [`crate::knowledge`].
    fn registered() -> crate::knowledge::Registry {
        crate::knowledge::Registry::new(vec![
            Box::new(crate::knowledge::rails::Rails::default()),
            Box::new(crate::knowledge::annotations::Annotations::default()),
            Box::new(crate::knowledge::structs::Structs::default()),
            Box::new(crate::knowledge::rspec::RSpec::default()),
            Box::new(crate::knowledge::factories::Factories::default()),
            Box::new(crate::knowledge::singletons::Singletons),
            Box::new(crate::knowledge::defines::Defines::default()),
            Box::new(crate::knowledge::mixins::Mixins::default()),
            Box::new(crate::knowledge::i18n::Translate::default()),
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
        // A directory URI, so the prefix test cannot match a sibling that merely starts the same
        // way (`/app` must not swallow `/app-vendor`).
        let workspace_prefix = DocUri::from_path(workspace.root())
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .unwrap_or_default();

        // Deliberately not calling `Graph::set_encoding`: it only feeds `Graph::encoding()`, and
        // `Offset::to_location` ignores it. Leaving the default keeps every rubydex offset in
        // bytes, which is what `position::TextDocument` converts from.
        Self {
            graph: indexed::Indexed::default(),
            types: types::Types::new(),
            exits: types::HeldExits::new(),
            unplaced: std::cell::Cell::new(false),
            synthesized: Synthesized::new(),
            views: views::Views::default(),
            generated: HashSet::new(),
            buffered: HashSet::new(),
            unbuffered: HashSet::new(),
            shared: crate::knowledge::Declared::new(),
            generated_from: None,
            walks: 0,
            reopenings: 0,
            placings: 0,
            contributions: HashMap::new(),
            knowledge: Self::registered(),
            indexed_text: HashMap::new(),
            spelled: HashMap::new(),
            spelling: Self::registered().spelled(),
            touched: HashSet::new(),
            reopened: HashSet::new(),
            passes: 0,
            stamps: Vec::new(),
            touched_all: true,
            open: HashMap::new(),
            encoding,
            client,
            documents,
            documents_live: Vec::new(),
            documents_declined: false,
            served: HashMap::new(),
            workspace,
            outgoing,
            cancellations,
            dirty: false,
            foreign_prefixes: Vec::new(),
            engine_prefixes: Vec::new(),
            load_prefixes: Vec::new(),
            resolve_at: None,
            pending_index: Vec::new(),
            stage: Stage::Workspace,
            workspace_files: 0,
            skipped: HashSet::new(),
            recovering: false,
            index_full_reported: false,
            workspace_prefix,
            // Filled by `index_workspace`, not here, so a `ya-lsp.toml` reload (which reruns that
            // walk and can change `[index] load_paths`) cannot leave it describing the old
            // configuration. Every other prefix list works this way.
            own_prefixes: Vec::new(),
            published: HashMap::new(),
            logging,
        }
    }

    /// Add the project's load paths outside the workspace root to the walk's result.
    ///
    /// - **Why.** `discover` starts at the root with `follow_links` off, so a sibling directory, or
    ///   a symlink to one, is reached by nothing else. Without this, a monorepo's shared tree on
    ///   `[index] load_paths` would resolve `require`s to documents the graph does not hold.
    /// - **Taken as `.rb` and `.rbs`**, like every other load path in the crate, not through
    ///   `index.include`: those globs are relative to the root and cannot describe a tree outside
    ///   it.
    /// - **Inside `index.max_files` all the same**: it is a budget over the project's own code, and
    ///   this is the project's own code.
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

    /// Index everything in the workspace, once, at startup (and on rebuild).
    fn index_workspace(&mut self) {
        let started = Instant::now();
        self.warn_about_unknown_rules();
        self.say_what_the_fences_replaced();
        // Before the walk that reads them, and re-read on every rebuild, because
        // `[index] load_paths` is configuration and a reload arrives here. A directory URI, so the
        // prefix test cannot match a sibling that merely starts the same way.
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
        // What the cap is measured against from here on. `truncated` already reported; this carries
        // the same budget forward so a file created later still meets it.
        self.workspace_files = count;
        self.index_full_reported = discovery.truncated;
        // The bulkhead's skip list. This is one of the two routes that re-reads unmoved text:
        // whatever crashed the indexer is still on disk, and `rebuild` runs this again. It goes
        // first so it also covers the two pre-passes, which are inline and would hit the same file.
        let files = self.without_skipped(discovery.files);
        // Two pre-passes over the batch, each taking the files it must edit before the graph sees
        // them and passing the rest on. Same rule for both: a file that reaches rubydex unedited is
        // indexed *wrongly*, not just differently. `index.include` covers `**/*.rbs` by default, so
        // a project's own `sig/` reaches this path with no configuration, and an `interface` there
        // would otherwise land its members on `Object`, as one in Ruby's own signatures would.
        let files = self.index_edited_signatures(files);
        let files = self.index_templates(files);
        let batch = indexer::index_files(self.graph.graph_mut(), files, &self.spelling);
        let indexed = started.elapsed();
        self.note_spelled(&batch.spelled);

        for error in &batch.errors {
            tracing::warn!("indexing error: {error:?}");
        }
        for uri in batch.skipped {
            self.record_skip(&uri);
        }

        // A whole tree just went into the graph, and this route names no document for it: exactly
        // what `touched_all` means. The cheap gate trusts `touched` to be everything that moved, so
        // a bulk route that forgets this makes it unsound. (The two production callers happen to be
        // covered anyway, since `generated_from` is `None` at startup and `Analysis::rebuild`
        // clears it, but that is the callers' property, not this method's.)
        self.touched_all = true;
        // There is something to settle: a whole tree just went into the graph. Set here, not
        // inferred at the generator stage, so that stage is idempotent: a request settling before
        // the loop steps does the pass, and the step then finds nothing left to do.
        self.dirty = true;

        // **Indexed, but neither linked nor generated.** This stage only promises every workspace
        // file is in the graph; linking is the generator stage's first act, because
        // [`Self::regenerate`] is one ordered sequence whose middle step is the resolve.
        //
        // - **Resolving here is not a free head start.** The pass is *written* for an unresolved
        //   graph holding only `Object`, `BasicObject`, `Module` and `Class`, and a linked one
        //   sends `types::harvest` into a recursion that hangs the suite.
        // - **Generating here cannot be right either.** The generators read `graph.definitions()`
        //   whole, so before the bundle is in they see a fraction of the concerns, and
        //   `place_generated_members` finds none of the bundle's definitions. See [`Stage`].
        //
        // **This stage is done and names the next: the generator pass.** That is the whole pipeline
        // for a project with no gems. `queue_background_indexing`, which both production callers
        // run right after this, inserts the bundle stage first when there is a bundle.
        self.stage = Stage::Generate;
        let refused = self.skipped.len();
        tracing::info!(
            "indexed {count} files in {indexed:.2?}, {refused} refused (total {:.2?})",
            started.elapsed()
        );
    }

    /// Main loop with a debounce timer for the settle.
    fn run(&mut self, receiver: &Receiver<Task>) {
        loop {
            // Gem indexing runs only in the gaps. Checking the queue first makes "background" real:
            // with work waiting, the editor's request goes first and the bundle waits.
            //
            // The `continue` also means an armed `resolve_at` is not checked until background work
            // runs out. An edit during a cold start reaches the *buffer* before the bundle (it is a
            // task, and tasks come first), but its index and debounced resolve belong to the
            // settle, which waits for the bundle. A request arriving meanwhile is not stalled:
            // `defers` answers it over the map, and one that cannot be mapped settles, which jumps
            // the queue. Reversing the order would cost a resolve per debounce of typing.
            // `threaded_tests::push_diagnostics_for_an_edit_wait_for_the_background_index` pins the
            // current choice.
            if receiver.is_empty() && self.step_pipeline() {
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

        // Drain pending resolution so a shutdown does not leave the graph half-linked. A future
        // on-disk cache would be written from here.
        if self.dirty {
            self.settle();
        }
    }

    fn handle(&mut self, task: Task) {
        // **Log what arrived that was not a request.** A notification changes every later answer
        // without saying so: a dropped `didChange`, a watched-file change this workspace does not
        // index, a config reload that threw the graph away. Any of these may explain an answer
        // someone is about to report as wrong.
        //
        // Requests are not logged here: `serve` writes its own pair, and a third line would say the
        // same thing less precisely.
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
                // The buffer answers for it now, not a text held while it was closed.
                self.exits.forget_texts();
                self.open.insert(
                    uri.clone(),
                    OpenDocument {
                        text: TextDocument::new(text.clone(), self.encoding),
                        version,
                        // What the client just sent is what it read from disk.
                        saved: true,
                        client_copy: None,
                    },
                );
                self.index_buffer(&uri, &text);
                self.touch_if_read_while_open(&uri);
            }
            Task::DidChange {
                uri,
                changes,
                version,
            } => {
                if !self.open.contains_key(&uri) {
                    // A change for a buffer never opened. Clients occasionally do this, and
                    // dropping the edit strands the file on stale content, but an incremental range
                    // only means something against the exact text it was computed from. So recover
                    // only when the client sent the whole buffer: applying a range to the wrong
                    // base is worse than not applying it.
                    if !changes.iter().any(|change| change.range.is_none()) {
                        tracing::warn!(
                            "didChange for un-opened {uri}; an incremental edit cannot be \
                             reconstructed, so it is being dropped"
                        );
                        return;
                    }
                    if uri.is_untitled() {
                        // The recovery below invents an open document, and for a buffer with no
                        // file that is the *only* way it would be indexed. Which unsaved buffers
                        // are indexed is decided by `didOpen`'s `languageId` alone (a `didChange`
                        // has none), so a buffer refused there must not get in through here.
                        tracing::debug!("didChange for un-opened {uri}; it was never admitted");
                        return;
                    }
                    tracing::warn!("didChange for un-opened {uri}; treating it as an open");
                    self.exits.forget_texts();
                    self.open.insert(
                        uri.clone(),
                        OpenDocument {
                            text: TextDocument::new(String::new(), self.encoding),
                            version,
                            saved: false,
                            client_copy: None,
                        },
                    );
                }

                {
                    let document = self.open.get_mut(&uri).expect("inserted above if missing");
                    // **The ranges belong to the client's copy, not the buffer.** They were
                    // computed against what the client last held, which is the buffer unless the
                    // server swapped in the disk's text under `Watched::ByTheServer`. Taking the
                    // copy here puts them back in step: the change lands on the text it was
                    // measured against, and the result is the buffer again.
                    if let Some(theirs) = document.client_copy.take() {
                        document.text = theirs;
                    }
                    for change in &changes {
                        document.text.apply(change.range, &change.text);
                    }
                    document.version = version;
                    // Anything on disk is now older than this, whoever wrote it.
                    document.saved = false;
                }
                // The edit is applied; the index is not (see `pending_index`). `mark_dirty_for`
                // must happen here, not with the index, or a graph request in between would see
                // `dirty` false and answer without the edit.
                if !self.pending_index.contains(&uri) {
                    self.pending_index.push(uri.clone());
                }
                self.mark_dirty_for(&uri);
            }
            Task::DidClose { uri } => {
                // **Whether this document was squiggled for being open**, asked before the buffer
                // is gone because the answer depends on it: a document outside the project is
                // published only while open. Closing it moves nothing in the graph (the text on
                // disk is what was indexed), so no settle follows to clear it, and without this its
                // squiggles would outlive the tab.
                let was_beside = self.layout().is_outside(uri.as_str());
                self.open.remove(&uri);
                // Closing a buffer does not remove the file from the project. Fall back to the
                // disk; drop the document only if the file is really gone.
                match uri.to_file_path().filter(|path| path.is_file()) {
                    Some(path) => match std::fs::read_to_string(&path) {
                        Ok(text) => {
                            self.index_buffer(&uri, &text);
                        }
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
                self.touch_if_read_while_open(&uri);
                if was_beside {
                    self.publish_diagnostics();
                }
            }
            Task::DidSave { uri } => {
                // The indexed buffer is what got written, so there is nothing to index. It is now
                // also what is on disk, which is what makes it safe for a later disk change to
                // replace it. `client_copy` is left alone: a save does not change what the client
                // holds, and the next `didChange`'s ranges are still measured against it.
                if let Some(document) = self.open.get_mut(&uri) {
                    document.saved = true;
                }
                tracing::trace!("saved {uri}");
            }
            Task::WatchedFiles { uris, watched } => {
                tracing::debug!(paths = uris.len(), ?watched, "watched files changed");
                self.refresh(uris, watched);
            }
            Task::ChangeConfig { options } => {
                self.workspace.set_options(options);
                self.handle(Task::ReloadConfig);
            }
            Task::ReloadConfig => {
                let mut problems = self.workspace.reload();
                // Before anything reads the new configuration. `[trees]` and `[index] load_paths`
                // are the two non-graph inputs to the table beside the graph, so a reload is the
                // one event that can make a held answer wrong, specifically answers that *drop* a
                // name from a list.
                self.graph.forget_placement();
                // Re-point the log before logging the reload, so a file log just turned on records
                // the reload that turned it on.
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

    /// Find everything outside the workspace that belongs in the graph (Ruby's own signatures and
    /// the project's gems) and queue their files. Indexes nothing itself.
    ///
    /// Discovery walks a few hundred directories: not free, but bounded and done once. Indexing the
    /// files it finds is seconds of work that must be interleaved with serving requests.
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
                // **The directories that hold something, not the directories searched.**
                // `Gems::roots` is every gem path on the machine (every Ruby asdf installed, plus
                // the system's), because the first question when no gems are found is where was
                // looked. Right for `is_own_code`, where wider is still correct; wrong for a
                // document selector, which would claim every Ruby file of every installed Ruby. So:
                // one directory per gem the lockfile resolved, containing its `lib/`, `sig/` and
                // any engine `app/`.
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

        // `.gem_rbs_collection/` is in `roots` for the same reason a vendored bundle is: it lives
        // *inside* the workspace root, so without it every squiggle in someone else's curated
        // signatures would be published as the user's.
        self.foreign_prefixes = roots
            .iter()
            .chain(signatures.origin.is_some().then_some(&signatures.root))
            .filter_map(|root| DocUri::from_path(root))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .collect();
        // The one moment `is_own_code` changes its mind about documents already in the graph: a
        // vendored bundle lives *inside* the root, so its files were the user's own until this
        // line. The bundle indexing below writes the graph and would drop the table anyway; this
        // covers a request served in between.
        self.graph.forget_placement();
        // From the same list the walk below uses, so "indexed as an engine" and "read as an engine"
        // cannot diverge. A directory URI, for `workspace_prefix`'s reason: `.../app` must not
        // match a gem's `app-bundle/`.
        self.engine_prefixes = engine_paths
            .iter()
            .filter_map(|path| DocUri::from_path(path))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .collect();
        // The walk's memo is keyed by "has rubydex re-indexed this document", and this list is its
        // one non-document input: `Analysis::is_generator_source` reads it, so a contribution
        // projected under the old list may admit a former engine's `app/` or refuse a new one. The
        // batch below sets `touched_all` and would drop the map anyway; clearing it here keeps the
        // dependency visible in one place.
        self.contributions.clear();
        // `Workspace::load_paths`, not the `load_paths` above, because the project's own `lib/` is
        // on it and shadows a same-named gem (what `require "version"` in an application means).
        // Order preserved: it is the list's whole content.
        self.load_prefixes = self
            .workspace
            .load_paths()
            .iter()
            .filter_map(|path| DocUri::from_path(path))
            .map(|uri| format!("{}/", uri.as_str().trim_end_matches('/')))
            .collect();

        // Here, before the walk below, and not behind the gem budget: this depends on which roots
        // were *discovered*, and a bundle big enough to be truncated is the one whose gems a user
        // most likely reads.
        //
        // The project's own trees outside the root join the gems' list for the same reason: a
        // client's selector is its folder, so a file outside every folder never gets a request.
        // They are not *foreign* (`own_prefixes` keeps them the user's code), just elsewhere, which
        // is what `register_documents` is for.
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

        // Signatures first, outside the gem budget. A few hundred files against a bundle's tens of
        // thousands, and the difference between `String` existing or not: letting
        // `[gems] max_files` decide whether Ruby's core is indexed would remove the built-ins on
        // exactly the largest projects.
        let mut files = signatures.files();
        let signature_files = files.len();

        // One load path at a time, not all at once, so the budget stops at a gem boundary and the
        // lockfile's first gem is indexed first. That needs a dedup: Ruby's platform directory is
        // nested *inside* its library directory, and a vendored bundle can sit inside a gem root.
        let mut truncated = false;
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        // 1. **The bundle's signatures before its code**, so a bundle that hits the budget still
        //    gets what answers what a method *returns*, the cheapest declarations of the pass. They
        //    stay inside `[gems] max_files`: unlike Ruby's core they are not the difference between
        //    `String` existing or not, and a budget with per-list exceptions stops being a budget.
        // 2. **Then each Rails engine's `app/`, before the bundle's `lib/`.** An engine declares
        //    `require_paths = ["lib"]`, so `ActiveStorage::Blob`, a class countless applications
        //    name, is on no load path and would not be a document at all. It is small next to the
        //    bundle, so a truncated bundle still gets the missing half.
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
            // Nothing outside the workspace to index, so no bundle stage is inserted, and the stage
            // `index_workspace` named stands.
            return;
        }
        let gems = gem_count;
        tracing::info!(
            "queued {signature_files} signature files and {} files from {gems} gems in {:.2?}",
            total - signature_files,
            started.elapsed()
        );

        // Popped from the back, so reversing makes the list's head the first indexed: signatures,
        // then the first gem, which Bundler's dependency order makes the one a user most likely
        // jumps into.
        files.reverse();
        // The bundle stage goes *before* the generator pass, which is the point: the generators
        // read `graph.definitions()` whole, so they must run when nothing is left to index.
        self.stage = Stage::Bundle(GemIndexing {
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

    /// Step whichever stage the pipeline is on. Returns true while there is more to do.
    ///
    /// **The whole contract is this match**: one stage at a time, each ending by naming the next,
    /// the loop asking only "is there more". The run loop calls it in idle turns and
    /// [`Analysis::serve`]'s third rung drains it. Because the generator pass is a stage, not a
    /// side effect of a settle, that drain hands the retried request a graph with the generated
    /// documents in it.
    fn step_pipeline(&mut self) -> bool {
        match &self.stage {
            Stage::Bundle(_) => self.step_bundle(),
            Stage::Generate => {
                self.generate();
                true
            }
            // Nothing for the loop to do: `Workspace` is stepped by its eager caller, and `Ready`
            // is the end.
            Stage::Workspace | Stage::Ready => false,
        }
    }

    /// The pipeline's last stage: everything is indexed, so generate from it.
    ///
    /// **The stage moves first**, because [`Self::settle`] suppresses the pass while the bundle
    /// arrives, and this is when that stops. Then an ordinary settle, which makes the step
    /// idempotent: the workspace index and every gem batch set `dirty`, so the pass runs here,
    /// unless a request that arrived first already settled, and then this is just a stage flip and
    /// a hint refresh.
    ///
    /// **It streams progress** because otherwise this is the one silent window of a cold start: the
    /// steps before it are fast or streamed (the bundle), and this pass takes around a second on
    /// large apps, between the gem stream's end and the first correct answer. It sends nothing
    /// else: the pass changes no published diagnostic, and a client without `inlayHint` has nobody
    /// to tell. A server that has gone quiet but is not finished looks like a hang.
    ///
    /// **The audit relies on it.** `client.settle` decides a server is ready by three conditions,
    /// the second being *no `$/progress` stream is open*: a statement, where the third is an
    /// inference from three seconds of silence. Without a stream, every sweep would depend on the
    /// pass fitting inside that quiet, which a bigger project or a slower runner can break, and the
    /// sweep would score a half-built graph as regressions. `audit.md` has the details.
    ///
    /// **No `report` in between**: one blocking call with no count to divide, and a made-up
    /// percentage is worse than a title. The begin message is empty for the same reason.
    fn generate(&mut self) {
        self.stage = Stage::Ready;
        let progress = Progress::begin(
            &self.outgoing,
            self.client.work_done_progress,
            GENERATE_PROGRESS_TOKEN,
            "Finishing the index",
            String::new(),
        );
        let started = Instant::now();
        self.settle();
        // The hints, which have no timer. This is the one moment an already-answered request's
        // answer changes without the document changing: a file opened before signatures arrived has
        // an empty margin, and nothing else would ask again.
        self.refresh_hints();
        // **After the refresh; that is the stream's contract.** It says "answers are not final
        // yet", so it may not close while something that changes an answered request is still
        // coming.
        if let Some(progress) = progress {
            progress.end(format!("in {:.2?}", started.elapsed()));
        }
    }

    /// Index one batch of the bundle. Returns true while there is more to do.
    fn step_bundle(&mut self) -> bool {
        let Stage::Bundle(work) = &mut self.stage else {
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
            // The second route that re-reads unmoved text: a rebuild re-queues the whole bundle,
            // and `mkmf-rice.rb` is a gem file.
            let batch = self.without_skipped(batch);
            let batch = self.index_edited_signatures(batch);
            let outcome = indexer::index_files(self.graph.graph_mut(), batch, &self.spelling);
            self.note_spelled(&outcome.spelled);
            for error in &outcome.errors {
                // Debug, not warn: a hundred-gem bundle always contains something that does not
                // parse, and none of it is the user's problem.
                tracing::debug!("gem indexing error: {error:?}");
            }
            // A crash is different: one gem file silently answering nothing is what a user would
            // otherwise spend an afternoon on.
            for uri in outcome.skipped {
                self.record_skip(&uri);
            }
            // Marked dirty without arming the debounce timer. The next request resolves what is
            // there; a resolve per batch would re-link the whole graph dozens of times for nobody.
            //
            // `touched_all` for the pass gate, not the timer: a batch of gem files is a change this
            // loop cannot name a document for, and a gem's `app/` is on four of the six lists. The
            // `Context` comparison would catch anything the walk sees anyway; this says it where it
            // is cheap.
            self.touched_all = true;
            self.dirty = true;
        }

        if !finished {
            return true;
        }

        let Stage::Bundle(work) = std::mem::replace(&mut self.stage, Stage::Generate) else {
            unreachable!("the bundle stage was matched at the top of this method")
        };
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
        // The stage is now `Generate`, set by the `replace` above. **Not `mark_dirty()`**: that
        // arms the debounce, delaying the pass by `RESOLVE_DEBOUNCE` after the last gem for no
        // reason, and it means "something changed" when what happened is "nothing is left to
        // index". The loop steps the next stage on its next turn.
        true
    }

    /// Index the signature files in `batch` that need editing first, and return the rest.
    ///
    /// - **One rule for every route.** See [`signatures`] for what is edited and why. Every `.rbs`
    ///   enters the graph through here or [`Self::index_buffer`]: the signature root
    ///   `workspace::rbs` found, a `sig/` that `index.include` covers, or a buffer the editor
    ///   opened. A file indexed differently depending on how it was found is worse than no
    ///   filtering at all.
    /// - **Every `.rbs` is read here**, for the return types, and only the one in five holding an
    ///   interface leaves the parallel path; the rest stay plain paths for worker threads.
    /// - **The cost is reading each signature twice**, here and in the indexer, which is small next
    ///   to a pass that takes seconds in the background. Indexing from the string held here would
    ///   avoid it but move megabytes of parsing off the workers onto this thread.
    fn index_edited_signatures(&mut self, batch: Vec<PathBuf>) -> Vec<PathBuf> {
        batch
            .into_iter()
            .filter(|path| !self.index_edited_signature(path))
            .collect()
    }

    /// Whether `path` was a signature file with an interface in it, now indexed.
    fn index_edited_signature(&mut self, path: &Path) -> bool {
        if path.extension() != Some(OsStr::new("rbs")) {
            return false;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            return false;
        };
        // Harvested before the edit, on purpose: the harvest skips `interface` blocks itself, so it
        // sees the same declarations either way, and doing it here covers the four files in five
        // that leave by the early return below.
        //
        // The URI must be `DocUri`'s spelling, as below, or an edit would file a second
        // contribution beside the startup walk's (see `Types::harvest`).
        if let Some(uri) = DocUri::from_path(path) {
            self.types.harvest(uri.as_str(), &source);
        }
        let Some(edited) = signatures::without_interfaces(&source) else {
            return false;
        };
        // The URI must be spelled as `index_files` would spell it, or this forks a second document
        // for the same file. `DocUri` is that spelling.
        let Some(uri) = DocUri::from_path(path) else {
            return false;
        };
        self.index_contained(&uri, &edited, &LanguageId::Rbs);
        true
    }

    /// Index the ERB templates in `batch`, and return the rest.
    ///
    /// - **[`Self::index_edited_signatures`]' shape, with higher stakes.** An unedited `.rbs` adds
    ///   extra declarations; an unedited template records **no references at all**: rubydex reads
    ///   the markup as Ruby, gives up in the first tag, and files parse errors instead of call
    ///   sites. See [`erb`].
    /// - **Runs on the walk, not only on `didOpen`**, because a real Rails app keeps about one call
    ///   site in seven in its templates. Indexing only open files would make `references`
    ///   incomplete by an amount that changes as tabs open, worse than a consistently narrow
    ///   answer, and exactly what `coverage.md` holds `references.rs` at 100% to prevent.
    /// - **Cheap**: around half the cost of indexing the app's `.rb` files.
    fn index_templates(&mut self, batch: Vec<PathBuf>) -> Vec<PathBuf> {
        batch
            .into_iter()
            .filter(|path| !self.index_template(path))
            .collect()
    }

    /// Whether `path` was a template, now indexed.
    fn index_template(&mut self, path: &Path) -> bool {
        if !erb::is_template(path) {
            return false;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            return false;
        };
        // Spelled as `index_files` would, or this forks a second document for the same file.
        let Some(uri) = DocUri::from_path(path) else {
            return false;
        };
        self.index_contained(&uri, &erb::ruby_view(&source), &LanguageId::Ruby);
        true
    }

    /// Index whatever `didChange` deferred.
    ///
    /// [`Analysis::settle`] is the only caller, and calls this first: the graph must hold every
    /// edit before the generators read it or the resolver links over it. Returns nothing: no caller
    /// needs to know whether work happened.
    ///
    /// A buffer closed between the edit and now is skipped, not replayed: `didClose` already
    /// restored the file to what is on disk, and re-indexing the buffer would undo that.
    fn index_pending(&mut self) {
        for uri in std::mem::take(&mut self.pending_index) {
            let Some(text) = self.open.get(&uri).map(|open| open.text.text().to_owned()) else {
                continue;
            };
            let _ = self.index_buffer(&uri, &text);
        }
    }

    /// Put `text` into the graph as `uri`, and say whether the graph was asked to take it.
    ///
    /// The return is for [`Self::refresh`], which logs "re-indexed N"; a skip does not count.
    fn index_buffer(&mut self, uri: &DocUri, text: &str) -> bool {
        let path = uri.to_file_path();
        let language = path
            .as_deref()
            .map_or(LanguageId::Ruby, indexer::language_of);
        // The same rule as the indexing path. Otherwise opening a signature file puts back the
        // declarations that path removed, and they stay, since nothing re-indexes the file after
        // the buffer closes. Templates likewise: `didOpen`, `didChange` and the watcher all share
        // this hook, and a template reaching the graph raw through any of them would replace its
        // call sites with parse errors.
        if path.as_deref().is_some_and(erb::is_template) {
            let view = erb::ruby_view(text);
            // The view, not the buffer, here and below: rubydex's offsets index `ruby_view`, which
            // is why a template needs no special case.
            if self.graph_holds(uri, &view) {
                self.indexed_text.insert(uri.clone(), view);
                return false;
            }
            if self.index_contained(uri, &view, &LanguageId::Ruby) {
                self.indexed_text.insert(uri.clone(), view);
            }
            self.mark_dirty_for(uri);
            return true;
        }
        let edited = matches!(language, LanguageId::Rbs)
            .then(|| {
                // The third route a signature takes into the graph, and the return types must
                // follow it, as the interface rule does: a table that disagrees with the graph
                // about a method gives a wrong answer, not a missing one.
                self.types.harvest(uri.as_str(), text);
                signatures::without_interfaces(text)
            })
            .flatten();
        let text = edited.as_deref().unwrap_or(text);
        if self.graph_holds(uri, text) {
            self.indexed_text.insert(uri.clone(), text.to_owned());
            return false;
        }
        // **Only when the index actually happened**: where the bulkhead meets the map.
        //
        // - **A contained panic costs the document its update, nothing else**: the graph keeps
        //   answering with the version it held. Recording the new text here would claim the graph
        //   holds it; `Rebase::between` would then compare equal strings, answer `identity`, and
        //   hand every offset to a graph an unknown number of edits behind, a wrong answer with no
        //   refusal.
        // - **Keeping the previous entry is correct**, not just safer: it is the text the graph
        //   really holds, so the map describes the difference exactly. A document whose very first
        //   index crashes has no entry (identity), which is harmless: the graph has no such
        //   document to be wrong about.
        if self.index_contained(uri, text, &language) {
            self.indexed_text.insert(uri.clone(), text.to_owned());
        }
        self.mark_dirty_for(uri);
        true
    }

    /// Whether the graph already holds exactly `source` for `uri`, so indexing it would do nothing.
    ///
    /// - **A pre-check of rubydex's own check**, answering yes only where that one would.
    ///   `Graph::consume_document_changes` compares `Document::content_hash` (`xxh3_64` of the
    ///   source) and returns early, so an unchanged document still costs a Prism parse and a full
    ///   walk that is thrown away. Hashing here, one step earlier, skips the parse for
    ///   microseconds.
    /// - **It saves more than the parse.** A write goes through [`indexed::Indexed::graph_mut`],
    ///   which drops the member index, so without this a `didOpen` of an already-indexed file would
    ///   also cost that index. rubydex's early return cannot help, because the `&mut Graph` is
    ///   already taken.
    /// - **The skip list overrides it.** A document whose last index crashed keeps the text the
    ///   graph accepted *before* the crash, so receiving that text again is a legitimate retry;
    ///   answering yes would keep the file on the skip list forever, out of every batch
    ///   [`Self::without_skipped`] filters.
    /// - **The caller records `indexed_text` either way**: yes means the graph holds these bytes,
    ///   which is what the map means and what [`Self::rebase_for`] needs to refuse an offset
    ///   instead of assuming identity.
    /// - **A hash collision would read two texts as one**, but that is rubydex's exposure, not a
    ///   new one: its own comparison is this comparison.
    fn graph_holds(&self, uri: &DocUri, source: &str) -> bool {
        if self.skipped.contains(uri) {
            return false;
        }
        self.graph
            .documents()
            .get(&UriId::from(uri.as_str()))
            .is_some_and(|document| document.content_hash() == xxh3_64(source.as_bytes()))
    }

    /// How this buffer's offsets relate to the graph's for it.
    ///
    /// `Rebase::identity` wherever the texts are equal, which is every request not answered between
    /// a keystroke and its index. On a document nobody is typing in, this is a length comparison of
    /// two equal strings.
    fn rebase_for(&self, uri: &DocUri, buffer: &str) -> Rebase {
        match self.indexed_text.get(uri) {
            Some(indexed) => Rebase::between(buffer, indexed),
            // Never indexed as a buffer, so the graph holds whatever the disk walk gave it, and
            // nothing here can say how that differs. Identity is the safe assumption, and a
            // deferred answer requires an entry to exist.
            None => Rebase::identity(u32::try_from(buffer.len()).unwrap_or(u32::MAX)),
        }
    }

    // ------------------------------------------------------------------------- the bulkhead

    /// Put one document into the graph, and remember it if that crashes the indexer.
    ///
    /// All five inline routes end here. A crash costs the document its *update* only: the panic is
    /// in the build, so the graph is never entered and its old version keeps answering. So a buffer
    /// being typed into a bad state keeps its previous answers.
    fn index_contained(&mut self, uri: &DocUri, source: &str, language: &LanguageId) -> bool {
        if indexer::index_source(self.graph.graph_mut(), uri.as_str(), source, language) {
            self.unskip(uri);
            let spelled = indexer::spells(source, &self.spelling);
            self.note_spelled(&[(uri.clone(), spelled)]);
            return true;
        }
        self.record_skip(uri);
        false
    }

    /// Record which texts each of these just-indexed documents spells ([`Self::spelled`]).
    fn note_spelled(&mut self, indexed: &[(DocUri, u64)]) {
        for (uri, spelled) in indexed {
            let id = UriId::from(uri.as_str());
            if *spelled == 0 {
                self.spelled.remove(&id);
            } else {
                self.spelled.insert(id, *spelled);
            }
        }
    }

    /// Remember that indexing `uri` crashed, and say so.
    ///
    /// `warn!` every time: a report needs rubydex's file and line (just printed by the default
    /// panic hook) plus the file that provoked it. `showMessage` only the first time, because the
    /// buffer route retries on every keystroke, and a user editing the offending file is owed one
    /// notification, not one per character.
    fn record_skip(&mut self, uri: &DocUri) {
        tracing::warn!("indexing {uri} crashed; leaving that file out of the index");
        if !self.skipped.insert(uri.clone()) {
            return;
        }
        let path = uri
            .to_file_path()
            .unwrap_or_else(|| PathBuf::from(uri.as_str()));
        let message = messages::file_not_indexed(&path);
        self.show_warning(&message);
    }

    /// Forget that it crashed, because it just worked.
    ///
    /// So the skip is not permanent: a user who fixes the file need not restart the server, and
    /// nothing has to decide what counts as a fix.
    fn unskip(&mut self, uri: &DocUri) {
        if self.skipped.remove(uri) {
            tracing::info!("{uri} indexes cleanly again");
        }
    }

    /// The paths in `batch` that did not crash the indexer last time.
    ///
    /// Asked by the two bulk routes, and by none of the four that index because the text may have
    /// moved. `is_empty` first, because this runs on every gem batch and the set is almost always
    /// empty.
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
    /// Shared by `ReloadConfig` (the configuration decides what belongs in the index, so nothing
    /// computed under the old one can be trusted) and by the recovery in [`Analysis::resolve`] (the
    /// graph is in an unknown state, and starting over is the only honest answer).
    fn rebuild(&mut self) {
        self.graph = indexed::Indexed::default();
        // The table is keyed by declarations this graph no longer has, and every signature file is
        // about to be re-read anyway.
        self.types.clear();
        // Same reason: the documents it maps are not in the new graph, and their generators run
        // again as the sources are re-read.
        self.synthesized.clear();
        self.generated.clear();
        // The pass gate has nothing to compare against: its projection is of a graph that no longer
        // exists. The final `mark_dirty` would cover it, but an invariant that depends on a later
        // line is one a later edit can lose.
        self.generated_from = None;
        // The per-document half of the same thing (the gate's evidence and the walk's memo), keyed
        // by `UriId`s of the graph being replaced; every document is about to be re-indexed anyway.
        self.contributions.clear();
        // Every entry describes offsets into the graph being thrown away.
        self.indexed_text.clear();
        self.spelled.clear();
        // Every module keeps its own parse memo. It is keyed by URI with the file's own freshness,
        // so it would survive a rebuild correctly, but a rebuild is a config change that can move
        // the workspace root and so every provenance line. Rebuilding the registry drops them
        // instead of reasoning about it.
        self.knowledge = Self::registered();
        // Anything still queued refers to the old configuration's gem roots and a graph that no
        // longer exists.
        //
        // Back to the entry stage, where `index_workspace` below runs and which
        // `queue_background_indexing` at the end leaves: a rebuild is the pipeline again from the
        // top, not a settle.
        if let Stage::Bundle(work) = std::mem::replace(&mut self.stage, Stage::Workspace)
            && let Some(progress) = work.progress
        {
            progress.end("cancelled".to_owned());
        }
        self.foreign_prefixes.clear();
        self.engine_prefixes.clear();
        self.load_prefixes.clear();
        // The deferred edits. Every open buffer is replayed below (a superset of what was waiting),
        // and the graph they were for is gone.
        self.pending_index.clear();
        // The skip list is deliberately **kept**. Everything above is keyed by the graph being
        // replaced; the skip list is keyed by a file still on disk that still crashes the indexer.
        // The recovery in [`Analysis::resolve`] runs the walk below, so clearing it would let the
        // file this rebuild may be recovering from crash the rebuild too.
        self.index_workspace();
        // Open buffers shadow the disk, so replay them over the freshly indexed tree.
        let buffers: Vec<(DocUri, String)> = self
            .open
            .iter()
            .map(|(uri, document)| (uri.clone(), document.text.text().to_owned()))
            .collect();
        for (uri, text) in buffers {
            self.index_buffer(&uri, &text);
        }
        // **Run the generator stage here, not via the loop.** A rebuild promises the graph comes
        // back, and an unlinked graph is not back: `index_workspace` indexes without resolving,
        // because the pass is written for an unresolved graph. It also always refreshes the editor,
        // even with no buffers to replay: a reload may have changed which rules are on or dropped
        // files from the index, and without a publish a project with no open files would keep the
        // old configuration's diagnostics forever.
        self.generate();
        // The bundle is re-queued *after* that, so it indexes in the background as on a cold start,
        // followed by one more generator stage, instead of blocking the analysis thread for the
        // length of a bundle.
        self.queue_background_indexing();
    }

    /// Replace an open buffer with what is on disk, where the rule in [`Watched`] allows it.
    ///
    /// Returns whether it happened: the caller's "this document was re-indexed".
    ///
    /// **Four conditions, all required:**
    ///
    /// 1. **The event is from the server's own watcher.** Otherwise an editor that reloads
    ///    unmodified files itself would have its next `didChange`'s ranges land in a string it
    ///    never measured.
    /// 2. **The buffer is saved.** Otherwise the swap discards text that exists nowhere else.
    /// 3. **The path is still a file.** A deletion is the loop's third outcome, not this one.
    /// 4. **The text actually differs.** Claude Code's edit arrives twice (`didChange` and
    ///    `didSave`, then the watcher seeing the same write), and the second has nothing to do.
    fn yield_to_disk(&mut self, uri: &DocUri, watched: Watched) -> bool {
        if watched != Watched::ByTheServer {
            return false;
        }
        if !self.open.get(uri).is_some_and(|document| document.saved) {
            return false;
        }
        let Some(path) = uri.to_file_path().filter(|path| path.is_file()) else {
            return false;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                tracing::debug!("{uri} changed on disk but could not be re-read: {error}");
                return false;
            }
        };
        let Some(document) = self.open.get_mut(uri) else {
            return false;
        };
        if document.text.text() == text {
            return false;
        }
        let theirs = std::mem::replace(
            &mut document.text,
            TextDocument::new(text.clone(), self.encoding),
        );
        // Only when empty: a second disk change before the client speaks must not overwrite the
        // client's copy with the first swap's result.
        if document.client_copy.is_none() {
            document.client_copy = Some(theirs);
        }
        tracing::debug!("{uri} changed on disk and its buffer was saved; taking the disk's text");
        // The swap is what this reports, whether or not the graph needed the text: the editor's
        // copy was replaced, which is what the caller counts.
        self.index_buffer(uri, &text);
        true
    }

    /// Bring the index back in line with what is on disk, for paths a watcher named.
    ///
    /// Four rules, each catching a mistake nothing else would:
    ///
    /// - **A buffer beats the disk.** A rebase under an open file must not overwrite what the
    ///   editor shows. `didClose` applies the same precedence the other way: fall back to disk
    ///   only once the buffer is gone.
    /// - **A gem is not the user's code.** Watchers are the client's and shared, so a change in
    ///   a vendored bundle can arrive; `is_own_code` is right even when the bundle is inside the
    ///   workspace root.
    /// - **The walk decides what belongs.** `Workspace::indexes` applies the startup walk's
    ///   rules, so a file the user excluded stays excluded however it is written.
    /// - **`index.max_files` still applies**: pathological repositories exist, and the cap is
    ///   all that stands between one and the process.
    ///
    /// **Three outcomes, not two**: re-index a document, forget one, or neither. A
    /// `db/structure.sql` or a `config/database.yml` is the third. `capabilities::watched_files` watches it (beside
    /// `ya-lsp.toml`, which is watched and never indexed), `synthesize` reads it, and it must
    /// never reach rubydex, which would parse SQL as Ruby. So its branch invalidates and indexes
    /// nothing, and sits **above** the `Workspace::indexes` gate, which is `index.include` (Ruby
    /// shapes) and would always say no.
    fn refresh(&mut self, uris: Vec<DocUri>, watched: Watched) {
        let started = Instant::now();
        let max_files = self.workspace.config().index.max_files;
        let (mut indexed, mut forgotten, mut full) = (0_usize, 0_usize, false);
        let mut invalidated = 0_usize;

        for uri in uris {
            if self.open.contains_key(&uri) {
                // **An open document is decided here and nowhere else in this loop.** Either it
                // yields to the disk (the rule in [`Watched`]) or the buffer stands, since the
                // editor's copy is by definition newer than the disk and is what every answer is
                // computed against.
                if self.yield_to_disk(&uri, watched) {
                    indexed += 1;
                } else {
                    tracing::trace!(
                        "{uri} changed on disk but is open in the editor; keeping the buffer"
                    );
                }
                continue;
            }
            if !self.is_own_code(uri.as_str()) {
                tracing::trace!("{uri} changed, but it is not this project's code to index");
                continue;
            }
            // A file a body of knowledge reads beside the graph (a locale file): the next settle
            // reads it again. A `.rb` one is Ruby too, and goes on to the index below.
            if let Some(path) = uri.to_file_path() {
                let mut claimed = false;
                for module in self.knowledge.modules_mut() {
                    claimed |= module.touched(&path);
                }
                if claimed {
                    self.mark_dirty();
                    invalidated += 1;
                    if !self.workspace.indexes(&path) {
                        continue;
                    }
                }
            }
            // Above the `is_file` test as well as the index gate, so one branch covers a dump being
            // written, edited or deleted: `synthesize` re-reads the directory every settle, so this
            // loop never needs to know which, and a deleted dump is pruned by `forget_stale`.
            let features = self.workspace.features();
            if uri.to_file_path().is_some_and(|path| {
                (features.schema && rails::is_structure(&path))
                    || (features.rails && rails::is_database_config(&path))
            }) {
                tracing::trace!(
                    "{uri} changed; the generators read it, so the next settle re-reads it"
                );
                self.mark_dirty();
                invalidated += 1;
                continue;
            }

            // Gone, or never a file: `didClose`'s test, for the same reason. Whether the workspace
            // *would* index a missing path cannot be asked and need not be: a document the graph
            // holds was indexed, so drop it; one it does not hold is nothing.
            let Some(path) = uri.to_file_path().filter(|path| path.is_file()) else {
                if self.forget_indexed(&uri) {
                    forgotten += 1;
                }
                continue;
            };
            if !self.workspace.indexes(&path) {
                tracing::trace!("{uri} changed but is not a file this workspace indexes");
                continue;
            }
            // A file the walk skipped is not in the graph but was counted against the cap
            // (`index_workspace` counts what discovery found, not what survived), so without this
            // the retry below would count it twice.
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
            // The same entry point as an open buffer, so the `.rbs` interface rule stays one rule:
            // a signature reaching the graph through the watcher must not restore the `Object`
            // members the indexing path removed.
            //
            // Counted on what it did, not what it was asked: a `git checkout` names every file that
            // differs *and* every file merely rewritten, and "re-indexed 200" when three documents
            // changed would hide this working.
            if self.index_buffer(&uri, &text) {
                indexed += 1;
            }
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
    /// One hash of the URI, cheap enough per changed file; asking the workspace walk would cost a
    /// `read_dir` per directory.
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
        self.graph.graph_mut().delete_document(uri.as_str());
        // The map describes offsets into a document that no longer exists.
        self.indexed_text.remove(uri);
        self.spelled.remove(&UriId::from(uri.as_str()));
        // Whatever this file *implied* goes with it. A generated declaration left behind would
        // outlive the only thing that could refresh it: nothing re-reads a deleted file, so a
        // deleted `db/schema.rb`'s columns would answer forever.
        self.synthesized.forget(self.graph.graph_mut(), uri);
        self.mark_dirty();
    }

    /// Mark a document touched when a module reads it only while the editor holds it
    /// ([`Wants::buffers`](crate::knowledge::Wants::buffers)): opening or closing it moves what the
    /// pass reads, though the graph, holding the same text, moved nothing.
    ///
    /// **Reopened, unless an edit touched it**: `index_buffer` runs first, and marks the document
    /// through [`Self::mark_dirty_for`] where the text it was handed differs from the graph's.
    fn touch_if_read_while_open(&mut self, uri: &DocUri) {
        let read = self.generated_from.as_ref().is_some_and(|context| {
            self.knowledge
                .wants()
                .iter()
                .filter(|row| row.buffers)
                .any(|row| {
                    context
                        .documents(row.list)
                        .binary_search_by(|held| held.as_str().cmp(uri.as_str()))
                        .is_ok()
                })
        });
        if read {
            let edited =
                self.touched.contains(uri.as_str()) && !self.reopened.contains(uri.as_str());
            self.mark_dirty_for(uri);
            if !edited {
                self.reopened.insert(uri.as_str().to_owned());
            }
        }
    }

    /// Something changed and the pass cannot know what, so the next pass does all its work.
    fn mark_dirty(&mut self) {
        self.touched_all = true;
        self.dirty = true;
        self.resolve_at = Some(Instant::now() + resolve_debounce());
    }

    /// The same, for the one route that knows which document moved.
    fn mark_dirty_for(&mut self, uri: &DocUri) {
        self.touched.insert(uri.as_str().to_owned());
        // An edit outranks an open: the text this document holds in the graph may have moved.
        self.reopened.remove(uri.as_str());
        self.dirty = true;
        self.resolve_at = Some(Instant::now() + resolve_debounce());
    }

    /// Run the debounced work: link the graph, then push whatever diagnostics changed.
    fn settle(&mut self) {
        // First, and before `dirty` is cleared: a deferred buffer must be in the graph before the
        // pass reads it or the resolver links over it. `index_buffer` sets dirty again, which is
        // why this cannot run after the flag.
        self.index_pending();
        self.resolve_at = None;
        // **Nothing links the graph until the bundle is fully in.**
        //
        // [`Self::regenerate`] is written for an unresolved graph (only `Object`, `BasicObject`,
        // `Module` and `Class`). A settle that linked a half-indexed bundle would make the first
        // pass read a resolved one, and rubydex then adds the same reference to a declaration twice
        // (`model/declaration.rs:160`). A contained panic there rebuilds, which resolves, which
        // panics: a crash loop, and a server answering nothing looks exactly like one thinking.
        //
        // So the whole pipeline is finished here first, on this thread. The cost: a request that
        // settles during a cold start waits for the last gem. The three methods a caret asks are
        // still answered over the map without settling (`defers`), which is where latency is felt.
        //
        // The whole pipeline, not only the bundle: stopping at [`Stage::Generate`] would make this
        // call run the pass, then the loop's next turn step `Generate` and run a *second* one
        // against the graph the first just linked, the same hazard. Draining through `Generate`
        // lets [`Self::generate`] own the pass, and the `dirty` it clears makes the check below
        // fall through instead of running twice.
        if !self.stage.is_ready() {
            while self.step_pipeline() {}
        }
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.regenerate();
        self.refresh_generated();
        self.publish_diagnostics();
    }

    /// Tell the client that a generated document it is reading has changed.
    ///
    /// - **Why.** That document is the one thing on screen nothing watches: no file, so no editor
    ///   reload, no watcher, no `didChange`. Without this, a reader who adds a column and looks
    ///   back at the RBS sees the pre-migration version indefinitely, with no hint it is stale.
    /// - **Drained every settle whether anyone reads or not**, which keeps [`Synthesized`]'s list
    ///   to one pass's worth instead of accumulating a string per body during a cold index.
    /// - **Sent only for documents the client asked about** (see [`Self::served`]). On a model
    ///   keystroke that is one document; on a cold index it would otherwise be every body in the
    ///   workspace, hundreds of requests at the busiest moment, about documents nobody opened.
    fn refresh_generated(&mut self) {
        let changed = self.synthesized.take_changed();
        if !self.client.generated_content || self.served.is_empty() {
            return;
        }
        for uri in changed.iter().filter_map(|uri| self.served.get(uri)) {
            // A string id in the server's own id space, as `refresh_hints` and the two
            // registrations use, one per document: the request names one document, so two changed
            // documents are two requests.
            let _ = self.outgoing.send(Message::Request(Request {
                id: RequestId::from(format!("ya-lsp/generated-refresh/{uri}")),
                method: "workspace/textDocumentContent/refresh".to_owned(),
                params: serde_json::json!({ "uri": uri }),
            }));
        }
    }

    /// Ask the client to show a document to the user.
    ///
    /// All `ya-lsp.showGenerated` does, and why the command exists: a generated document has no
    /// `file:` URI, so no `Location` may name it and no link may point at it
    /// (`DocUri::from_graph_uri` refuses the scheme, the backstop `synthesized.md` is built on). A
    /// command argument is not a `Location`, so the URI reaches the client without weakening that.
    ///
    /// The answer is read and dropped like every server-initiated request: a client that declines
    /// has decided not to open it, and a retry would change nothing.
    fn show_generated(&self, uri: &str) {
        let _ = self.outgoing.send(Message::Request(Request {
            id: RequestId::from(format!("ya-lsp/show-generated/{uri}")),
            method: "window/showDocument".to_owned(),
            params: serde_json::json!({ "uri": uri, "external": false, "takeFocus": true }),
        }));
    }

    /// The name this server's command goes out under, which carries its root.
    ///
    /// Computed from the workspace each time, not held: two hashes a session, on a key the user
    /// pressed, and a cached copy would be a second answer to a question the workspace already
    /// answers. [`show_generated_command`] explains why the root is in it.
    fn show_generated_command(&self) -> String {
        show_generated_command(self.workspace.root())
    }

    /// Remember that the client is reading a generated document, and under what name.
    ///
    /// Called from the content handler, the only place both spellings are known: the key is
    /// this crate's, so [`Self::refresh_generated`] can match what the generators rewrote, and
    /// the value is the client's, so the refresh names a document the client can find.
    fn serving(&mut self, spelling: &str, asked: &str) {
        self.served.insert(spelling.to_owned(), asked.to_owned());
    }

    /// Generate, link, then find the places only a linked graph can name.
    ///
    /// **One function because the three are one order**, and the middle step makes it an order.
    /// Every generator writes text rubydex is about to link, so during the pass the graph holds
    /// four declarations (`Object`, `BasicObject`, `Module`, `Class`), and a generator asking
    /// *where does Rails write `where`* cannot be answered there. It states the name instead, and
    /// [`Self::place_generated_members`] answers it, which is sound only after the resolve.
    ///
    /// Both callers are bulk routes: a workspace's first index and a settle. Neither may run only
    /// two of the three.
    fn regenerate(&mut self) {
        let rewritten = self.synthesize();
        self.resolve();
        self.place_generated_members(rewritten.as_ref());
        // **One more step that needs the resolve first**, for a similar reason:
        // `alias_method :blank?, :empty?` is written in ActiveSupport and `empty?` declared in
        // `vendor/rbs`, so the row a Ruby alias copies exists only once every signature is
        // harvested. See [`Types::adopt_aliases`](types::Types::adopt_aliases).
        self.types.adopt_aliases(&self.graph);
    }

    /// Link the graph, and survive rubydex panicking while it does.
    ///
    /// - **The bug.** rubydex panics inside `Resolver::resolve` after a document is deleted:
    ///   `Graph::delete_document` invalidates first and untracks the document's strings second, so
    ///   queued work can name a string that is gone, and `resolution.rs:748` unwraps it. Reproduced
    ///   by deleting `lib/solargraph/yard_map/to_method.rb` from a solargraph v0.58.2 checkout.
    ///   Reachable through `didClose` on a vanished file, and routine with the watcher on: a
    ///   `git checkout` that removes a file is ordinary.
    /// - **Contained here**, since there is nothing to upgrade to and the alternative is a fork.
    /// - **Uncontained, it is the worst failure this server has**: the analysis thread dies, the
    ///   editor keeps sending requests, and a server answering nothing looks like one thinking. A
    ///   graph half-way through a resolve is in an unknown state, so the only honest recovery is to
    ///   throw it away and index everything again.
    fn resolve(&mut self) {
        let started = Instant::now();
        // The panic message still reaches stderr through the default hook, which prints the rubydex
        // file and line a report needs.
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            crash_the_next_resolve_if_asked();
            Resolver::new(self.graph.graph_mut()).resolve();
            // Inside the seam: it rewrites chains the resolve just wrote, so a graph it broke is
            // the resolve's failure unit too.
            self.graph.repair_superclasses();
        }))
        .is_err();
        if !crashed {
            tracing::debug!("resolved in {:.2?}", started.elapsed());
            return;
        }
        if self.recovering {
            // The rebuild crashed too, so rebuilding again would only crash again. The graph keeps
            // whatever it managed to link; features degrade instead of stopping.
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
    /// Sending every URI every time would be one notification per workspace file per keystroke, so
    /// this diffs: unchanged URIs are skipped, and a URI that dropped out gets an explicit empty
    /// publish, the only way to clear it.
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

        // `current` never holds an empty entry, so the map stays the size of "files with problems",
        // not of the workspace.
        self.published = current;
    }

    /// Every diagnostic the graph holds, grouped by document and converted to LSP.
    ///
    /// - **Two kinds of document are squiggled**: the user's own code, and an open buffer the
    ///   project does not contain (a file beside it, or an unsaved tab). A syntax error is a syntax
    ///   error wherever it is typed; a missing `end` in a scratch buffer deserves red.
    /// - **Widening [`Self::is_own_code`] instead would be the bug.** That test is also asked by
    ///   `rename`, the picker and both ranked surfaces, and widening it would publish diagnostics
    ///   inside gems, offer renames there, and put the bundle's classes in `workspace/symbol`. A
    ///   gem file the reader opens is under a gem root, not outside the project, so it stays
    ///   silent: the fence's asymmetry, unchanged.
    /// - **The rule table keeps this quiet.** Eight of the ten rules ship `Off` or `Hint`, because
    ///   they are rubydex saying *it* gave up; only `parse-error` and `parse-warning` are
    ///   statements about the user's code. See `diagnostics.rs`.
    fn collect_diagnostics(&self) -> HashMap<DocUri, Vec<lsp_types::Diagnostic>> {
        let config = &self.workspace.config().diagnostics;
        // Built once per publish and read once per diagnostic: a handful of open buffers checked
        // against `Layout::is_outside`, instead of parsing a URL for each of a bundle's tens of
        // thousands of diagnostics.
        let layout = self.layout();
        let beside: HashSet<&str> = self
            .open
            .keys()
            .map(DocUri::as_str)
            .filter(|uri| layout.is_outside(uri))
            .collect();

        // Group by the graph's URI string first. Converting to a `DocUri` parses a URL and checks
        // the filesystem, and a file with a hundred parse errors should pay that once.
        let mut by_document: HashMap<
            &str,
            Vec<(&rubydex::diagnostic::Diagnostic, DiagnosticSeverity)>,
        > = HashMap::new();

        for diagnostic in self.graph.all_diagnostics() {
            let rule = *diagnostic.rule();
            let configured =
                config.severity(diagnostics::name(rule), diagnostics::default_severity(rule));
            // `Off` has no LSP spelling: the diagnostic is dropped, not downgraded.
            let Some(severity) = diagnostics::to_lsp_severity(configured) else {
                continue;
            };
            let Some(document) = self.graph.documents().get(diagnostic.uri_id()) else {
                // The document was deleted, but a declaration still carries its diagnostic. There
                // is no file to attach it to.
                continue;
            };
            // Filtered here, on the raw URI string, before grouping: a Rails bundle contributes
            // tens of thousands of diagnostics nobody can act on, and parsing a URL for each on
            // every settle would cost more than the diagnostics.
            if !self.is_own_code(document.uri()) && !beside.contains(document.uri()) {
                continue;
            }
            by_document
                .entry(document.uri())
                .or_default()
                .push((diagnostic, severity));
        }

        let mut collected = HashMap::with_capacity(by_document.len());
        for (raw_uri, entries) in by_document {
            let Some(uri) = DocUri::from_graph_uri(raw_uri) else {
                // rubydex's synthetic built-in document, or anything else without a file.
                continue;
            };
            // A template's diagnostics are not about anything the user wrote. What survives a
            // correct scan is `<%= yield :subnav %>` in a layout: legal, because a compiled Rails
            // template is a method body, but Prism reads a file. A rule that fires on correct input
            // earns no squiggle; see `diagnostics.rs`, and `erb.rs` for the scanners measured
            // against this one.
            //
            // **Keyed on the markup, not the handler, on evidence.** Rails compiles `.jbuilder`,
            // `.builder` and `.ruby` into method bodies just like `.erb`, so this looks like it
            // should cover all four. But across the corpora's such files, no line uses a construct
            // legal only in a compiled template (`yield` is the whole reason for the drop, and a
            // jbuilder has no layout). And the cost runs the other way: a template's Ruby is a view
            // this crate synthesised, while a `.jbuilder` is the user's own Ruby, so a syntax error
            // there is real and earns its squiggle.
            if erb::is_template_uri(&uri) {
                continue;
            }
            // Reading the file to place ranges is affordable only because this runs for files that
            // *have* diagnostics, which on healthy code is none.
            let Some(mut items) = self.with_text(&uri, |text| {
                entries
                    .iter()
                    .map(|(diagnostic, severity)| {
                        let offset = diagnostic.offset();
                        lsp_types::Diagnostic {
                            range: text.range_at(offset.start(), offset.end()),
                            severity: Some(*severity),
                            // The rule name, so the Problems panel shows the key to put in
                            // `[diagnostics.rules]` to change or silence it.
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
                // No readable text means no trustworthy ranges. Publishing nothing beats squiggles
                // in the wrong place.
                tracing::debug!("skipping diagnostics for unreadable {uri}");
                continue;
            };

            // `all_diagnostics` walks hash maps, so its order varies between runs. Sorting makes
            // the set comparable with the last publish, which keeps the diff honest.
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
            // Lets the client discard diagnostics for text the user has edited past.
            version: self.open.get(uri).and_then(|open| open.version),
        };
        let _ = self
            .outgoing
            .send(Message::Notification(lsp_server::Notification::new(
                "textDocument/publishDiagnostics".to_owned(),
                params,
            )));
    }

    /// Whether a document is the user's own code: inside the workspace, outside every gem.
    ///
    /// - **Three features rely on it, for one reason**: a result the user cannot act on is worse
    ///   than none. Nobody fixes a warning in someone's gem or edits a gem to rename their own
    ///   method, and a Rails bundle would bury the answer under tens of thousands of such results.
    /// - **The gem check is not redundant.** A *vendored* bundle lives at
    ///   `vendor/bundle/ruby/<abi>`, inside the root, so the prefix test alone would call a hundred
    ///   gems (and, for a project vendoring its signatures, all of Ruby's core) the user's own
    ///   code.
    /// - **Compared as URI prefixes**: both sides come from `Url::from_file_path`, so they are
    ///   canonical, and this runs once per diagnostic.
    fn is_own_code(&self, uri: &str) -> bool {
        self.layout().is_own(uri)
    }

    /// Whether a generator may read this document: the user's own code, or a Rails engine's.
    ///
    /// - **The one place the answer differs from [`Analysis::is_own_code`].** An engine ships
    ///   models under `app/` and declares `require_paths = ["lib"]`, and its
    ///   `has_many :variant_records` is a member of `ActiveStorage::Blob`, a class applications
    ///   name and chain off that nothing in the bundle's `lib/` declares.
    /// - **Why two predicates.** Widening `is_own_code` would publish diagnostics inside gems,
    ///   offer renames there, and put the bundle's classes in `workspace/symbol`. Those want "code
    ///   the user can act on"; a generator wants "code whose declarations the user can reach".
    fn is_generator_source(&self, uri: &str) -> bool {
        self.is_own_code(uri)
            || self
                .engine_prefixes
                .iter()
                .any(|prefix| uri.starts_with(prefix))
    }

    /// Run `f` over the text of `uri`: the open buffer if the editor has one, otherwise the file on
    /// disk. `None` when there is no readable text.
    ///
    /// The disk read is what rubydex indexed, unless the file changed underneath, in which case a
    /// re-index is on its way and the ranges correct themselves.
    ///
    /// - **A template is handed over blanked.** Nine of the seventeen requests parse the document
    ///   themselves (outline, folds, the scope walk under highlight and rename, semantic tokens,
    ///   the cursor under a typed receiver), and each would otherwise get markup to parse as Ruby.
    ///   This is [`erb::ruby_view`] again, the same bytes rubydex got, and it preserves length and
    ///   line breaks, so offsets agree on both sides. The buffer itself stays exactly what the
    ///   editor sent, since incremental edits apply to it.
    /// - **Blanked for reading, unblanked for addressing**, and getting that wrong is silent. An
    ///   LSP position counts code units in the *client's* text, and blanking replaces a 3-byte `“`
    ///   with three spaces. A column counted against the view is off by (bytes − units) for every
    ///   non-ASCII character earlier in the line's markup. [`TextDocument::blanked`] carries both
    ///   texts for that reason. The cost is one extra read (disk) or clone (buffer), small beside a
    ///   Prism parse.
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
        let text = std::fs::read_to_string(uri.to_file_path()?).ok()?;
        if template {
            let view = erb::ruby_view(&text);
            return Some(f(&TextDocument::blanked(text, view, self.encoding)));
        }
        Some(f(&TextDocument::new(text, self.encoding)))
    }

    /// The document exactly as the editor has it, markup and all.
    ///
    /// The one question [`Self::with_text`] cannot answer, since it removes the markup: whether the
    /// cursor is in Ruby. Only `completion` asks.
    fn with_source<R>(&self, uri: &DocUri, f: impl FnOnce(&str) -> R) -> Option<R> {
        match self.open.get(uri) {
            Some(open) => Some(f(open.text.text())),
            None => Some(f(&std::fs::read_to_string(uri.to_file_path()?).ok()?)),
        }
    }

    /// Another document's text, by the URI rubydex filed it under, and its map onto the graph.
    ///
    /// - **The one non-graph thing [`types`] reads.** It goes through [`Self::with_text`], not
    ///   straight to disk: an open buffer is authoritative, so a controller being edited types its
    ///   template before it is saved.
    /// - **It clones**, unlike every other accessor here: the text must outlive the borrow because
    ///   a parse in another module reads it.
    /// - **The [`Rebase`] finishes the thought.** Preferring the buffer is what lets an unsaved
    ///   controller answer, and also what puts its offsets out of step with the graph once someone
    ///   types. Both come from one `with_text` call, so text and map describe the same string.
    /// - **A closed document read at the version the graph holds is kept across requests**
    ///   ([`types::HeldExits::text`]): an instance variable's read asks every writer document of
    ///   its object, a thousand files under one gem's base class, on every hover.
    fn read_of(&self, uri: &str) -> Option<(Rc<str>, Rebase)> {
        let indexed = self
            .graph
            .documents()
            .get(&UriId::from(uri))
            .map(|document| document.content_hash());
        if let Some(hash) = indexed
            && let Some(held) = self.exits.text(uri, hash)
        {
            return Some(held);
        }
        let document = DocUri::from_graph_uri(uri)?;
        let (text, rebase) = self.with_text(&document, |text| {
            (
                Rc::<str>::from(text.text()),
                self.rebase_for(&document, text.text()),
            )
        })?;
        if let Some(hash) = indexed
            && !self.open.contains_key(&document)
            && xxh3_64(text.as_bytes()) == hash
        {
            self.exits.keep_text(uri, hash, &text, rebase);
        }
        Some((text, rebase))
    }

    /// What the type side reads besides the graph, built per request.
    ///
    /// Per request, not held, because the closure borrows `self` and the flag is configuration a
    /// `workspace/didChangeConfiguration` can change.
    fn sources<'a>(
        &'a self,
        read: &'a types::ReadText<'a>,
        memo: &'a types::Memo<'a>,
    ) -> types::Sources<'a> {
        types::Sources {
            graph: &self.graph,
            types: &self.types,
            read,
            markup: self,
            views: &self.views,
            guess: self.workspace.config().types.guess_from_names,
            features: self.workspace.features(),
            layout: self.layout(),
            memo,
            // Nothing followed yet. The one rung that raises it passes a copy down instead of
            // mutating this, so each request starts at zero.
            constant_hops: 0,
            body_hops: 0,
            // No body is being read yet, so no object's class narrows a read.
            object: None,
            // Nor for a call, so no parameter is bound to what one passed.
            bound: None,
            made: None,
            extended: None,
            // The half of that memo that outlives the request, given to every surface: what a
            // document's `def`s return depends only on its text, and a single cursor re-reads the
            // same unchanged gem file `inlayHint` does.
            held_exits: &self.exits,
            generated: &self.synthesized,
            knowledge: &self.knowledge,
        }
    }

    /// Log which built-in list a replacing `[trees]` key replaced.
    ///
    /// - **Two of the three lists replace instead of extending**, because a wrong name on either
    ///   *deletes* answers. Setting one takes four directory names out of play, and this is the
    ///   only place that shows. `include_is_empty` does the same for `index.include`.
    /// - **A log line, not a `messages::` sentence**: setting these is legitimate, and a
    ///   `window/showMessage` every session about an intended setting is nagging. `test_support` is
    ///   additive, replaces nothing, so there is nothing to say.
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

    /// A misspelled key in `[diagnostics.rules]` silently does nothing, forever. Say so.
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
        // A send failure means the client is gone; the main loop is already shutting down.
        let _ = self.outgoing.send(Message::Response(response));
    }

    /// Ask the client to redraw the margin.
    ///
    /// A server-initiated request, like `client/registerCapability`: the main loop reads and drops
    /// the answer. The id is a string in the server's own id space, so it cannot collide with a
    /// client id.
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

    /// Ask the client to claim files this server answers about that its own selector did not.
    ///
    /// - **Why.** A gem's source, Ruby's stdlib and their RBS live outside every workspace folder,
    ///   and the server indexes all three. But a document selector is the only gate on what the
    ///   client sends, so without this, `definition` jumps into a gem and every request in that
    ///   file is dead, with nothing logged because nothing was asked.
    /// - **Sent from here because the answer exists here.** The prefixes are the gem roots the
    ///   bundle resolved to, which the handshake does not know: discovery runs on this thread.
    ///   `client/registerCapability` is the same channel the file watcher uses. The client's
    ///   `didOpen` registration **back-fills**, sending one for every open file the new selector
    ///   matches, so a gem file already open starts answering at registration.
    /// - **Additive**: a client that declines keeps exactly what it has.
    fn register_documents(&mut self, held: &[&PathBuf]) {
        // **Not `foreign_prefixes`**, a superset built for another question (see the list passed in
        // `queue_background_indexing`).
        //
        // The workspace's own prefix is filtered out too. A vendored bundle at
        // `vendor/bundle/ruby/<abi>` and a project's `.gem_rbs_collection/` are foreign *and*
        // inside the root, so the client's original selector already claims them. Claiming twice
        // adds nothing but a second provider on one document: the same hover answered twice.
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
        // A string id in the server's own id space, as `register_file_watchers` and `refresh_hints`
        // use. The main loop reads the answer and logs a refusal; nothing else to do either way.
        let _ = self.outgoing.send(Message::Request(Request {
            id: RequestId::from("ya-lsp/document-registration".to_owned()),
            method: "client/registerCapability".to_owned(),
            params: serde_json::json!({ "registrations": registrations }),
        }));
    }

    /// Take back the live document registrations, by the names they were made under.
    ///
    /// Nothing to do at startup. It exists for reloads: `[gems] enabled`, an `[rbs] path` or a new
    /// root all change which files the server answers about, and re-registering an id the client
    /// holds replaces its *record* without disposing the old provider. The old selector would keep
    /// answering beside the new one, the duplicate `register_documents`' filter avoids, by another
    /// route.
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

/// The run loop itself, driven over the real channel on the real thread.
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
    fn each_goto_reads_its_own_link_support_and_never_a_neighbours() {
        // Claude Code's `initialize`, as the 2.1.274 binary sends it: link support on `definition`,
        // no `implementation` capability. Each goto has its own flag, so the negotiated shape is
        // `LocationLink[]` for one and `Location[]` for the other; reading one flag for both would
        // send an agent a shape it never asked for.
        let claude_code: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "textDocument": {
                "synchronization": { "dynamicRegistration": false, "didSave": true },
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "definition": { "linkSupport": true },
                "references": {},
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                "callHierarchy": {},
            },
        }))
        .expect("capabilities");

        let negotiated = ClientSupport::negotiate(&claude_code, &serde_json::Value::Null);
        assert!(negotiated.definition_links);
        assert!(!negotiated.implementation_links);
        assert!(!negotiated.type_definition_links);
        assert!(!negotiated.declaration_links);
        assert!(negotiated.hierarchical_symbols);

        // The other direction, one goto at a time, so no flag is hard-coded `false` or copied from
        // a neighbour. Four capabilities, four answers, and each case asserts the other three stay
        // off.
        for (capability, definition, implementation, type_definition, declaration) in [
            ("definition", true, false, false, false),
            ("implementation", false, true, false, false),
            ("typeDefinition", false, false, true, false),
            ("declaration", false, false, false, true),
        ] {
            let one: ClientCapabilities = serde_json::from_value(serde_json::json!({
                "textDocument": { capability: { "linkSupport": true } },
            }))
            .expect("capabilities");
            let negotiated = ClientSupport::negotiate(&one, &serde_json::Value::Null);
            assert_eq!(negotiated.definition_links, definition, "{capability}");
            assert_eq!(
                negotiated.implementation_links, implementation,
                "{capability}"
            );
            assert_eq!(
                negotiated.type_definition_links, type_definition,
                "{capability}"
            );
            assert_eq!(negotiated.declaration_links, declaration, "{capability}");
        }
    }

    #[test]
    fn the_two_capabilities_the_generated_document_needs_are_read_separately_and_one_has_no_name() {
        // `workspace.textDocumentContent` has no field in `lsp-types` 0.97 (the gap
        // `server::capabilities::Advertised` works around the other way), so it is read from the
        // raw object beside the typed one. This tests that both reads are of the same client.
        let raw = serde_json::json!({
            "window": { "showDocument": { "support": true } },
            "workspace": { "textDocumentContent": { "dynamicRegistration": true } },
        });
        let typed: ClientCapabilities = serde_json::from_value(raw.clone()).expect("capabilities");
        let negotiated = ClientSupport::negotiate(&typed, &raw);
        assert!(negotiated.show_document);
        assert!(negotiated.generated_content);

        // Neovim's shape, and why the second flag exists: it will show a document but has nothing
        // to fill one, so the two flags must not be read off each other.
        let raw = serde_json::json!({ "window": { "showDocument": { "support": true } } });
        let typed: ClientCapabilities = serde_json::from_value(raw.clone()).expect("capabilities");
        let negotiated = ClientSupport::negotiate(&typed, &raw);
        assert!(negotiated.show_document);
        assert!(!negotiated.generated_content);

        // The object's presence, not a field: its only member says how a provider may be
        // *registered*, not whether the request is answered.
        let raw = serde_json::json!({ "workspace": { "textDocumentContent": {} } });
        let typed: ClientCapabilities = serde_json::from_value(raw.clone()).expect("capabilities");
        assert!(ClientSupport::negotiate(&typed, &raw).generated_content);

        // A client that sends neither, as every client did before 3.18.
        let raw = serde_json::json!({ "workspace": { "textDocumentContent": null } });
        let typed: ClientCapabilities = serde_json::from_value(raw.clone()).expect("capabilities");
        let negotiated = ClientSupport::negotiate(&typed, &raw);
        assert!(!negotiated.show_document);
        assert!(!negotiated.generated_content);
    }

    #[test]
    fn indexes_the_workspace_on_startup() {
        let mut harness = Harness::new();
        harness.write("lib/person.rb", "class Person\n  def shout\n  end\nend\n");

        harness.index();

        assert!(harness.has("Person"));
        assert!(harness.has("Person#shout()"));
        assert_eq!(harness.document_count(), 1);
    }

    #[test]
    fn editing_a_buffer_replaces_declarations_without_leaking_the_old_ones() {
        // Re-indexing the same URI must not accumulate.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();
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

        std::fs::remove_file(uri.to_file_path().unwrap()).unwrap();
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
    fn a_watched_change_to_a_file_that_is_not_the_project_s_own_is_not_indexed() {
        // The `refresh` loop's second gate, which a bundle makes necessary: a client's watchers are
        // shared across every server and registration, so gem changes arrive routinely. Re-indexing
        // would be work nobody asked for: the bundle is read once at startup, and a gem file does
        // not change under a running editor unless the user is editing the gem.
        let (dir, gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty\n");
        harness.index();
        harness.index_gems();
        assert!(harness.has("Shouty::Megaphone"));

        let gem = DocUri::from_path(&gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .expect("an absolute path");
        std::fs::write(
            gem.to_file_path().expect("a path"),
            "module Shouty\n  class Loudhailer\n  end\nend\n",
        )
        .unwrap();
        harness.watch(&[&gem]);

        assert!(!harness.has("Shouty::Loudhailer"));
        assert!(
            harness.has("Shouty::Megaphone"),
            "the graph still holds what the startup walk read"
        );
    }

    #[test]
    fn a_change_for_an_unsaved_buffer_nobody_opened_is_not_a_way_in() {
        // The recovery above invents an open document from a whole-buffer change, the only way a
        // fileless document would be indexed. Which unsaved buffers are indexed is decided by
        // `didOpen`'s `languageId` alone (no other notification has one), so a buffer refused there
        // must not get in here. A shopping list in a new tab is the case.
        let mut harness = Harness::new();
        let untitled = DocUri::from_lsp(&"untitled:Untitled-9".parse().expect("a uri"))
            .expect("an unsaved buffer is a document");
        harness.analysis.index_workspace();

        harness.change(&untitled, "class Recovered\nend\n");

        assert!(!harness.has("Recovered"));
        assert!(!harness.analysis.open.contains_key(&untitled));
    }

    #[test]
    fn an_edit_reaches_the_buffer_at_once_and_the_graph_at_the_settle() {
        // `didChange` does two things and only the second is expensive: apply the edit
        // (microseconds), and index the document into rubydex (hundreds of milliseconds for a
        // central model on a large app). The four requests `needs_the_graph` exempts read only the
        // buffer, and editors send `semanticTokens/full` after every keystroke, so they must not
        // wait on an index they do not read.
        //
        // `handle`, not `Harness::run`: `run` settles whenever the task left anything dirty, which
        // is right for every other test here and exactly what this one tests, so the task goes in
        // alone.
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
        // `definitions_in`, not `has`: a declaration belongs to the resolver and would be absent
        // here whether or not the document was indexed.
        assert_eq!(
            harness.definitions_in(&uri),
            1,
            "the index was not deferred"
        );

        // A second keystroke before anything asked about the first: both are deferred and the
        // document recorded once, so a burst costs one index, not one per character.
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

        // The claim is about `serve`, not the handler: a request that never asks the graph is
        // answered without settling, so the index stays deferred across it.
        harness.ask(
            "textDocument/foldingRange",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );
        assert_eq!(
            harness.definitions_in(&uri),
            1,
            "a request that never reads the graph settled anyway"
        );

        // The other half, which makes deferral safe, not just cheap: anything that reads the graph
        // settles, and `settle` indexes pending edits before linking anything over them.
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
        // The one ordering a deferred index adds: an edit is deferred, then the buffer closes
        // before the settle that would index it. `didClose` has already restored the file to what
        // disk says, so replaying the buffer would undo that, with a version of the file the user
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

    #[test]
    fn a_didopen_of_the_text_the_graph_already_holds_indexes_nothing() {
        // The commonest thing an editor does: the walk indexed every file at startup, then someone
        // opens one. rubydex compares `Document::content_hash`, finds it equal and returns, but
        // only after a full Prism parse, and only after `graph_mut` has dropped the member index
        // `locator`'s name rung reads.
        let mut harness = Harness::new();
        let source = "class Story\n  def title\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.analysis.settle();

        // Ask the name rung something, so there is an index to lose.
        assert_eq!(harness.analysis.graph.members_named("title()").len(), 1);

        harness.analysis.handle(Task::DidOpen {
            uri: uri.clone(),
            text: source.to_owned(),
            version: Some(1),
        });

        assert!(
            harness.analysis.graph.members_are_built(),
            "opening a file the walk already indexed dropped the member index"
        );
        assert!(
            !harness.analysis.dirty,
            "and armed a settle over a graph nothing had changed in"
        );
    }

    #[test]
    fn a_didopen_that_indexes_nothing_still_records_what_the_graph_holds() {
        // `rebase_for` translates a deferred answer through `indexed_text`, and a document with no
        // entry is assumed to be at identity. The walk leaves no entry (it indexes paths, not
        // buffers), so a skip that also skipped the entry would hand the next keystroke's offsets
        // to a graph one edit behind, with no refusal. A skip states that the graph holds exactly
        // these bytes, which is what the map means.
        let mut harness = Harness::new();
        let source = "class Story\n  def title\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.analysis.settle();
        assert!(
            !harness.analysis.indexed_text.contains_key(&uri),
            "the walk indexes paths, so there is nothing to rebase through yet"
        );

        harness.open(&uri, source);

        assert_eq!(
            harness.analysis.indexed_text.get(&uri).map(String::as_str),
            Some(source),
            "an index that was skipped left the document with no map"
        );
    }

    #[test]
    fn a_template_the_graph_already_holds_the_view_of_indexes_nothing() {
        // The same question, asked of the text rubydex was actually given, which for a template is
        // `erb::ruby_view`, not the buffer. Comparing the buffer would answer no for every
        // template.
        let mut harness = Harness::new();
        let source = "<h1><%= @story.title %></h1>\n";
        let uri = harness.write("app/views/stories/show.html.erb", source);
        harness.index();
        harness.analysis.settle();

        harness.analysis.handle(Task::DidOpen {
            uri: uri.clone(),
            text: source.to_owned(),
            version: Some(1),
        });

        assert!(
            !harness.analysis.dirty,
            "a template was re-indexed over the view the graph already held"
        );
        assert_eq!(
            harness.analysis.indexed_text.get(&uri).map(String::as_str),
            Some(erb::ruby_view(source).as_str()),
            "and the map records the view rather than the buffer"
        );
    }

    #[test]
    fn a_document_on_the_skip_list_is_handed_its_text_again_rather_than_left_there() {
        // The one case where the graph already holding these bytes is no reason to do nothing. The
        // last index of this document crashed, so the graph holds the version from *before* the
        // crash, and receiving that version again is a retry. Answering "already held" would keep
        // the file on the skip list forever, out of every batch `without_skipped` filters.
        let mut harness = Harness::new();
        let source = "class Story\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.analysis.settle();
        assert!(!harness.analysis.skipped.contains(&uri));

        // An edit the indexer cannot take. The graph keeps its previous version.
        harness.run(Task::DidOpen {
            uri: uri.clone(),
            text: format!("{source}{}", indexer::CRASHES),
            version: Some(1),
        });
        assert!(
            harness.analysis.skipped.contains(&uri),
            "the stand-in never reached the bulkhead"
        );

        // Closing restores the file to what disk says, which is exactly what the graph holds.
        harness.run(Task::DidClose { uri: uri.clone() });

        assert!(
            !harness.analysis.skipped.contains(&uri),
            "a document whose text the graph already held was never retried"
        );
    }

    #[test]
    fn a_file_rewritten_on_disk_with_the_text_it_had_is_not_indexed_again() {
        // What a `git checkout` between branches does: it names every file it wrote, and rewrote
        // most of them byte for byte. The watcher cannot tell those apart and should not try; the
        // hash does it, for the client's watcher and ya-lsp's alike.
        let mut harness = Harness::new();
        let source = "class Story\n  def title\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.analysis.settle();
        assert_eq!(harness.analysis.graph.members_named("title()").len(), 1);

        harness.write("app/models/story.rb", source);
        harness.watch(&[&uri]);

        assert!(
            harness.analysis.graph.members_are_built(),
            "a file rewritten with its own text cost the member index"
        );
        assert!(
            harness.has("Story#title()"),
            "and the document is still in the graph"
        );
    }

    // -----------------------------------------------------------------------
    // The index and the disk
    // -----------------------------------------------------------------------

    #[test]
    fn a_file_written_while_the_server_runs_is_indexed_without_the_editor_opening_it() {
        // `git checkout`, `git pull`, a rebase, `rails g model`: each writes Ruby the editor never
        // opened, which the index must pick up without a restart.
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
        // The branch-switch case: same path, different declarations. Keeping the old ones is worse
        // than not noticing, because navigation lands somewhere that is gone.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();
        assert!(harness.has("Person#shout()"));

        std::fs::write(
            uri.to_file_path().unwrap(),
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
        // A deletion is the one change no other notification can replace: nothing else tells a
        // server a declaration is gone. And `publishDiagnostics` is stateful per URI, so a file
        // that vanishes with squiggles keeps them forever unless an empty set is sent.
        let mut harness = Harness::new();
        let uri = harness.write("app/broken.rb", "class Broken\n  def oops(\nend\n");
        harness.index();
        assert!(harness.has("Broken"));
        assert!(!harness.latest(&uri).unwrap_or_default().is_empty());

        std::fs::remove_file(uri.to_file_path().unwrap()).unwrap();
        harness.watch(&[&uri]);

        assert!(!harness.has("Broken"));
        assert_eq!(harness.document_count(), 0);
        assert_eq!(
            harness.latest(&uri),
            Some(Vec::new()),
            "an explicit empty publish is the only thing that clears a squiggle"
        );
    }

    /// The disk's text for the fixture below, and the buffer's.
    const ON_DISK: &str = "class Person\n  def shout\n  end\nend\n";

    /// A project with one file, open in a client that has just read it from disk.
    ///
    /// `didOpen` carries exactly what is on disk, which makes the buffer *saved*, the condition the
    /// swap depends on. Each test below changes one thing.
    fn a_saved_buffer() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", ON_DISK);
        harness.index();
        harness.open(&uri, ON_DISK);
        (harness, uri)
    }

    fn rewrite(uri: &DocUri, text: &str) {
        std::fs::write(uri.to_file_path().expect("a path"), text).expect("the file");
    }

    fn buffer(harness: &Harness, uri: &DocUri) -> String {
        harness
            .analysis
            .open
            .get(uri)
            .expect("an open document")
            .text
            .text()
            .to_owned()
    }

    #[test]
    fn a_saved_buffer_yields_to_the_disk_when_the_server_is_the_one_watching() {
        // **The gap this closes.** An agent queries a file through its LSP tool, edits it through a
        // shell, and queries again, while the document stays open (Claude Code keeps up to fifty
        // open and never reopens them). Without this, the second answer uses the file's text from
        // when it was opened, for the whole session: confidently wrong, not absent.
        let (mut harness, uri) = a_saved_buffer();
        rewrite(&uri, "class Person\n  def from_disk\n  end\nend\n");

        harness.watched(&[&uri], Watched::ByTheServer);

        assert!(
            harness.has("Person#from_disk()"),
            "a saved buffer did not follow the file it is a copy of"
        );
        assert!(!harness.has("Person#shout()"));
    }

    #[test]
    fn a_buffer_with_unsaved_changes_keeps_its_text_until_it_is_saved() {
        // The other half, which makes the rule safe: a buffer changed since its last save holds
        // text that exists nowhere else, and no watcher may discard it. A `didSave` hands it back
        // to the disk; Claude Code sends one right after every edit.
        let (mut harness, uri) = a_saved_buffer();
        harness.change(&uri, "class Person\n  def unsaved\n  end\nend\n");
        rewrite(&uri, "class Person\n  def from_disk\n  end\nend\n");

        harness.watched(&[&uri], Watched::ByTheServer);

        assert!(
            harness.has("Person#unsaved()"),
            "text the editor has not written anywhere must survive a change on disk"
        );
        assert!(!harness.has("Person#from_disk()"));

        harness.save(&uri);
        rewrite(&uri, "class Person\n  def later\n  end\nend\n");
        harness.watched(&[&uri], Watched::ByTheServer);

        assert!(
            harness.has("Person#later()"),
            "a saved buffer is a copy of the disk again, whatever it held before"
        );
    }

    #[test]
    fn a_clients_own_watcher_never_replaces_a_buffer_however_saved_it_is() {
        // **The VS Code guarantee, as a test.** An editor holding the registration reloads an
        // unmodified file itself and then sends a `didChange` with ranges measured against *its*
        // text. Swapping under it corrupts the buffer, so the rule depends on which watcher saw the
        // change, never on the buffer alone.
        let (mut harness, uri) = a_saved_buffer();
        rewrite(&uri, "class Person\n  def from_disk\n  end\nend\n");

        harness.watched(&[&uri], Watched::ByTheClient);

        assert!(harness.has("Person#shout()"), "the buffer still stands");
        assert!(!harness.has("Person#from_disk()"));
    }

    #[test]
    fn a_ranged_change_after_a_swap_lands_in_the_text_the_client_measured_it_against() {
        // The corruption `client_copy` prevents. The server swapped the buffer for the disk's text;
        // the client, unaware, sends ranges computed against its own copy. Applied to the disk's
        // text, `1:6-1:11` covers `from_` and the file becomes `def yelldisk`, which parses,
        // indexes, and is wrong.
        let (mut harness, uri) = a_saved_buffer();
        rewrite(&uri, "class Person\n  def from_disk\n  end\nend\n");
        harness.watched(&[&uri], Watched::ByTheServer);
        assert!(harness.has("Person#from_disk()"), "the swap happened");

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
                text: "yell".to_owned(),
            }],
        );

        assert_eq!(
            buffer(&harness, &uri),
            "class Person\n  def yell\n  end\nend\n",
            "the change was applied to the disk's text instead of the client's"
        );
        assert!(harness.has("Person#yell()"));
    }

    #[test]
    fn a_write_the_buffer_already_holds_costs_nothing_and_keeps_no_copy() {
        // Claude Code's edit arrives twice: `didChange`, `didSave`, then the watcher seeing the
        // same bytes. The second has nothing to do, and comparing is the cheapest way to know. A
        // swap here would leave a `client_copy` for a change that never happened, and the next
        // ranged edit would apply to a stale string.
        let (mut harness, uri) = a_saved_buffer();
        let edited = "class Person\n  def renamed\n  end\nend\n";
        harness.change(&uri, edited);
        harness.save(&uri);
        rewrite(&uri, edited);

        harness.watched(&[&uri], Watched::ByTheServer);

        assert!(harness.has("Person#renamed()"));
        assert!(
            harness
                .analysis
                .open
                .get(&uri)
                .expect("an open document")
                .client_copy
                .is_none(),
            "the disk and the buffer already agreed; nothing was swapped"
        );
    }

    #[test]
    fn a_deletion_for_something_the_index_never_held_costs_nothing() {
        // Watchers are the client's, so a delete can name a path this server never indexed, and
        // `Workspace::indexes` cannot be asked about a missing path. The graph knows, and answers
        // both halves of the question at once.
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
        // A rebase under an open file must not overwrite what the editor shows: the buffer holds
        // edits the disk has never seen, and the editor is the authority until it says otherwise.
        // `didClose` applies the same precedence the other way, handing authority to the disk when
        // the buffer goes.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();
        harness.open(&uri, "class Person\n  def unsaved\n  end\nend\n");
        assert!(harness.has("Person#unsaved()"));

        std::fs::write(
            uri.to_file_path().unwrap(),
            "class Person\n  def from_disk\n  end\nend\n",
        )
        .unwrap();
        harness.watch(&[&uri]);

        assert!(
            harness.has("Person#unsaved()"),
            "the buffer the editor is showing must survive the disk change"
        );
        assert!(!harness.has("Person#from_disk()"));

        // Once the buffer goes, disk is the truth again, through the existing path.
        harness.run(Task::DidClose { uri });
        assert!(harness.has("Person#from_disk()"));
    }

    #[test]
    fn every_notification_says_it_arrived_and_what_it_was_about() {
        // A notification changes every later answer without saying so. The explanation for an
        // answer someone is about to report as wrong is often here (a dropped `didChange`, a reload
        // that threw the graph away), so it is logged.
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
        // "It is reading my ya-lsp.toml" and "it is reading my ya-lsp.toml and every key is already
        // the default" would behave and log identically, and the second is what someone who
        // mistyped a table name has.
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
        // `rails` is listed too, because the harness sends `rails.enabled = true` where the default
        // is `auto`: that *is* a setting somebody set.
        assert!(logged.contains("types"), "{logged}");

        // A file that only repeats the defaults does not appear on it.
        std::fs::write(
            harness.root.path().join("ya-lsp.toml"),
            "[types]\nguess_from_names = true\n",
        )
        .unwrap();
        let (_, logged) = crate::testing::captured_logs(tracing::Level::INFO, || {
            harness.run(Task::ReloadConfig);
        });
        assert!(logged.contains("reloaded configuration"), "{logged}");
        // The detection logs its result **after** the log is re-pointed, which is why that line is
        // not where the decision is made: a reload that just turned the file log on must record the
        // reload that turned it on.
        assert!(logged.contains("rails knowledge"), "{logged}");
        assert!(
            !logged.contains("types"),
            "a value equal to the default is not a change: {logged}"
        );
    }

    #[test]
    fn a_watched_change_the_workspace_does_not_index_is_ignored() {
        // A client's watchers are shared across every server and registration, so anything can
        // arrive. Indexing it would put files `index.exclude` rules out into the graph, and the
        // walk and this path disagreeing is a failure neither the user nor the log would ever show.
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
        // The gap between `is_file` and reading it: a half-written file mid-checkout, a permission,
        // or (portably testable) non-UTF-8 bytes. Dropping the document is the worse answer: the
        // old declarations were true a moment ago, and the next write brings another notification.
        let mut harness = Harness::new();
        let uri = harness.write("app/person.rb", "class Person\n  def shout\n  end\nend\n");
        harness.index();

        std::fs::write(
            uri.to_file_path().unwrap(),
            b"class Person\n  def \xff\nend\n",
        )
        .unwrap();
        let (_, logged) = crate::testing::captured_logs(tracing::Level::WARN, || {
            harness.watch(&[&uri]);
        });

        assert!(harness.has("Person#shout()"), "the last good index is kept");
        assert!(logged.contains("after a watched change"), "{logged}");
    }

    #[test]
    fn a_crash_while_linking_the_graph_rebuilds_instead_of_killing_the_server() {
        // rubydex panics in `Resolver::resolve` after a document is deleted, with no fixed release
        // to upgrade to. Uncaught, the analysis thread dies and the server answers nothing forever,
        // which looks like a server thinking, so nobody restarts it. Reproduced by deleting one
        // file from a solargraph v0.58.2 checkout; this pins that ya-lsp recovers.
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
        // that cannot be linked would rebuild forever and never answer. Degrading to whatever was
        // linked is the worse index and the better server.
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
    fn a_crash_in_the_generator_stage_does_not_spin_the_pipeline() {
        // **The hazard is the driver, not the crash.** `serve`'s cold rung and the harness both run
        // `while step_pipeline() {}`, and `rebuild` re-enters the pipeline from the top, so a stage
        // whose work rebuilds could re-arm itself and the loop would never end. A hung language
        // server is the worst failure, because it looks like one thinking.
        //
        // What bounds it: **`rebuild` finishes its own pipeline**. It runs the generator stage
        // itself and leaves `Ready` (or `Bundle`, which only moves forward), never `Generate`.
        // `recovering` bounds the crashing, this bounds the stepping; neither covers the other.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");

        RESOLVES_TO_CRASH.set(5);
        let (_, logged) = crate::testing::captured_logs(tracing::Level::ERROR, || harness.index());

        assert!(logged.contains("crashed again during recovery"), "{logged}");
        assert!(
            harness.analysis.stage.is_ready(),
            "the pipeline has to have finished, or its driver is still stepping it"
        );
        assert_eq!(
            RESOLVES_TO_CRASH.replace(0),
            3,
            "two resolves, then the guard"
        );
    }

    // -----------------------------------------------------------------------
    // The bulkhead
    // -----------------------------------------------------------------------

    /// The same bug through a keystroke, where it is inline on the analysis thread with no worker
    /// under it.
    ///
    /// Not a startup problem: the file need never be on disk, and a user meets this half while
    /// working. Pinned: the rest of the workspace keeps answering, and *this* document keeps what
    /// it had before the edit instead of emptying.
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

    /// The buffer route retries on every keystroke, which makes the skip list self-clearing, and
    /// would mean a notification per character if `record_skip` did not speak only once.
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

    /// The one thing `rebuild` does not clear, because of the recovery it belongs to.
    ///
    /// `resolve`'s crash recovery rebuilds, and a rebuild runs the workspace walk, so a workspace
    /// holding a file that crashes the indexer would take the recovery down with it if the skip
    /// list were cleared like the caches above it.
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

    /// The other half of "not permanent": nothing decides what counts as a fix.
    ///
    /// Every route that indexes because the text may have moved just tries again, so the entry goes
    /// once the file works, without a server restart.
    #[test]
    fn a_file_that_is_fixed_is_indexed_again_without_a_restart() {
        let mut harness = Harness::new();
        let bad = harness.write("app/rice.rb", indexer::CRASHES);
        harness.index();
        assert!(harness.analysis.skipped.contains(&bad));

        std::fs::write(
            bad.to_file_path().unwrap(),
            "class Rice\n  def cook\n  end\nend\n",
        )
        .unwrap();
        harness.watch(&[&bad]);

        assert!(harness.has("Rice#cook()"));
        assert!(harness.analysis.skipped.is_empty());
    }

    /// A gem's files take the other bulk route, and the files that provoke this are more often in
    /// gems than in applications.
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
        // `bundle install` rewrites tens of thousands of files at once. Answering on the analysis
        // thread, one `index_source` at a time, is the stall the background gem index avoids. So a
        // gem is not the user's code even when the include globs would take it, as for a bundle
        // vendored outside `vendor/`.
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
        // `index.max_files` exists because pathological repositories exist, and a watcher can add
        // files the walk stopped before. Said once, not per branch switch: the condition does not
        // change between them, and a notification that repeats forever gets dismissed unread.
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

        // The budget is a count, not a high-water mark: deleting the file that filled it makes room
        // for the next.
        std::fs::remove_file(harness.root.path().join("app/person.rb")).unwrap();
        let person = DocUri::from_path(&harness.root.path().join("app/person.rb")).unwrap();
        harness.watch(&[&person, &second]);
        assert!(!harness.has("Person"));
        assert!(harness.has("Place"), "the freed slot is usable");
    }

    #[test]
    fn a_settings_change_takes_effect_without_a_restart_and_the_file_still_wins() {
        // The editor's settings are a layer, not the truth: `ya-lsp.toml` is committed so a whole
        // team behaves the same in any editor, so it must keep outranking one person's preferences.
        let mut harness = Harness::new();
        harness.write("lib/person.rb", "class Person\nend\n");
        harness.write("spec/person_spec.rb", "class PersonSpec\nend\n");
        harness.index();
        assert!(harness.has("PersonSpec"), "indexed by default");

        harness.run(Task::ChangeConfig {
            options: Some(serde_json::json!({ "index": { "exclude": ["spec/**/*"] } })),
        });
        assert!(!harness.has("PersonSpec"), "the client's settings applied");

        // A project file that says something different wins, and keeps winning across a later
        // settings change that does not mention the key.
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

        // Dropping the layer entirely returns to the server's defaults.
        harness.run(Task::ChangeConfig { options: None });
        assert!(harness.has("PersonSpec"));
    }

    #[test]
    fn config_reload_reindexes_and_replays_open_buffers() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", "class Person\nend\n");
        harness.write("spec/person_spec.rb", "class PersonSpec\nend\n");
        harness.index();
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
        // Prism also emits an indentation warning here, which must arrive under its own rule so
        // `[diagnostics.rules]` can silence it separately.
        assert!(
            items.iter().any(|item| item.code == code("parse-warning")
                && item.severity == Some(DiagnosticSeverity::WARNING)),
            "{items:?}"
        );
        // Sorted by position, so the first diagnostic is at the top of the file.
        assert_eq!(items[0].range.start, lsp_types::Position::new(0, 0));
    }

    #[test]
    fn fixing_the_file_publishes_an_empty_set_rather_than_going_quiet() {
        // Guards the classic failure: diagnostics that never clear. LSP keeps the last set sent for
        // a URI on screen forever, so silence is not a retraction.
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
        // Otherwise every keystroke anywhere re-sends diagnostics for every file that has any,
        // which is why the publisher diffs.
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
        // Resolution diagnostics hang off declarations, not documents, so the `uri_id` lookup is
        // what puts them in the right file. The rule ships off (it fires on correct Ruby), so this
        // turns it on, which also exercises the config path.
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
        // rubydex gives UTF-8 byte offsets and `Offset::to_location` only returns UTF-8 columns, so
        // a diagnostic *after* a wide character is where a naive mapping lands in the wrong place.
        // Two emoji are 8 UTF-8 bytes but 4 UTF-16 units.
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
        // `  "` is 3 units, then the emoji (4 UTF-16 vs 8 UTF-8), then `"; ` is 3 more.
        assert_eq!(wide.character, 10, "utf-16 column");
        assert_eq!(narrow.character, 14, "utf-8 column");
    }

    #[test]
    fn a_buffer_the_project_does_not_contain_is_squiggled_and_a_gem_is_not() {
        // **Both halves of the diagnostics clause, the one place the outward fence is deliberately
        // open.** A syntax error is one wherever it is typed, and a missing `end` in a file beside
        // the project deserves red. The exception is *an open buffer outside the project*, not
        // *code that is not the user's own*: a gem is a full member of the project, not outside it,
        // so it stays silent.
        let mut harness = Harness::new();
        harness.index();

        let outside = tempfile::tempdir().expect("tempdir");
        let path = outside.path().join("scratch_pad.rb");
        std::fs::write(&path, UNTERMINATED).unwrap();
        let uri = DocUri::from_path(&path).unwrap();

        harness.open(&uri, UNTERMINATED);
        let published = harness.latest(&uri).expect("the buffer is squiggled");
        assert!(!published.is_empty());
        assert!(
            published.iter().all(|item| matches!(
                item.code.as_ref(),
                Some(lsp_types::NumberOrString::String(rule))
                    if rule == "parse-error" || rule == "parse-warning"
            )),
            "only the two rules that are statements about the user's own code reach an outside \
             buffer, because the other eight ship `Off` or `Hint`: {published:?}"
        );
        assert!(
            published
                .iter()
                .any(|item| item.severity == Some(lsp_types::DiagnosticSeverity::ERROR)),
            "{published:?}"
        );

        // **Open, not merely indexed.** After a close the file is still on disk and in the graph
        // (`didClose` re-reads it), so the empty publish shows the clause is read as written, not
        // as "anything in the graph outside the project".
        harness.run(Task::DidClose { uri: uri.clone() });
        assert_eq!(
            harness.latest(&uri),
            Some(Vec::new()),
            "closing it clears what was published, because the exception was the buffer"
        );
    }

    #[test]
    fn a_gem_a_reader_has_open_is_still_not_squiggled() {
        // The other half, which keeps the clause from being a widening of `is_own_code`: nobody can
        // fix a warning inside someone's gem, a Rails bundle would bury the user's problems under
        // thousands of them, and opening the file does not make it the reader's to fix. A gem is
        // under a gem root, which `Layout::is_outside` checks first.
        let (dir, gem_home, env) = project_with_gem(UNTERMINATED);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.index();
        harness.index_gems();

        let uri = DocUri::from_path(&gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .expect("an absolute path");
        harness.open(&uri, UNTERMINATED);

        assert!(
            harness.latest(&uri).is_none_or(|items| items.is_empty()),
            "a gem the reader has open is still not theirs to fix"
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
        // Clients occasionally do this. An incremental range means something only against its exact
        // base text, so applying one to an empty buffer is worse than dropping it; a whole-buffer
        // change carries its own base and can stand in for the missing `didOpen`.
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
        // Closing a tab does not remove the file from the project. The graph must return to the
        // disk's version, dropping the document only if no disk copy remains (a rename or delete,
        // from here).
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
        std::fs::remove_file(uri.to_file_path().expect("a path")).unwrap();
        harness.run(Task::DidClose { uri: uri.clone() });
        assert!(
            !harness.has("Person"),
            "a file that is gone from disk is gone from the graph"
        );
    }

    #[test]
    fn saving_changes_nothing_because_the_buffer_was_already_indexed() {
        // `didSave` follows every `didChange` for the same text. Re-indexing here would double the
        // work of typing for no new information.
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
        // `ya-lsp.toml` is edited by hand and can be saved half-written. The server must say so and
        // continue with defaults; going quiet looks like a crash.
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
        // The editor's layer never touches the filesystem, so it arrives carried, not re-read, and
        // it still rebuilds the graph, because it can change what is indexed.
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
        // Silence is the worst outcome here: a half-indexed bundle looks exactly like a gem that
        // was never installed.
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

    /// A gem's source lives outside every workspace folder, so the server must claim it itself.
    ///
    /// The client's document selector is the only gate on what it sends, and it cannot name a gem
    /// root without a second copy of `workspace::gems` in TypeScript. So the server says where its
    /// answers are, over the file watcher's channel. Otherwise `definition` jumps into
    /// `activerecord-8.1.3.1/lib/active_record.rb` and every request in that file is dead, with
    /// nothing logged because nothing was asked.
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

        // **The gem's own directory, not where it was found.** `Gems::roots` holds every gem path
        // on the machine (every installed Ruby, plus the system's), and claiming all of them would
        // claim every Ruby file of every bundle. It also makes the extension's arbitration
        // meaningful: two folders on one Ruby with *different* bundles claim different gems, and a
        // gem only one locked stays with that one.
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

    /// A gem file the client opens answers like any other file: the claim's other half.
    ///
    /// The server was never broken here (the bundle is already in the graph); it just was never
    /// asked. `didOpen` a gem's source and all three requests a user reaches for inside one answer.
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
        // A `LocationLink`, because the harness's client takes them; the URI is the point.
        assert_eq!(
            harness.definition_at(&gem_file, SOURCE, "Shouty\n    end")[0]["targetUri"],
            serde_json::json!(gem_file.as_str()),
            "and a jump that lands in the gem it started in"
        );
    }

    /// A vendored bundle is already claimed, and claiming it again answers every hover twice.
    ///
    /// `vendor/bundle/ruby/<abi>` is inside the root by construction, so it is *foreign* (nobody
    /// can fix a warning in it) and also *claimed* by the client's original selector. The two
    /// questions differ, and only the second decides this.
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
    /// Re-registering an id the client holds replaces its *record* without disposing the old
    /// provider, so the old selector would keep answering beside the new one, the duplicate the
    /// workspace filter avoids, by another route. The ids are fixed strings so they can be named
    /// again.
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
        while harness.analysis.step_pipeline() {}

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

    /// A client that takes no dynamic registration keeps what it has, and is told once.
    ///
    /// Silence is expensive here: one file answering nothing while its neighbours answer reads as
    /// the server being wrong, not as never being asked. Once per process, not per reload, so
    /// saving `ya-lsp.toml` five times does not repeat it five times.
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
        // The declaration still carries its diagnostic after the file is deleted. Placing a range
        // needs the text, and squiggles in guessed positions are worse than none.
        let mut harness = Harness::new();
        let uri = harness.write("app/broken.rb", "class Broken\n  def oops(\nend\n");
        harness.index();
        assert!(
            harness.latest(&uri).is_some_and(|items| !items.is_empty()),
            "the syntax error is reported while the file is there"
        );

        std::fs::remove_file(uri.to_file_path().expect("a path")).unwrap();
        harness.analysis.publish_diagnostics();

        // Not silence: `publishDiagnostics` is stateful per URI, so unplaceable squiggles must be
        // cleared with an explicit empty array, or they stay on screen as long as the editor is
        // open.
        assert_eq!(
            harness.latest(&uri),
            Some(Vec::new()),
            "the stale diagnostics have to be cleared, not merely stopped"
        );
    }

    #[test]
    fn closing_an_unsaved_buffer_clears_the_squiggles_it_had() {
        // A different loss from `..._gone_from_disk`: there the document stays in the graph and
        // only its text goes, so the diagnostics survive with nowhere to go. Here the document is
        // deleted outright (`didClose` on a fileless buffer), and its diagnostics go with it.
        //
        // So the *clearing* is the whole assertion: the gone document still needs an explicit empty
        // array, or its squiggles stay on screen with no buffer and no way to remove them.
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
        // On close the buffer stops shadowing disk and the file is re-read, so the graph holds what
        // is really there. A file that exists but cannot be read is the one case with neither
        // answer: keeping the buffer's text would make the graph assert the contents of a file
        // nobody can open.
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

        let path = uri.to_file_path().expect("a path");
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
        // Without this filter, opening a real Rails app publishes hundreds of diagnostics inside
        // other people's gems. Nobody can act on them, and they bury what the user actually broke.
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
        // A vendored bundle sits at `vendor/bundle/ruby/<abi>`, inside the root, so a
        // workspace-prefix test alone lets every gem there publish diagnostics.
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
        // gone for the session, and the only symptom is navigation quietly degrading after someone
        // edits ya-lsp.toml.
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty::Megaphone.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();
        assert!(!harness.definition_at(&uri, source, "Megaphone").is_null());

        harness.run(Task::ReloadConfig);
        // The reload queues the gems again but indexes none; that is background work.
        while harness.analysis.step_pipeline() {}
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
        // The engine walk. `ActiveStorage::Service` lives in `lib/` and always answers;
        // `ActiveStorage::Blob` lives in `app/models/`, and an engine declares
        // `require_paths = ["lib"]`, so without the walk it does not. Both halves are asserted: a
        // fix that made `app/` a load path would pass the first and break `require`.
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

        // The engine is indexed and still not the user's code. Three features depend on that test
        // for an unchanged reason (nobody fixes a warning in someone's engine), so generators ask a
        // separately named question instead of widening it.
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
        // `.gem_rbs_collection/` is hidden, so the workspace walk prunes it; it arrives as its own
        // signature path, on the same background pass as a gem's `sig/`.
        //
        // The second assertion is the important one. The collection lives *inside* the root, the
        // shape that made a vendored bundle publish unfixable squiggles: `is_own_code` must exclude
        // it explicitly, because "under the root" is the wrong question.
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
        // A filter, not a mute button: the file beside it is the user's.
        assert!(harness.analysis.is_own_code(uri.as_str()));

        let found = harness.declarations_at(&uri, "Blanket.new.~\n");
        assert_eq!(found, vec!["tuck".to_owned()], "{found:?}");
    }
    /// A reload that changes `[index] load_paths` changes what counts as the user's own code.
    ///
    /// `own_prefixes` is the one prefix list not derived from the bundle, so nothing else would
    /// rebuild it. `rebuild` re-runs the walk and clears every neighbouring list; a stale one here
    /// would answer about the *previous* configuration's directories for the rest of the session.
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

    /// A tree outside the root, named by `[index] load_paths`, is indexed, is the user's own code,
    /// and is a root the client is asked to claim.
    ///
    /// The monorepo case, where each of the three can fail quietly:
    ///
    /// 1. **Not indexed**: a shared model would be a name the graph does not hold.
    /// 2. **Foreign**: `is_own_code` is a prefix test against the root, so no diagnostics, no
    ///    rename, and ranked below the bundle in search, in a directory the project wrote down by
    ///    hand.
    /// 3. **Unclaimed**: a file outside every workspace folder is claimed by no selector, so
    ///    nothing would ask about it.
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

        // The user's own code: the prefix test's second way to say yes.
        //
        // Canonicalized, because the index holds that spelling: `resolve_load_path` resolves the
        // path once, and the walk, `own_prefixes` and the registration all derive from it. On macOS
        // a temp directory reaches disk via `/var -> /private/var`, so the spellings really differ
        // here.
        let real = shared.path().canonicalize().unwrap();
        let user_uri = DocUri::from_path(&real.join("models/user.rb")).unwrap();
        assert!(
            harness.analysis.is_own_code(user_uri.as_str()),
            "a tree the project named by hand is not somebody else's gem"
        );
        // The guard in the direction a widened prefix test threatens: a real gem root must stay
        // foreign, or diagnostics appear inside the bundle.
        assert!(
            !harness
                .analysis
                .is_own_code("file:///gems/activerecord-8.1.3.1/lib/active_record.rb"),
            "widening `is_own_code` must not have swallowed the gems it exists to exclude"
        );
    }

    #[test]
    fn a_closed_document_is_read_once_per_version_the_graph_holds() {
        // An instance variable's read asks every writer document of its object on every hover, so
        // a closed one is held by the version the graph holds, not read from disk each time.
        let mut harness = Harness::new();
        let written = "class Story\n  def initialize\n    @title = \"x\"\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", written);
        harness.index();
        let read = |harness: &Harness| {
            harness
                .analysis
                .read_of(uri.as_str())
                .unwrap()
                .0
                .to_string()
        };
        assert_eq!(read(&harness), written);

        // The disk moves on before anyone says so: the read keeps the text the graph holds, which
        // is what every offset into it is measured against.
        let moved = "class Story\nend\n";
        harness.write("app/models/story.rb", moved);
        assert_eq!(read(&harness), written);

        // Once the graph holds the new version, so does the read.
        harness.watch(&[&uri]);
        assert_eq!(read(&harness), moved);

        // A text first read after the disk moved is not the graph's, so it is not held: each read
        // goes to the disk, as before.
        let other = harness.write("app/models/tag.rb", "class Tag\nend\n");
        harness.watch(&[&other]);
        let read_other = |harness: &Harness| {
            harness
                .analysis
                .read_of(other.as_str())
                .unwrap()
                .0
                .to_string()
        };
        harness.write("app/models/tag.rb", "class Tag\n  # 2\nend\n");
        assert_eq!(read_other(&harness), "class Tag\n  # 2\nend\n");
        harness.write("app/models/tag.rb", "class Tag\n  # 3\nend\n");
        assert_eq!(read_other(&harness), "class Tag\n  # 3\nend\n");

        // An open buffer answers for itself, never the held text: opened as the disk has it, the
        // graph's version is the held one's, and typing leaves the graph behind.
        harness.open(&uri, moved);
        assert_eq!(read(&harness), moved);
        let typed = "class Story\n  # typed\nend\n";
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: None,
                text: typed.to_owned(),
            }],
        );
        assert_eq!(read(&harness), typed);
    }
}
