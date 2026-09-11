//! The harness every end-to-end test in the crate drives the server through.
//!
//! It is here rather than in [`crate::testing`] for the reason `analysis/` exists at all: a
//! [`Harness`] holds an [`Analysis`], which is private to `analysis`, and the crate's first
//! invariant is that nothing outside `analysis/` may name a rubydex type. A child module of
//! `analysis` sees every private field; the rest of the crate sees a `Harness` that answers in
//! `serde_json::Value`, `String`, `usize` and `bool` and nothing else. The one method that does
//! name a rubydex type — [`Harness::every_generated_document`] — is `pub(super)` for that
//! reason, so the boundary is the compiler's rather than a habit's.
//!
//! That is also why [`Harness::has`], [`Harness::declarations_of`], [`Harness::generated_for`]
//! and [`Harness::document_count`] exist: they are the graph questions a test beside a *reader*
//! needs to ask, phrased in types that reader is allowed to hold. A test that wants more than
//! they offer is asking about the analysis thread rather than about the reader, whatever Ruby
//! feature it happens to exercise, and belongs inside `analysis/`.

use super::locator::Site;
use super::requests::file_name;
use super::*;

// The names a moved test reaches for, re-exported so a test module beside the code it pins needs
// one `use` line rather than fifteen. Everything here is already `pub` somewhere; nothing private
// to `analysis` is widened by being named again.
pub(crate) use crate::analysis::position::PositionEncoding;
pub(crate) use crate::analysis::{ClientSupport, Task, TextChange, synthesized};
pub(crate) use crate::messages;
pub(crate) use crate::workspace::{DocUri, Workspace, gems};

/// The LSP position of the first occurrence of `needle`.
///
/// Every fixture here is ASCII, so counting characters is counting bytes; `position.rs`
/// owns the cases where that is not true.
pub(crate) fn position_of(source: &str, needle: &str) -> serde_json::Value {
    let offset = source
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in fixture"));
    let line = source[..offset].matches('\n').count();
    let column = offset - source[..offset].rfind('\n').map_or(0, |index| index + 1);
    serde_json::json!({ "line": line, "character": column })
}

/// The LSP position of the `~` in a fixture, which is removed before it is sent.
pub(crate) fn marked_position(marked: &str) -> serde_json::Value {
    let offset = marked.find('~').expect("a ~ marking the cursor");
    let line = marked[..offset].matches('\n').count();
    let character = offset - marked[..offset].rfind('\n').map_or(0, |index| index + 1);
    serde_json::json!({ "line": line, "character": character })
}

/// The selection a fixture marks with two `~`, as the protocol spells one — or an empty
/// range at a single `~`, which is what an editor sends when nothing is selected.
pub(crate) fn marked_range(marked: &str) -> serde_json::Value {
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
pub(crate) fn drawn(help: &serde_json::Value) -> String {
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
pub(crate) fn drawn_hierarchy(answer: &serde_json::Value) -> String {
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

/// Each hint spliced into the line it is drawn on, which is the line the user sees.
///
/// The position is half of every hint and the half a list of labels cannot show — a return
/// label drawn at the method's name rather than past its parameter list lands *inside* the
/// parameter list, where it reads as an annotation on the last parameter — and a column
/// number is the hardest possible way to notice that. So the label goes into the text, and
/// the assertion is a picture of the margin.
///
/// The fixture is ASCII, so a character offset and a byte offset are the same number here.
/// `position.rs` is what holds that apart everywhere it is not.
pub(crate) fn drawn_hints(source: &str, answer: &serde_json::Value) -> String {
    let Some(rows) = answer.as_array() else {
        return "null".to_owned();
    };
    rows.iter()
        .map(|hint| {
            let line = hint["position"]["line"].as_u64().unwrap_or_default() as usize;
            let at = hint["position"]["character"].as_u64().unwrap_or_default() as usize;
            let text = source.lines().nth(line).unwrap_or("(past the file)");
            let label = hint["label"].as_str().unwrap_or("(not a string)");
            format!("{}{label}{}", &text[..at], &text[at..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rows of a call hierarchy answer, drawn with the call sites that put them there.
///
/// `fromRanges` is the half a list of names cannot show, and the half an editor draws: a row
/// naming the right method with its ranges against the wrong file highlights whatever text
/// happens to be at those offsets. Incoming and outgoing rows differ only in which key holds
/// the item, so one function draws both and a test never has to say which it asked for.
pub(crate) fn drawn_calls(answer: &serde_json::Value) -> String {
    let Some(rows) = answer.as_array() else {
        return "null".to_owned();
    };
    rows.iter()
        .map(|row| {
            let item = if row["from"].is_object() {
                &row["from"]
            } else {
                &row["to"]
            };
            let sites: Vec<String> = row["fromRanges"]
                .as_array()
                .map(|ranges| {
                    ranges
                        .iter()
                        .map(|range| {
                            format!(
                                "{}:{}-{}",
                                range["start"]["line"],
                                range["start"]["character"],
                                range["end"]["character"]
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "{} — {} [{}]",
                item["name"].as_str().unwrap_or_default(),
                item["detail"].as_str().unwrap_or("(no detail)"),
                sites.join(" ")
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
pub(crate) fn edits_in(answer: &serde_json::Value) -> Vec<(String, Vec<lsp_types::TextEdit>)> {
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
pub(crate) fn all_symbols(outline: &serde_json::Value) -> Vec<&serde_json::Value> {
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
pub(crate) fn uncontained(outline: &serde_json::Value) -> Vec<String> {
    pub(crate) fn point(value: &serde_json::Value) -> (u64, u64) {
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

/// The file back, with `mark` under every byte of each span, and every untouched line
/// dropped.
///
/// Lines with nothing on them go so that an assertion is about what was answered; the ones
/// that stay carry their own text, which is what makes "and not the one in the comment"
/// something the fixture *shows* rather than something a test name claims.
///
/// One merge rule, for the pictures that draw two answers at once: a `d` landing on a cell
/// something else already marked **uppercases** that mark rather than replacing it, so
/// "both requests said this span" and "only one did" are different characters.
pub(crate) fn draw<'s>(
    source: &str,
    spans: impl Iterator<Item = (&'s serde_json::Value, char)>,
) -> String {
    let mut masks: Vec<Vec<char>> = source
        .lines()
        .map(|line| vec![' '; line.chars().count()])
        .collect();
    for (range, mark) in spans {
        let line = range["start"]["line"].as_u64().unwrap_or_default() as usize;
        let start = range["start"]["character"].as_u64().unwrap_or_default() as usize;
        let end = range["end"]["character"].as_u64().unwrap_or_default() as usize;
        let Some(mask) = masks.get_mut(line) else {
            continue;
        };
        for column in start..end {
            if let Some(cell) = mask.get_mut(column) {
                *cell = match (*cell, mark) {
                    (' ', _) => mark,
                    (already, 'd') => already.to_ascii_uppercase(),
                    _ => mark,
                };
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

/// An `Analysis` wired to a discarded output channel, over a throwaway workspace.
pub(crate) struct Harness {
    pub(super) analysis: Analysis,
    pub(super) outgoing: Receiver<Message>,
    /// Notifications `ask` stepped over on its way to a response. Without this, asking a
    /// question silently throws away every `showMessage` and `publishDiagnostics` the
    /// handler sent first, and a test that looks for one quietly cannot find it.
    stashed: std::cell::RefCell<Vec<Message>>,
    pub(crate) root: tempfile::TempDir,
    version: i32,
}

/// The registrar the real server builds, for a client that takes every dynamic registration.
///
/// Built the way `server::run` builds it rather than stubbed, because the thing worth pinning is
/// that a gem root the bundle resolved to comes back as a registration the client would act on —
/// and a stub would pin the harness instead. The reach across into `server::capabilities` is
/// test-only and deliberate: production code goes the other way, which is why what crosses the
/// thread boundary there is a closure and not that module's types.
pub(crate) fn document_registrar() -> crate::analysis::DocumentRegistrar {
    let capabilities = crate::server::capabilities::every_dynamic_registration_accepted();
    let requested =
        crate::server::capabilities::dynamic_documents(PositionEncoding::Utf16, &capabilities);
    Box::new(move |prefixes| {
        crate::server::capabilities::document_registrations(&requested, prefixes)
    })
}

impl Harness {
    pub(crate) fn new() -> Self {
        Self::with_encoding(PositionEncoding::Utf16)
    }

    /// A harness whose workspace holds `config` as its `ya-lsp.toml`.
    ///
    /// The file layer and not the client's, which is the layer the harness itself uses — so a
    /// test written this way outranks the harness's own settings and can turn off something it
    /// switches on.
    pub(crate) fn configured(config: &str) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("ya-lsp.toml"), config).expect("ya-lsp.toml");
        Self::at(root, PositionEncoding::Utf16)
    }

    pub(crate) fn with_encoding(encoding: PositionEncoding) -> Self {
        Self::at(tempfile::tempdir().expect("tempdir"), encoding)
    }

    pub(crate) fn at(root: tempfile::TempDir, encoding: PositionEncoding) -> Self {
        // An empty environment, so gem discovery cannot wander off into whatever Ruby the
        // machine running the tests happens to have installed.
        Self::at_with_env(root, encoding, gems::Env::default())
    }

    pub(crate) fn at_with_env(
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
        // **The harness is a Rails project and says so**, rather than leaving it to detection.
        // `rails.enabled` defaults to `auto`, which asks whether there is a
        // `config/application.rb` or a lockfile that holds railties — and a `tempdir` with three
        // files in it has neither, so every fixture that writes `has_many` would silently stop
        // being about Rails. Writing one of those markers into the fixture instead would put a
        // file in the index (`config/application.rb` is Ruby) or a lockfile in front of gem
        // discovery, and both move counts these tests assert on.
        //
        // It is the **client's** layer, so a fixture that writes its own `ya-lsp.toml` still
        // wins — which is what lets a test turn a family of it back off and mean it.
        let options = serde_json::json!({ "rails": { "enabled": true } });
        let (workspace, problems) =
            Workspace::load_with_env(root.path().to_path_buf(), Some(options), env);
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
                    hint_refresh: true,
                },
                document_registrar(),
                sender,
                Cancellations::default(),
                crate::logging::Reload::default(),
            ),
            outgoing: receiver,
            stashed: std::cell::RefCell::new(Vec::new()),
            root,
            version: 1,
        }
    }

    pub(crate) fn write(&self, relative: &str, source: &str) -> DocUri {
        let path = self.root.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, source).unwrap();
        DocUri::from_path(&path).unwrap()
    }

    /// Index the workspace and push the first round of diagnostics, as `spawn` does.
    pub(crate) fn index(&mut self) {
        self.analysis.index_workspace();
        self.analysis.publish_diagnostics();
    }

    /// Queue the gems and run the background index to completion, as the run loop would
    /// during a stretch with no editor traffic.
    pub(crate) fn index_gems(&mut self) {
        self.analysis.queue_background_indexing();
        while self.analysis.step_gem_indexing() {}
        self.analysis.settle();
    }

    /// Make this harness's client one that takes no dynamic registration at all.
    ///
    /// A setter rather than a sixth constructor: what changes is one value, and every fixture in
    /// this module is still the right project to ask the question of.
    pub(crate) fn takes_no_registration(&mut self) {
        self.analysis.documents = crate::analysis::no_document_registrar();
    }

    /// Every notification sent since the last drain, in order, including the ones `ask`
    /// stepped over.
    pub(crate) fn notifications(&self, method: &str) -> Vec<lsp_server::Notification> {
        self.sent(|message| matches!(message, Message::Notification(sent) if sent.method == method))
            .into_iter()
            .filter_map(|message| match message {
                Message::Notification(notification) => Some(notification),
                _ => None,
            })
            .collect()
    }

    /// Every server-initiated request for `method`, sent since the last read.
    ///
    /// The server starts three of its own — the file watcher's registration, the document
    /// registration that claims a gem's source, and the inlay-hint refresh — and they travel the
    /// same channel as the notifications above rather than a second one.
    pub(crate) fn requests(&self, method: &str) -> Vec<Request> {
        self.sent(|message| matches!(message, Message::Request(sent) if sent.method == method))
            .into_iter()
            .filter_map(|message| match message {
                Message::Request(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    /// Everything the server has sent, with whatever this caller did not ask for put back.
    ///
    /// **Put back rather than dropped.** Two of these readers are now used in one test — a
    /// registration is a `Request` and the sentence explaining a decline is a `Notification` — and
    /// while this drained unconditionally, whichever ran first took the other's messages with it.
    fn sent(&self, wanted: impl Fn(&Message) -> bool) -> Vec<Message> {
        let mut pending: Vec<Message> = self.stashed.borrow_mut().drain(..).collect();
        while let Ok(message) = self.outgoing.try_recv() {
            pending.push(message);
        }
        let (matched, rest): (Vec<Message>, Vec<Message>) = pending.into_iter().partition(&wanted);
        self.stashed.borrow_mut().extend(rest);
        matched
    }

    /// Every `window/showMessage` body sent since the last drain.
    pub(crate) fn messages(&self) -> Vec<String> {
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
    pub(crate) fn progress(&self) -> Vec<(String, String)> {
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
    pub(crate) fn published(&self) -> Vec<(String, Vec<lsp_types::Diagnostic>)> {
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
    pub(crate) fn latest(&self, uri: &DocUri) -> Option<Vec<lsp_types::Diagnostic>> {
        self.published()
            .into_iter()
            .rfind(|(sent, _)| sent == uri.as_str())
            .map(|(_, items)| items)
    }

    /// A `workspace/didChangeWatchedFiles`, as a client sends it: the file system changed
    /// and nothing else did — no `didOpen`, no `didSave`, no buffer anywhere.
    pub(crate) fn watch(&mut self, uris: &[&DocUri]) {
        self.run(Task::WatchedFiles {
            uris: uris.iter().map(|uri| (*uri).clone()).collect(),
        });
    }

    pub(crate) fn open(&mut self, uri: &DocUri, text: &str) {
        self.run(Task::DidOpen {
            uri: uri.clone(),
            text: text.to_owned(),
            version: Some(1),
        });
    }

    /// A whole-buffer change, which is what a client sends for a paste or a revert.
    pub(crate) fn change(&mut self, uri: &DocUri, text: &str) {
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
    pub(crate) fn edit_without_indexing(&mut self, uri: &DocUri, changes: Vec<TextChange>) {
        self.version += 1;
        let version = self.version;
        self.analysis.handle(Task::DidChange {
            uri: uri.clone(),
            changes,
            version: Some(version),
        });
    }

    pub(crate) fn edit(&mut self, uri: &DocUri, changes: Vec<TextChange>) {
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
    pub(crate) fn ask(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
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
    pub(crate) fn ask_raw(&mut self, method: &str, params: serde_json::Value) -> Response {
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
    pub(crate) fn synthesize(
        &mut self,
        source: &DocUri,
        rbs: &str,
        mappings: Vec<synthesized::Mapping>,
    ) -> String {
        let uri = self
            .analysis
            .synthesized
            .record(
                &mut self.analysis.graph,
                &mut self.analysis.types,
                source,
                vec![synthesized::Part {
                    body: "class:Story".to_owned(),
                    rbs: rbs.to_owned(),
                    mappings,
                    named: Vec::new(),
                }],
            )
            .into_iter()
            .next()
            .unwrap_or_default();
        // What every other indexing entry point leaves to its caller, for the same reason:
        // a batch marks itself dirty once rather than per file.
        self.analysis.mark_dirty();
        self.analysis.settle();
        uri
    }

    pub(crate) fn hover_at(
        &mut self,
        uri: &DocUri,
        source: &str,
        needle: &str,
    ) -> serde_json::Value {
        self.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, needle),
            }),
        )
    }

    pub(crate) fn definition_at(
        &mut self,
        uri: &DocUri,
        source: &str,
        needle: &str,
    ) -> serde_json::Value {
        self.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, needle),
            }),
        )
    }

    pub(crate) fn references_at(
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
    pub(crate) fn reference_list(
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

    pub(crate) fn symbol_search(&mut self, query: &str) -> serde_json::Value {
        self.ask("workspace/symbol", serde_json::json!({ "query": query }))
    }

    /// Search results as `name` or `Container#name`, in the order they were ranked.
    pub(crate) fn symbol_names(&mut self, query: &str) -> Vec<String> {
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
    pub(crate) fn complete(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
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
    pub(crate) fn signature(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
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
    pub(crate) fn signature_card(&mut self, uri: &DocUri, marked: &str) -> String {
        drawn(&self.signature(uri, marked))
    }

    /// Open a buffer written with a `~` where the cursor is, and ask what expanding the
    /// selection from it reaches.
    pub(crate) fn selection(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
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
    pub(crate) fn folding(&mut self, uri: &DocUri, source: &str) -> serde_json::Value {
        self.open(uri, source);
        self.ask(
            "textDocument/foldingRange",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        )
    }

    pub(crate) fn links(&mut self, uri: &DocUri, source: &str) -> serde_json::Value {
        self.open(uri, source);
        self.ask(
            "textDocument/documentLink",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        )
    }

    /// The code actions offered over a buffer marked with one or two `~`, drawn as each
    /// title followed by the file the action leaves behind.
    pub(crate) fn actions(&mut self, uri: &DocUri, marked: &str) -> String {
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
    pub(crate) fn highlight(&mut self, uri: &DocUri, marked: &str) -> serde_json::Value {
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
    pub(crate) fn highlight_map(&mut self, uri: &DocUri, marked: &str) -> String {
        let found = self.highlight(uri, marked);
        let source = marked.replace('~', "");
        let Some(spans) = found.as_array() else {
            return "null".to_owned();
        };

        draw(
            &source,
            spans.iter().map(|span| {
                // LSP numbers them `Text` 1, `Read` 2, `Write` 3; `Text` is never answered.
                let mark = if span["kind"].as_u64() == Some(3) {
                    'w'
                } else {
                    'r'
                };
                (&span["range"], mark)
            }),
        )
    }

    /// `documentHighlight` and `definition` at one cursor, in one picture.
    ///
    /// `w` and `r` are what the highlight lit, and a definition target landing in this file
    /// **uppercases** the cell it lands on. So a `W` is the two requests agreeing, an `R` is
    /// a jump to a place the file only reads, and a lone `d` is a jump to a span nothing lit
    /// — which is the disagreement lane 2 of the audit counts, drawn where it happened.
    ///
    /// Both requests are asked of one open buffer at one offset, which is the only way the
    /// comparison means anything.
    pub(crate) fn agreement_map(&mut self, uri: &DocUri, marked: &str) -> String {
        let position = marked_position(marked);
        let source = marked.replace('~', "");
        self.open(uri, &source);
        let at = serde_json::json!({
            "textDocument": { "uri": uri.as_str() },
            "position": position,
        });
        let lit = self.ask("textDocument/documentHighlight", at.clone());
        let jumped = self.ask("textDocument/definition", at);
        let empty = Vec::new();
        let lit = lit.as_array().unwrap_or(&empty);
        let jumped = jumped.as_array().unwrap_or(&empty);
        if lit.is_empty() && jumped.is_empty() {
            return "null".to_owned();
        }
        draw(
            &source,
            lit.iter()
                .map(|span| {
                    // LSP numbers them `Text` 1, `Read` 2, `Write` 3; `Text` is never
                    // answered.
                    let mark = if span["kind"].as_u64() == Some(3) {
                        'w'
                    } else {
                        'r'
                    };
                    (&span["range"], mark)
                })
                .chain(
                    jumped
                        .iter()
                        .filter(|link| link["targetUri"] == serde_json::json!(uri.as_str()))
                        .map(|link| (&link["targetSelectionRange"], 'd')),
                ),
        )
    }

    /// Ask for the type hierarchy at the first occurrence of `needle`.
    pub(crate) fn prepare_hierarchy(
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
    pub(crate) fn expand(
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

    /// Every hint in one document: the whole file as the range, which is what an editor
    /// showing a short file asks for.
    pub(crate) fn hints_in(&mut self, uri: &DocUri) -> serde_json::Value {
        self.hints_within(uri, (0, 0), (9_999, 0))
    }

    pub(crate) fn hints_within(
        &mut self,
        uri: &DocUri,
        start: (u32, u32),
        end: (u32, u32),
    ) -> serde_json::Value {
        self.ask(
            "textDocument/inlayHint",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "range": {
                    "start": { "line": start.0, "character": start.1 },
                    "end": { "line": end.0, "character": end.1 },
                },
            }),
        )
    }

    pub(crate) fn prepare_calls(
        &mut self,
        uri: &DocUri,
        source: &str,
        needle: &str,
    ) -> serde_json::Value {
        self.ask(
            "textDocument/prepareCallHierarchy",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, needle),
            }),
        )
    }

    /// Prepare at `needle`, expand it the way an editor does, and draw the rows.
    ///
    /// The item goes back verbatim — `data`, `uri` and both ranges — because that is all the
    /// client sends and `outgoingCalls` reads more of it than `incomingCalls` does.
    pub(crate) fn call_rows(
        &mut self,
        method: &str,
        uri: &DocUri,
        source: &str,
        needle: &str,
    ) -> String {
        let prepared = self.prepare_calls(uri, source, needle);
        let item = prepared[0].clone();
        assert!(item.is_object(), "nothing to expand at {needle:?}");
        drawn_calls(&self.ask(method, serde_json::json!({ "item": item })))
    }

    /// The rows of a hierarchy answer, drawn the way Ruby writes what they are.
    ///
    /// The keyword makes the kind visible — `module Comparable` in a list of supertypes is
    /// the answer's most surprising claim and also its most correct one — and the detail
    /// column is what separates the project's two rows from the gems' eight. Drawn rather
    /// than asserted field by field, as the signature card and the highlight map are: a row
    /// in the wrong place, with the wrong kind, or pointing at the wrong file is one thing
    /// to read here and three assertions to write otherwise.
    pub(crate) fn hierarchy_rows(
        &mut self,
        method: &str,
        uri: &DocUri,
        source: &str,
        needle: &str,
    ) -> String {
        drawn_hierarchy(&self.expand(method, uri, source, needle))
    }

    pub(crate) fn prepare_rename(
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
    pub(crate) fn renamed(&mut self, uri: &DocUri, source: &str, needle: &str, to: &str) -> String {
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
    pub(crate) fn suggestions(&mut self, uri: &DocUri, marked: &str) -> Vec<String> {
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
    pub(crate) fn first_rows(&mut self, uri: &DocUri, marked: &str, count: usize) -> Vec<String> {
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
    pub(crate) fn declarations_at(&mut self, uri: &DocUri, marked: &str) -> Vec<String> {
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

    pub(crate) fn outline(&mut self, uri: &DocUri) -> serde_json::Value {
        self.ask(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        )
    }

    /// Run a task and let the debounced resolution finish, as the real loop would.
    pub(crate) fn run(&mut self, task: Task) {
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
    pub(crate) fn generated_rbs(&self, relative: &str) -> String {
        let uri = DocUri::from_path(&self.root.path().join(relative)).expect("a file uri");
        self.analysis.synthesized.text(&uri).unwrap_or_default()
    }

    /// Every document the generators wrote, text and all, in a stable order.
    pub(super) fn every_generated_document(
        &self,
    ) -> std::collections::BTreeMap<rubydex::model::ids::UriId, String> {
        self.analysis
            .synthesized
            .every_document()
            .into_iter()
            .map(|(uri, rbs)| (uri, rbs.to_owned()))
            .collect()
    }

    /// Settle the graph with no task behind it, as the run loop does on the debounce.
    pub(crate) fn settle(&mut self) {
        self.analysis.settle();
    }

    /// The RBS the generators wrote for one source document, addressed by its URI.
    ///
    /// [`Harness::generated_rbs`] asks the same question from a relative path; this is for the
    /// callers that already hold the [`DocUri`] `write` handed back.
    pub(crate) fn generated_for(&self, source: &DocUri) -> Option<String> {
        self.analysis.synthesized.text(source)
    }

    /// How many declarations the graph holds under one name.
    ///
    /// [`Harness::has`] answers "is this declared at all", which cannot tell "declared once"
    /// from "declared by two generators that both thought they owned it" — the failure the
    /// macro readers are most likely to have.
    pub(crate) fn declarations_of(&self, name: &str) -> usize {
        self.analysis
            .graph
            .get(name)
            .map_or(0, |declarations| declarations.len())
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.analysis.graph.get(name).is_some()
    }

    /// How many definitions the graph holds for one document.
    ///
    /// [`Harness::has`] cannot answer "was this indexed": a *declaration* is built by
    /// `Resolver::resolve` and a document that was indexed and not yet resolved has none,
    /// so it reads the same as one that was never indexed at all. A **definition** is put
    /// there by the indexing itself, which is what the deferral's tests ask.
    pub(crate) fn definitions_in(&self, uri: &DocUri) -> usize {
        self.analysis
            .graph
            .documents()
            .values()
            .find(|document| document.uri() == uri.as_str())
            .map_or(0, |document| document.definitions().len())
    }

    /// Documents rubydex knows about, minus the synthetic `rubydex:built-in` one.
    pub(crate) fn document_count(&self) -> usize {
        self.analysis
            .graph
            .documents()
            .values()
            .filter(|document| !document.uri().starts_with("rubydex:"))
            .count()
    }
}

// ---------------------------------------------------------------------------------------
// The fixtures and project builders more than one module's tests read
//
// A fixture with a single reader lives beside that reader; these are the ones two or more
// modules ask the same question of, and duplicating them is how two copies of "the same"
// project quietly stop being the same.
// ---------------------------------------------------------------------------------------

/// `class Foo` with no `end`: Prism reports it, and it is unambiguously the user's problem.
pub(crate) const UNTERMINATED: &str = "class Foo\n  def bar\n";

pub(crate) fn code(name: &str) -> Option<lsp_types::NumberOrString> {
    Some(lsp_types::NumberOrString::String(name.to_owned()))
}

pub(crate) const LIBRARY: &str = "\
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

pub(crate) fn library() -> (Harness, DocUri) {
    let mut harness = Harness::new();
    let uri = harness.write("lib/person.rb", LIBRARY);
    harness.index();
    (harness, uri)
}

pub(crate) const GALLERY: &str = "\
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

/// An rbs root shaped the way Ruby's own is, with RDoc's markup in it.
///
/// Synthetic rather than the vendored copy, deliberately: what the tests below pin is the
/// *card*, and pinning a card against 800 files of upstream prose would break on every rbs
/// release for a reason that has nothing to do with ya-lsp. Every shape that matters is
/// here — the call-seq header, a `<code>` span, an indented example, a dead `rdoc-ref:`
/// link — and each was copied from the real `String#upcase` comment.
pub(crate) const CORE_RBS: &str = "\
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
pub(crate) const OVERLOAD_RBS: &str = "\
class Coordinate
  # A point, from a pair or from text.
  def initialize: (String text) -> void
                | (Integer x, Integer y) -> void
end
";

pub(crate) const STDLIB_RBS: &str = "\
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
pub(crate) fn with_signatures(source: &str) -> (Harness, DocUri) {
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
pub(crate) const TYPED_RBS: &str = "\
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

GREETING: String
MYSTERY: Ghost
";

pub(crate) fn with_types(source: &str) -> (Harness, DocUri) {
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
pub(crate) fn class_at(harness: &mut Harness, uri: &DocUri, marked: &str) -> String {
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

/// The labels a completion response offered, and whether the receiver was resolved at all.
///
/// The second half is what tells a real answer from the fall-through: `precise: false` is
/// the name-based list, which matches **every method in the project** by name and is what
/// completion degrades to when it cannot type the receiver.
pub(crate) fn offered(answer: &serde_json::Value) -> (Vec<String>, bool) {
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

pub(crate) fn card(harness: &mut Harness, uri: &DocUri, source: &str, needle: &str) -> String {
    harness.hover_at(uri, source, needle)["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover on {needle:?}"))
        .to_owned()
}

/// Three shapes of `new`: an ordinary constructor, a class that writes its own `self.new`,
/// and a class with no constructor at all.
pub(crate) const CONSTRUCTORS: &str = "\
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

/// Build a project with one installed gem, and an `Env` pointing at it.
///
/// The gem is a real one in shape: unpacked under `gems/<full name>/lib`, with the
/// serialised gemspec RubyGems writes beside it. The gem home deliberately sits *outside*
/// the project, which is where a version manager puts it; the vendored case, where it does
/// not, has its own test.
pub(crate) fn project_with_gem(
    gem_source: &str,
) -> (tempfile::TempDir, tempfile::TempDir, gems::Env) {
    project_with_gem_file("lib/shouty.rb", gem_source)
}

/// The same gem, with its one file put where the caller says.
///
/// `relative` is under the gem root, so `lib/shouty/test/utils.rb` is a library file inside a
/// directory called `test` — which is what `environment::Fence::only_the_suite` exists to tell
/// apart from a suite, and what no path test alone can.
pub(crate) fn project_with_gem_file(
    relative: &str,
    gem_source: &str,
) -> (tempfile::TempDir, tempfile::TempDir, gems::Env) {
    let dir = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    std::fs::write(
        root.join("Gemfile.lock"),
        "GEM\n  remote: https://rubygems.org/\n  specs:\n    shouty (1.2.3)\n",
    )
    .unwrap();

    let gem_home = elsewhere.path().to_path_buf();
    let file = gem_home.join("gems/shouty-1.2.3").join(relative);
    std::fs::create_dir_all(file.parent().expect("a parent directory")).unwrap();
    std::fs::write(&file, gem_source).unwrap();
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
pub(crate) const ANCESTRY: &str = "\
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

/// A project whose bundle holds one Rails engine: a gem with `require_paths = ["lib"]`
/// whose real code is under `app/`.
pub(crate) fn project_with_engine(
    files: &[(&str, &str)],
) -> (tempfile::TempDir, PathBuf, gems::Env) {
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

/// A schema of the shape the reader takes: two columns on one table.
pub(crate) const SCHEMA: &str = "\
ActiveRecord::Schema.define(version: 1) do
  create_table \"stories\" do |t|
    t.string \"title\"
    t.string \"byline\"
  end
end
";

/// What a generator writes for [`SCHEMA`].
pub(crate) const SCHEMA_RBS: &str = "\
class Story
  def title: () -> String
  def byline: () -> String
end
";

/// The byte span of `needle` in `text`, for building a mapping out of a fixture rather than
/// out of hand-counted offsets that go stale the moment a line moves.
pub(crate) fn span(text: &str, needle: &str) -> (u32, u32) {
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
pub(crate) fn synthetic_project(caller: &str) -> (Harness, DocUri, DocUri) {
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
pub(crate) fn title_only(schema: &DocUri) -> Vec<synthesized::Mapping> {
    vec![synthesized::Mapping {
        generated: span(SCHEMA_RBS, "  def title: () -> String\n"),
        declared: Site {
            uri: schema.as_str().to_owned(),
            full: span(SCHEMA, "t.string \"title\""),
            selection: span(SCHEMA, "\"title\""),
        },
    }]
}

/// A Rails application, as small as one can be and still be one.
///
/// Two tables and one model between them: `stories` is claimed, `widgets` is not, and the
/// difference between them is the whole of what "the schema does not type more receivers,
/// it makes the ones already typed answer" means in a fixture.
pub(crate) const SCHEMA_RB: &str = "\
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

pub(crate) fn rails_project(caller: &str) -> (Harness, DocUri, DocUri) {
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

/// The same two tables as [`SCHEMA_RB`], as pg_dump would have written them.
///
/// Deliberately the same database, because the claim is that the *format* is the only
/// difference — so the two fixtures declaring the same thing is the assertion, and a
/// dump that happened to describe some other schema would hide it.
pub(crate) const STRUCTURE_SQL: &str = "\
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
pub(crate) fn sql_project(caller: &str) -> (Harness, DocUri, DocUri) {
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

/// A Rails application with three models and every association shape that matters.
///
/// `Story` has one of each; `Comment` is the element type two collections share, which is
/// what makes "one relation class per element type" observable; `Tag` exists so that a
/// `has_many :through` has an intermediate to find. `Ghost` is named by nothing and defined
/// by nothing, which is the decline every wrong inflection ends at.
pub(crate) fn models_project(caller: &str) -> (Harness, DocUri, DocUri) {
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
         scope :recent, -> { order(created_at: :desc) }\n  \
         belongs_to :keeper, class_name: Keeper::Handle.new\n\
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

/// A workspace shaped like the half of Rails the concern edge is about: a concern whose class-side
/// methods reach an includer through an `extend` no file writes.
///
/// Two spellings of the convention and one module that is not it, because the gate has to
/// be the nested `module ClassMethods` rather than `extend ActiveSupport::Concern` — 6 of
/// the 17 such modules in six corpora hand-roll the hook and 3 write neither.
pub(crate) const CONCERNS: &str = "\
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

/// The third spelling of the same edge, and the one no `ClassMethods` module appears in.
///
/// `ActiveSupport::Concern` `class_eval`s an `included do` block on each including class, so a
/// bare `extend M` in one puts `M`'s **instance** methods on that class's singleton. It is what
/// `activemodel/lib/active_model/api.rb` writes — `extend ActiveModel::Naming` and
/// `extend ActiveModel::Translation`, reached by every model through `ActiveRecord::Base`'s
/// `include ActiveModel::API` — and what installs `model_name` and `human_attribute_name`.
///
/// The `def`s are in **`Naming`'s own file** and not in this one, which is the whole point of the
/// fixture: the concern names a module and the module is somewhere else.
pub(crate) const EXTENDING_CONCERN: &str = "\
module Nameable
  extend ActiveSupport::Concern

  included do
    extend Naming
    extend Module.new { }
    Foo.extend Naming
  end
end
";

/// The module [`EXTENDING_CONCERN`] names, in the file a jump has to land in.
pub(crate) const EXTENDED_MODULE: &str = "\
module Naming
  def model_name
  end

  def self.not_installed
  end

  def hidden
  end

  private :hidden
end
";

/// The application's **own** concern, written where Rails puts one and installing its
/// `ClassMethods` by hand rather than through `ActiveSupport::Concern` — 6 of the corpus'
/// 17 do exactly this, and a gate on the `extend` would decline every one of them.
pub(crate) const OWN_CONCERN: &str = "\
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

/// The **other** spelling of the same edge: `class_methods do`, which writes no `module
/// ClassMethods` for the gate to find.
///
/// `ActiveSupport::Concern#class_methods` builds that module at run time and `module_eval`s the
/// block on it, so rubydex — which has no namespace for a block body — files every `def` here as
/// an *instance* member of the concern and the class object reaches none of them. The six
/// corpora write this spelling **120** times against 17 files holding a `module ClassMethods`, so
/// it is the majority one.
///
/// Every arm of the reader is in here: a `def` with each shape of parameter, an operator name RBS
/// cannot spell, a `def self.` that `extend` installs on nothing, both spellings of `private`, and
/// a `class_methods do` written in a **class**, where the call raises `NoMethodError`.
pub(crate) const BLOCK_CONCERN: &str = "\
module Tallyable
  extend ActiveSupport::Concern

  class_methods do
    def tally_by(column, limit = nil, *rest, scale:, unit: nil, **options)
    end

    def tally_all
    end

    def ==(other)
    end

    def self.not_extended
    end

    def named_private
    end

    private :named_private

    private

    def after_private
    end
  end
end

class Ledger
  include Tallyable

  class_methods do
    def never_reached
    end
  end
end
";

/// The mailer file the entry-point reader takes, and the base it inherits which the
/// application does not define — `ActionMailer::Base` is a gem's, and is read anyway.
pub(crate) const MAILERS: &str = "\
class UserMailer < ApplicationMailer
  def welcome(user)
    mail(to: user)
  end

  private

  def sender
  end
end
";

/// A routes file with one of each of the shapes the end-to-end tests need.
pub(crate) const ROUTES: &str = "\
Rails.application.routes.draw do\n\
  root to: \"home#index\"\n\
  resources :stories, only: [:index, :show] do\n\
    post :upvote, on: :member\n\
  end\n\
  draw :admin\n\
end\n";

/// A project with a routes file, a controller, a helper and a template.
pub(crate) fn routes_project(caller: &str) -> (Harness, DocUri) {
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

/// One spelling — `name` — used every way a Ruby file uses one.
///
/// A parameter in two methods, a block parameter shadowing one of them, a method and a call
/// to it, an instance variable in two different objects, and the same six letters in a
/// comment and in a string. That last pair is not decoration: matching words is what an
/// editor does when no server answers, and lighting up the comment is exactly how it is
/// wrong. `MAX` is here so the exact half — a constant the resolver linked — is pinned by
/// the same file as the half that is a scope walk.
pub(crate) const OCCURRENCES: &str = "\
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

/// `source` with the cursor at the end of `needle`, which must occur in it exactly once.
///
/// Naming a position by the text around it rather than by an index is what keeps these
/// readable while the fixture grows: `on("def greet(name")` says which of the seven `name`s
/// it means, and `on("(name", 2)` would not.
pub(crate) fn cursor_after(source: &str, needle: &str) -> String {
    let at = source.find(needle).expect("the needle is in the fixture");
    assert!(
        !source[at + 1..].contains(needle),
        "{needle:?} has to name one position, and names more than one"
    );
    let end = at + needle.len();
    format!("{}~{}", &source[..end], &source[end..])
}

/// [`cursor_after`] against [`OCCURRENCES`], which most of the drawings below use.
pub(crate) fn on(needle: &str) -> String {
    cursor_after(OCCURRENCES, needle)
}

/// A constant in a namespace, used four ways across two files, with a second constant of the
/// same name in another namespace that must not move.
pub(crate) const HR: &str = "\
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

/// The model a template renders, and the template that renders it.
///
/// Deliberately ordinary Rails: a collection assigned to an instance variable, a block local
/// taken out of it, a method call on that local, a constant, and markup wrapped around all of
/// it. Every ERB test below reads one of these two files, so what any of them asserts is
/// about the *technique* rather than about a fixture written to suit it.
pub(crate) const STORY: &str = "\
class Story
  TAGLINE = \"news\"

  def title
    @title
  end
end
";

pub(crate) const VIEW: &str = "\
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
pub(crate) fn shape(answer: &serde_json::Value) -> String {
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

/// The controller `app/views/stories/*` names, holding the two shapes a corpus has: an
/// instance variable assigned something nameable, and one assigned an ActiveRecord chain.
/// 88 of the corpus's 318 receiver sites are the first and most of the rest are the
/// second, so a fixture with only the first would be a fixture written to flatter the
/// feature.
pub(crate) const CONTROLLER: &str = "\
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
pub(crate) fn rails_app(harness: &Harness) -> DocUri {
    harness.write("app/models/story.rb", STORY);
    harness.write("app/controllers/stories_controller.rb", CONTROLLER);
    harness.write(
        "app/views/stories/show.html.erb",
        "<h1><%= @story.title %></h1>\n",
    )
}
