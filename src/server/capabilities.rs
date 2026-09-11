//! What the server tells the client it can do.
//!
//! Capabilities are announced per milestone: advertising a feature we do not implement makes
//! the editor show an empty result instead of falling back to its own heuristics, which is a
//! worse experience than not advertising at all.

use std::path::Path;

use lsp_types::{
    CallHierarchyServerCapability, ClientCapabilities, CodeActionKind, CodeActionOptions,
    CodeActionProviderCapability, CompletionOptions, CompletionOptionsCompletionItem,
    DidChangeWatchedFilesRegistrationOptions, DocumentLinkOptions, FileSystemWatcher,
    FoldingRangeProviderCapability, GlobPattern, HoverProviderCapability, InlayHintOptions,
    InlayHintServerCapabilities, OneOf, Registration, RelativePattern, RenameOptions, SaveOptions,
    SelectionRangeProviderCapability, SemanticTokenType, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensServerCapabilities,
    ServerCapabilities, SignatureHelpOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, WorkDoneProgressOptions,
    WorkspaceFileOperationsServerCapabilities, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};

use crate::{
    analysis::position::PositionEncoding,
    workspace::{
        DocUri,
        config::{CONFIG_FILE_NAME, IndexConfig},
    },
};

/// The `capabilities` object exactly as it goes out on the wire.
///
/// `lsp-types` 0.97 — the latest published version — has a field for `callHierarchyProvider` and
/// none for `typeHierarchyProvider`, so the one capability the crate cannot spell is flattened in
/// beside the ones it can. A typed struct rather than a `serde_json::Map` insertion: this module
/// is the wire contract, and the contract is worth being unable to misspell.
///
/// The alternative was `client/registerCapability`, which the protocol does allow for this one.
/// It would have made the feature depend on a client that takes dynamic registrations — the same
/// dependency that leaves the file watcher unavailable in several editors — for no reason beyond
/// a missing struct field.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Advertised {
    #[serde(flatten)]
    standard: ServerCapabilities,
    type_hierarchy_provider: bool,
}

#[must_use]
pub fn advertised(encoding: PositionEncoding) -> Advertised {
    Advertised {
        standard: server_capabilities(encoding),
        // Nothing is taken away by this one: no editor guesses at a type hierarchy, so
        // the command simply reports that there are no results until a server answers it.
        type_hierarchy_provider: true,
    }
}

#[must_use]
pub fn server_capabilities(encoding: PositionEncoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.to_lsp()),
        // Incremental. rubydex reparses the whole buffer on every `index_source`, so
        // this saves transfer rather than parsing — but on a large file, sending the entire
        // text on every keystroke is transfer the editor pays for at typing speed.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
            },
        )),
        // Announced only once implemented: a client that is told a server
        // provides hover will stop showing its own word-based fallback, so advertising early
        // makes the editor worse, not better.
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        // `workspace/symbol` answers with `SymbolInformation`, which carries a full
        // location — so no `resolveProvider`, and no `workspaceSymbol/resolve`. The lazy shape
        // exists to avoid reading a file per result; measured here that read is a few
        // milliseconds for a capped result set, and every client understands the eager one.
        references_provider: Some(OneOf::Left(true)),
        // Announced with the rest of them and for the same reason: a client told a
        // server highlights occurrences stops matching words itself, and a word match — which
        // lights up the name inside a comment, inside a string, and in an unrelated scope — is
        // better than nothing at all. ya-lsp answers `null` wherever it does not know, which is
        // what puts the client's own fallback back in play for exactly those positions.
        document_highlight_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        // `.` and `:` are the two characters that change what a completion *means* rather
        // than just narrowing it, and a client only re-asks mid-word for characters listed
        // here. `:` covers `Foo::` — LSP trigger characters are single characters, so there is
        // no way to say `::`, and the request for a lone `:` is classified and answered with
        // nothing. `@` and `$` are here because a sigil starts a name the client's own word
        // pattern would not treat as one, so without them instance and global variables are
        // never asked for at all.
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![
                ".".to_owned(),
                ":".to_owned(),
                "@".to_owned(),
                "$".to_owned(),
            ]),
            // Documentation is the expensive half of a completion item and the user reads it
            // for one row out of hundreds, so it is filled in on demand.
            resolve_provider: Some(true),
            completion_item: Some(CompletionOptionsCompletionItem {
                label_details_support: Some(false),
            }),
            ..CompletionOptions::default()
        }),
        // `(` and `,` are where a Ruby call gains an argument — the second covers the
        // paren-less form too, since `link_to "x", ` is where the next one goes. `)` only
        // re-triggers, which is to say it is asked while the popup is already up: the call it
        // closes has no further arguments, ya-lsp answers `null`, and the popup goes away
        // rather than standing there describing a call the cursor has left.
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_owned(), ",".to_owned()]),
            retrigger_characters: Some(vec![")".to_owned()]),
            ..SignatureHelpOptions::default()
        }),
        // Expand-selection has nothing to take away — in a Ruby file the command does
        // nothing at all today — so this one is pure addition.
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        // `prepareProvider` is the half of this that matters: it is what lets ya-lsp
        // answer "not here" *before* the editor asks the user for a new name, which is the only
        // point at which declining costs the user nothing. A client that does not support it
        // sends `textDocument/rename` straight off, so every refusal is reachable from both.
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        // The kinds are listed rather than `Simple(true)` because a client filters on
        // them *before* it asks: VS Code's Refactor… menu sends `only: ["refactor"]`, and a
        // server that advertises no kinds is asked nothing. Listing the two that are implemented
        // and no more is the same rule this module opens with, one level down — an advertised
        // `quickfix` would put an empty entry under the lightbulb on every diagnostic.
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![
                CodeActionKind::REFACTOR_EXTRACT,
                CodeActionKind::REFACTOR_REWRITE,
            ]),
            resolve_provider: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        // The one capability here that *removes* a fallback rather than replacing
        // an absence: a client with a folding provider stops guessing from indentation, and on
        // well-formatted Ruby that guess is decent. So `analysis::ranges` covers the shapes the
        // guess gets right as well as the ones it cannot see, and answers `null` — never an
        // empty array — where it found nothing, which is what hands the guess back.
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        // `resolveProvider: false`, and it is the only interesting field here: a link's target
        // is known at the moment the link is made — the graph either has the required file or
        // it does not — so there is nothing a second round trip could add. Advertising the
        // resolve step would buy the client a request per link for an answer it already has.
        //
        // Nothing is taken away by this one either. An editor underlines a `require` path in a
        // Ruby file today only if a grammar guessed at it, and none does.
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        // A bare `true` rather than the options struct, because the struct's only field is
        // `workDoneProgress` and this server reports progress for indexing rather than for a
        // request. The type hierarchy is announced two fields further out — in `Advertised`,
        // which exists because `lsp-types` 0.97 can spell this capability and not that one.
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        // `resolveProvider: true`, and it is the opposite decision from the document link's for
        // the opposite reason. A link's target is known the moment the link is made; a hint's
        // tooltip is a sentence nobody sees until they point at one, and building it for every
        // hint on screen would put a paragraph of markdown on the wire per line of the file. The
        // hints that carry no tooltip at all — the resolved tier, which is most of them — ship
        // no `data`, so the client never asks about those either.
        //
        // Nothing is taken away by this one: no editor draws a Ruby type in the margin today.
        inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
            InlayHintOptions {
                resolve_provider: Some(true),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        ))),
        // The legend is the wire contract twice over: a client reads every token's type
        // as an index into this list, so it must be exactly `tokens::LEGEND` and in exactly its
        // order — a mismatch recolours every token in every file, consistently, which is the
        // hardest kind of wrong to see. `the_legend_the_client_is_sent_is_the_one_the_tokens_are
        // _numbered_against` is what holds the two together.
        //
        // `full: Bool(true)` and no `delta`, deliberately: a delta is a wire optimisation over
        // an answer the server would have computed anyway, paid for with a cache of every
        // response sent per document and an id to invalidate on every edit. See `tokens`.
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: crate::analysis::tokens::LEGEND
                        .iter()
                        .map(|name| SemanticTokenType::new(name))
                        .collect(),
                    token_modifiers: Vec::new(),
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: None,
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        )),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                // Announcing support for the notification without
                // acting on it would be a lie the client cannot detect.
                change_notifications: None,
            }),
            file_operations: Some(WorkspaceFileOperationsServerCapabilities::default()),
        }),
        ..ServerCapabilities::default()
    }
}

/// Reported to the client and shown in editor logs.
#[must_use]
pub fn server_info() -> serde_json::Value {
    serde_json::json!({
        "name": "ya-lsp",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// The schema dumps `index.include` can never name, and the only non-Ruby file this server
/// reads.
///
/// `db/*structure.sql` rather than `**/*.sql`, which would sweep up every fixture, seed and
/// migration in a repository for the sake of one file. The shape mirrors Rails' own
/// `schema_dump`, which names the primary database's dump `structure.sql` and every other one
/// `<database>_structure.sql` — the same pair of names `rails::is_structure` matches, and it is
/// that predicate rather than this glob that has the last word, exactly as `Workspace::indexes`
/// does for the Ruby patterns.
const SCHEMA_DUMP_GLOB: &str = "db/*structure.sql";

/// The id the watcher registration is made under.
///
/// Fixed rather than generated: the protocol identifies a registration by this string, so
/// anything that later unregisters it has to be able to name the same one.
const WATCHED_FILES_ID: &str = "ya-lsp-watched-files";

/// Ask the client to watch the project's `ya-lsp.toml` and the files it indexes.
///
/// The protocol has no static form for file watching — `initialize` cannot announce it, which is
/// why this is not in `server_capabilities` — so `client/registerCapability` is the only way to
/// ask, and `None` here means the client did not say it accepts one. Without it, reload works
/// only in an editor whose extension supplies a watcher of its own.
///
/// The Ruby patterns are `index.include` itself, so a project that widened it to cover `sig/`
/// gets its signatures watched too. `index.exclude` has no counterpart here — LSP watchers
/// cannot say "not this" — so the registration is deliberately the *wider* of the two, and
/// `Workspace::indexes` narrows it back down on arrival. Watching too much costs notifications
/// the server drops; watching too little is a file that never refreshes.
///
/// **Two of the patterns are constants and neither is indexed**, which is the shape rather than
/// an exception. `ya-lsp.toml` is watched and never indexed, and [`SCHEMA_DUMP_GLOB`] is beside
/// it for the same reason: a `db/structure.sql` is read
/// by the generator pass, is not Ruby, and must never reach rubydex. Being constants is what
/// makes them safe here — this registration is made once, at `initialize`, and is never made
/// again, so anything derived from a configuration the user can reload would be stale for the
/// life of the process.
#[must_use]
pub fn watched_files(
    root: &Path,
    index: &IndexConfig,
    capabilities: &ClientCapabilities,
) -> Option<Registration> {
    let watched = capabilities
        .workspace
        .as_ref()?
        .did_change_watched_files
        .as_ref()?;
    if watched.dynamic_registration != Some(true) {
        return None;
    }
    let relative = watched.relative_pattern_support == Some(true);
    let options = DidChangeWatchedFilesRegistrationOptions {
        watchers: [CONFIG_FILE_NAME, SCHEMA_DUMP_GLOB]
            .into_iter()
            .chain(index.include.iter().map(String::as_str))
            .map(|pattern| FileSystemWatcher {
                glob_pattern: watch_glob(root, pattern, relative),
                // All three kinds, which is what omitting `kind` means. A deleted `ya-lsp.toml`
                // is a configuration change — it means back to the defaults — and so is one
                // written for the first time in a project that never had one; a deleted `.rb`
                // is the one case the index cannot reach any other way, since nothing else ever
                // says a declaration has gone.
                kind: None,
            })
            .collect(),
    };
    Some(Registration {
        id: WATCHED_FILES_ID.to_owned(),
        method: "workspace/didChangeWatchedFiles".to_owned(),
        // Infallible for this shape — every field in it is a string — but the type is a
        // `Result`, and a registration whose options went missing asks the client to watch
        // nothing at all. Better no registration than one that silently watches nothing.
        register_options: Some(serde_json::to_value(options).ok()?),
    })
}

/// One pattern the watcher is registered with, relative to `root`.
///
/// Relative when the client says it takes one (LSP 3.17). The base there is a URI rather than
/// glob syntax, so a root holding `[`, `{`, `*` or `?` — a directory named `[wip]` is enough —
/// cannot be read as a pattern. The absolute form has no defence against that: LSP's glob
/// syntax defines no escape. Its separators are `/` on every platform, Windows included, which
/// is why the path is not simply handed over as the OS spells it.
fn watch_glob(root: &Path, pattern: &str, relative_patterns: bool) -> GlobPattern {
    if relative_patterns
        && let Some(base) = DocUri::from_path(root).and_then(|uri| uri.to_lsp().ok())
    {
        return GlobPattern::Relative(RelativePattern {
            base_uri: OneOf::Right(base),
            pattern: pattern.to_owned(),
        });
    }
    GlobPattern::String(root.join(pattern).to_string_lossy().replace('\\', "/"))
}

#[must_use]
pub fn sync_kind(capabilities: &ServerCapabilities) -> Option<TextDocumentSyncKind> {
    match capabilities.text_document_sync.as_ref()? {
        TextDocumentSyncCapability::Kind(kind) => Some(*kind),
        TextDocumentSyncCapability::Options(options) => options.change,
    }
}

/// The language ids a document registration claims.
///
/// The client's own selector names the same two — `package.json` contributes `erb` with the id and
/// the extensions ruby-lsp uses — and `vscode_manifest.rs` is what holds the two lists together.
/// A registration naming only `ruby` would leave a Rails engine's templates unclaimed, which is
/// the same silence this mechanism exists to end.
pub const LANGUAGE_IDS: [&str; 2] = ["ruby", "erb"];

/// The prefix every document registration's id is made under.
///
/// Fixed and recognisable, for the same reason [`WATCHED_FILES_ID`] is fixed and for one more:
/// an extension driving several servers has to be able to tell *these* registrations from the
/// watcher's, because what it does about them is the opposite. The watcher's registration is
/// forwarded untouched; a document registration may have to be narrowed, since two folders on one
/// Ruby ask for the same gem roots and two servers answering one hover is the thing the narrow
/// selector was protecting against in the first place.
pub const DOCUMENTS_ID_PREFIX: &str = "ya-lsp-documents/";

/// A document capability that can be asked for a second time, over a wider set of files than the
/// client's own selector named.
///
/// Three names, never the same word twice: `advertised` is what the *server* capability goes out
/// under in [`advertised`], `client` is what the *client* capability comes back under in
/// `textDocument`, and `method` is the one a registration is made with — which for the two
/// hierarchies is the `prepare` half rather than any of the four requests that follow it, and for
/// semantic tokens is `textDocument/semanticTokens` rather than the `/full` the server answers.
struct Dynamic {
    advertised: &'static str,
    client: &'static str,
    method: &'static str,
}

/// Every request method this server answers that a document selector gates.
///
/// **Generated from, and checked against, [`advertised`]** — `every_advertised_document_capability_
/// can_be_registered_again` is the test, and it is the whole reason this is a table rather than a
/// hand-written list of registrations: a capability advertised and missing from here would be
/// claimed for `didOpen` and silent for that one request, which looks like it is working and is
/// worse than today's file that is silent for everything.
///
/// `completionItem/resolve`, `inlayHint/resolve` and the four hierarchy walks are deliberately
/// absent: the protocol registers each of those through the parent's options, so they arrive with
/// `resolveProvider` and with the `prepare` entry rather than under a method of their own.
const DYNAMIC: [Dynamic; 16] = [
    Dynamic {
        advertised: "hoverProvider",
        client: "hover",
        method: "textDocument/hover",
    },
    Dynamic {
        advertised: "definitionProvider",
        client: "definition",
        method: "textDocument/definition",
    },
    Dynamic {
        advertised: "documentSymbolProvider",
        client: "documentSymbol",
        method: "textDocument/documentSymbol",
    },
    Dynamic {
        advertised: "referencesProvider",
        client: "references",
        method: "textDocument/references",
    },
    Dynamic {
        advertised: "documentHighlightProvider",
        client: "documentHighlight",
        method: "textDocument/documentHighlight",
    },
    Dynamic {
        advertised: "completionProvider",
        client: "completion",
        method: "textDocument/completion",
    },
    Dynamic {
        advertised: "signatureHelpProvider",
        client: "signatureHelp",
        method: "textDocument/signatureHelp",
    },
    Dynamic {
        advertised: "selectionRangeProvider",
        client: "selectionRange",
        method: "textDocument/selectionRange",
    },
    Dynamic {
        advertised: "renameProvider",
        client: "rename",
        method: "textDocument/rename",
    },
    Dynamic {
        advertised: "codeActionProvider",
        client: "codeAction",
        method: "textDocument/codeAction",
    },
    Dynamic {
        advertised: "foldingRangeProvider",
        client: "foldingRange",
        method: "textDocument/foldingRange",
    },
    Dynamic {
        advertised: "documentLinkProvider",
        client: "documentLink",
        method: "textDocument/documentLink",
    },
    Dynamic {
        advertised: "callHierarchyProvider",
        client: "callHierarchy",
        method: "textDocument/prepareCallHierarchy",
    },
    Dynamic {
        advertised: "typeHierarchyProvider",
        client: "typeHierarchy",
        method: "textDocument/prepareTypeHierarchy",
    },
    Dynamic {
        advertised: "inlayHintProvider",
        client: "inlayHint",
        method: "textDocument/inlayHint",
    },
    Dynamic {
        advertised: "semanticTokensProvider",
        client: "semanticTokens",
        method: "textDocument/semanticTokens",
    },
];

/// The advertised capabilities no document registration carries, each with the reason it is here
/// rather than in [`DYNAMIC`].
///
/// A list rather than a silence, because the test that walks [`advertised`] has to fail on a
/// capability nobody has ruled on — and "this one is not a document request" is a ruling.
/// `cfg(test)` because the test below is the only thing that reads it: it is a ruling, and a
/// ruling's whole job is to make the walk over [`advertised`] fail on a capability nobody made.
#[cfg(test)]
const NOT_A_DOCUMENT: [&str; 4] = [
    // Negotiated once, inside the handshake. A registration cannot change the coordinates the
    // answers already went out in.
    "positionEncoding",
    // Four notifications rather than one capability, built by `synchronization` below.
    "textDocumentSync",
    // A search of the whole graph. The request names no document, so no selector gates it, and it
    // has been answering inside gems since it shipped.
    "workspaceSymbolProvider",
    // Workspace folders and file operations. Neither is about a document.
    "workspace",
];

/// One registration the client will take, waiting for the files it should cover.
///
/// The selector is deliberately *not* in here. What the server has answers about is the gem roots,
/// Ruby's own library and the RBS beside them, and none of those is known until the bundle has
/// been discovered — which happens on the analysis thread, long after the handshake this is
/// negotiated in. A `Registration` with no selector would fall back to the client's own, which is
/// the narrow folder, so the half-built value is a separate type that cannot be sent by mistake.
#[derive(Debug, Clone)]
pub struct Requested {
    method: &'static str,
    options: serde_json::Map<String, serde_json::Value>,
}

/// What this client will accept a second registration of, derived from what the server advertised.
///
/// Empty when the client cannot dynamically register text synchronisation, and that is a
/// deliberate all-or-nothing: a document the client never sends `didOpen` for cannot be asked
/// about, so registering the sixteen requests over it would claim files and answer nothing.
///
/// Every entry is filtered by the client's *own* `dynamicRegistration` flag for that capability.
/// Read out of the serialized `textDocument` object rather than through sixteen typed accessor
/// chains: the flag is spelled identically in every one of them, and the table above is already
/// the place the two vocabularies are matched up.
#[must_use]
pub fn dynamic_documents(
    encoding: PositionEncoding,
    capabilities: &ClientCapabilities,
) -> Vec<Requested> {
    // `Null` for a client that said nothing about documents at all, which then answers `Null` to
    // every lookup below and so declines everything — one path rather than two.
    let text_document = capabilities
        .text_document
        .as_ref()
        .and_then(|it| serde_json::to_value(it).ok())
        .unwrap_or(serde_json::Value::Null);
    let dynamic = |client: &str| {
        text_document[client]["dynamicRegistration"] == serde_json::Value::Bool(true)
    };
    if !dynamic("synchronization") {
        return Vec::new();
    }

    let advertised = serde_json::to_value(advertised(encoding)).unwrap_or(serde_json::Value::Null);
    let mut requested: Vec<Requested> = DYNAMIC
        .iter()
        .filter(|entry| dynamic(entry.client))
        .map(|entry| Requested {
            method: entry.method,
            // The advertised value *is* the registration's options, minus the selector: a
            // `CompletionOptions` and a `CompletionRegistrationOptions` differ by exactly that
            // field. A capability advertised as a bare `true` carries nothing, so it registers
            // with nothing, and `every_advertised_document_capability_can_be_registered_again` is
            // what rules out the third case of a method here that is advertised nowhere.
            options: match &advertised[entry.advertised] {
                serde_json::Value::Object(options) => options.clone(),
                _ => serde_json::Map::new(),
            },
        })
        .collect();
    // Synchronisation last, and `didOpen` last of all inside it: registering `didOpen` is what
    // walks the already-open documents and sends one for every file the new selector newly
    // matches, so every provider above is in place before the client is told the file exists.
    requested.extend(synchronization(&advertised["textDocumentSync"]));
    requested
}

/// The four notifications `textDocumentSync` is registered as, in the order they must arrive.
///
/// `syncKind` and `includeText` are carried across from the advertised options rather than
/// restated: a registration that dropped `syncKind` would leave the client at its own default and
/// `position::Rebase` would be handed whole-document changes it is not written for.
fn synchronization(sync: &serde_json::Value) -> Vec<Requested> {
    // Reads rather than questions: `textDocumentSync` is a constant in `server_capabilities`, so
    // both fields are always there. `a_registration_carries_the_options_the_handshake_announced` is
    // what keeps that true — it asserts the values, so a change to the constant's shape fails there
    // rather than shipping a registration with a null in it.
    let change = serde_json::Map::from_iter([("syncKind".to_owned(), sync["change"].clone())]);
    let save = serde_json::Map::from_iter([(
        "includeText".to_owned(),
        sync["save"]["includeText"].clone(),
    )]);
    vec![
        Requested {
            method: "textDocument/didChange",
            options: change,
        },
        Requested {
            method: "textDocument/didClose",
            options: serde_json::Map::new(),
        },
        Requested {
            method: "textDocument/didSave",
            options: save,
        },
        Requested {
            method: "textDocument/didOpen",
            options: serde_json::Map::new(),
        },
    ]
}

/// The registrations to send, once it is known which files the server has answers about.
///
/// `prefixes` are directory URIs — the gem roots, Ruby's own library, the RBS root — and the
/// workspace's own is **not** among them: the client already claimed that with the selector it
/// was built with, and a second provider over the same file is one server answering a hover
/// twice. Empty `prefixes` means there is nothing to widen to and nothing is sent.
#[must_use]
pub fn document_registrations(requested: &[Requested], prefixes: &[String]) -> Vec<Registration> {
    if prefixes.is_empty() {
        return Vec::new();
    }
    let selector: Vec<serde_json::Value> = prefixes
        .iter()
        .flat_map(|prefix| {
            LANGUAGE_IDS.iter().map(move |language| {
                serde_json::json!({
                    "scheme": "file",
                    "language": language,
                    // The protocol's own relative pattern, whose `baseUri` is a URI *string*.
                    // `vscode-languageclient` recognises this shape and nothing else, and what it
                    // does with anything else is not to ignore the pattern but to drop it — which
                    // widens the filter to scheme and language alone, claiming every Ruby file
                    // the editor has open anywhere.
                    "pattern": { "baseUri": prefix.trim_end_matches('/'), "pattern": "**/*" },
                })
            })
        })
        .collect();

    requested
        .iter()
        .map(|entry| {
            let mut options = entry.options.clone();
            options.insert(
                "documentSelector".to_owned(),
                serde_json::Value::Array(selector.clone()),
            );
            Registration {
                id: format!("{DOCUMENTS_ID_PREFIX}{}", entry.method),
                method: entry.method.to_owned(),
                register_options: Some(serde_json::Value::Object(options)),
            }
        })
        .collect()
}

/// A client that accepts every dynamic registration this server asks for, for the suite.
///
/// **Built from [`DYNAMIC`] rather than written out.** VS Code's client says yes to all of these,
/// so this is the realistic shape — and deriving it from the table is what makes a row added there
/// covered by every test that drives a harness, instead of by one somebody has to remember to
/// widen.
#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
#[must_use]
pub(crate) fn every_dynamic_registration_accepted() -> ClientCapabilities {
    let mut text_document = serde_json::Map::new();
    for client in DYNAMIC
        .iter()
        .map(|entry| entry.client)
        .chain(std::iter::once("synchronization"))
    {
        let mut capability = serde_json::json!({ "dynamicRegistration": true });
        if client == "semanticTokens" {
            // The four fields `lsp-types` does not make optional here, because the protocol does
            // not either: a client that says it takes semantic tokens has to say which ones.
            capability["requests"] = serde_json::json!({ "full": true });
            capability["tokenTypes"] = serde_json::json!(crate::analysis::tokens::LEGEND);
            capability["tokenModifiers"] = serde_json::json!([]);
            capability["formats"] = serde_json::json!(["relative"]);
        }
        text_document.insert(client.to_owned(), capability);
    }
    serde_json::from_value(serde_json::json!({ "textDocument": text_document }))
        .expect("the client capabilities every entry in the table names")
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capability_lsp_types_cannot_spell_still_goes_out_with_the_rest() {
        // `ServerCapabilities` has no `typeHierarchyProvider` field in any published version of
        // the crate, so this is the one capability that is added on the way to JSON. The second
        // half of the assertion is the point: a flatten that stopped flattening would announce
        // *only* the type hierarchy, and every other feature would silently stop being offered.
        let advertised =
            serde_json::to_value(advertised(PositionEncoding::Utf8)).expect("plain data");
        assert_eq!(advertised["typeHierarchyProvider"], serde_json::json!(true));
        assert_eq!(advertised["hoverProvider"], serde_json::json!(true));
        assert!(advertised["completionProvider"].is_object());
        assert!(advertised["textDocumentSync"].is_object());
    }

    #[test]
    fn the_v0_3_0_rename_provider_asks_to_be_consulted_before_the_editor_prompts() {
        // A bare `renameProvider: true` would still work, and would move every refusal to
        // *after* the user has typed a new name. `prepareProvider` is what buys the earlier
        // question, and it is the only option this provider has.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        let OneOf::Right(rename) = capabilities.rename_provider.expect("a rename provider") else {
            panic!("announced without options, so nothing asks before the prompt");
        };
        assert_eq!(rename.prepare_provider, Some(true));
    }

    #[test]
    fn the_m5_provider_is_announced_with_its_triggers() {
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        let completion = capabilities
            .completion_provider
            .expect("a completion provider");
        assert_eq!(completion.resolve_provider, Some(true));
        // Without `.` in this list a client narrows the list it already had instead of asking
        // again, and `foo.` would complete against whatever `foo` matched.
        let triggers = completion.trigger_characters.expect("trigger characters");
        assert!(triggers.contains(&".".to_owned()));
        assert!(triggers.contains(&":".to_owned()));
    }

    #[test]
    fn the_v0_3_0_highlight_provider_is_announced() {
        // Announced without options: `documentHighlight` has none, and the whole of the
        // decision is that a client stops matching words itself the moment it is told.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        assert!(capabilities.document_highlight_provider.is_some());
    }

    #[test]
    fn the_v0_3_0_provider_is_announced_with_its_triggers() {
        // `(` and `,` are where an argument list gains an argument; `)` only re-triggers, so
        // the popup is asked once more as the call closes and takes ya-lsp's `null` as its cue
        // to go away. A client is asked nothing at all for a character that is on neither list.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        let help = capabilities
            .signature_help_provider
            .expect("a signature help provider");
        assert_eq!(
            help.trigger_characters,
            Some(vec!["(".to_owned(), ",".to_owned()])
        );
        assert_eq!(help.retrigger_characters, Some(vec![")".to_owned()]));
    }

    #[test]
    fn the_v0_3_0_range_providers_are_announced() {
        // Neither takes options, and the two are announced for opposite reasons: expand-selection
        // has nothing to displace in a Ruby file, while folding takes the editor's indentation
        // guess out of play the moment it is told — which is why `analysis::ranges` covers what
        // that guess got right as well as what it could not see.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        assert!(capabilities.selection_range_provider.is_some());
        assert!(capabilities.folding_range_provider.is_some());
    }

    #[test]
    fn the_call_hierarchy_is_announced_and_the_type_hierarchy_beside_it() {
        // The two hierarchies are announced through different mechanisms — one is a field
        // `lsp-types` has and the other is flattened in beside it — and a client that gets one
        // and not the other shows a "show call hierarchy" command that reports no results. So
        // both are asserted here, in one test, against the struct that goes out on the wire.
        let advertised = serde_json::to_value(advertised(PositionEncoding::Utf16))
            .expect("the advertised capabilities serialize");
        assert_eq!(advertised["callHierarchyProvider"], true);
        assert_eq!(advertised["typeHierarchyProvider"], true);
    }

    #[test]
    fn the_document_link_provider_says_it_resolves_nothing() {
        // `Some(false)` rather than `None`, and the difference is what the client does with it:
        // an absent field and a `false` mean the same to the protocol, but a server that has
        // *decided* not to resolve and one that forgot to answer look identical on the wire.
        // The target is known when the link is made, so the round trip would buy nothing.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        let links = capabilities
            .document_link_provider
            .expect("a document link provider");
        assert_eq!(links.resolve_provider, Some(false));
    }

    #[test]
    fn the_inlay_hint_provider_says_it_resolves_the_tooltip() {
        // The opposite answer from the document link's, for the opposite reason. A link's
        // target is known the moment the link is made, so a resolve step would buy a round trip
        // per link and nothing else; a hint's tooltip is a paragraph nobody sees until they
        // point at one, and shipping it eagerly puts markdown on the wire for every line of the
        // file on every scroll.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        let Some(OneOf::Right(InlayHintServerCapabilities::Options(hints))) =
            capabilities.inlay_hint_provider
        else {
            panic!("an inlay hint provider with options");
        };
        assert_eq!(hints.resolve_provider, Some(true));
    }

    #[test]
    fn the_v0_4_0_semantic_token_legend_is_the_one_the_tokens_are_numbered_against() {
        // The legend *is* the wire contract: a client reads every token's type as an index into
        // this list. A mismatch between it and `tokens::Kind` recolours every token in every
        // file, consistently and plausibly, which is the hardest kind of wrong to notice — so
        // the two are asserted against each other rather than each against a hand-written list.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        let SemanticTokensServerCapabilities::SemanticTokensOptions(options) = capabilities
            .semantic_tokens_provider
            .expect("a semantic tokens provider")
        else {
            panic!("registered dynamically, which this server does not do");
        };

        let sent: Vec<String> = options
            .legend
            .token_types
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect();
        assert_eq!(sent, crate::analysis::tokens::LEGEND);
        assert!(
            options.legend.token_modifiers.is_empty(),
            "a modifier nothing sends is a promise nothing keeps"
        );
        // The full document and nothing else. A delta is a wire optimisation over an answer the
        // server computes anyway; announcing it would oblige ya-lsp to keep every response it
        // has sent, per document, keyed by an id it must invalidate on every edit.
        assert!(matches!(
            options.full,
            Some(lsp_types::SemanticTokensFullOptions::Bool(true))
        ));
        assert!(options.range.is_none());
    }

    #[test]
    fn the_m4_providers_are_announced() {
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        assert!(capabilities.references_provider.is_some());
        assert!(capabilities.workspace_symbol_provider.is_some());
    }

    #[test]
    fn text_sync_is_incremental_and_the_m2_providers_are_announced() {
        // These four are a contract with the client, fixed at `initialize` and never
        // renegotiated. Getting `change` wrong is the expensive one: the client would send
        // ranges while the server expected whole buffers, and every edit would corrupt the
        // document with no error anywhere.
        let capabilities = server_capabilities(PositionEncoding::Utf8);
        assert_eq!(
            sync_kind(&capabilities),
            Some(TextDocumentSyncKind::INCREMENTAL)
        );
        assert!(capabilities.hover_provider.is_some());
        assert!(capabilities.definition_provider.is_some());
        assert!(capabilities.document_symbol_provider.is_some());
    }

    // ------------------------------------------------------------------ the config watcher

    /// Client capabilities that answer `didChangeWatchedFiles` the way `answer` says.
    fn watching(
        dynamic_registration: Option<bool>,
        relative_pattern_support: Option<bool>,
    ) -> ClientCapabilities {
        ClientCapabilities {
            workspace: Some(lsp_types::WorkspaceClientCapabilities {
                did_change_watched_files: Some(
                    lsp_types::DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration,
                        relative_pattern_support,
                    },
                ),
                ..lsp_types::WorkspaceClientCapabilities::default()
            }),
            ..ClientCapabilities::default()
        }
    }

    fn watchers(registration: &Registration) -> Vec<FileSystemWatcher> {
        let options: DidChangeWatchedFilesRegistrationOptions = serde_json::from_value(
            registration
                .register_options
                .clone()
                .expect("a registration carries its options"),
        )
        .expect("the options are the shape the protocol names");
        options.watchers
    }

    #[test]
    fn the_watchers_cover_the_config_and_everything_the_index_includes() {
        // Without this the server never asks anyone to watch anything: `ya-lsp.toml` reloads
        // only in the one editor that brought its own watcher, and no editor anywhere notices a
        // `git checkout`.
        let root = Path::new("/tmp/ya-lsp-watch/project");
        let index = IndexConfig {
            include: vec!["**/*.rb".to_owned(), "sig/**/*.rbs".to_owned()],
            ..IndexConfig::default()
        };
        let registration = watched_files(root, &index, &watching(Some(true), None))
            .expect("a watcher is registered");
        assert_eq!(registration.method, "workspace/didChangeWatchedFiles");
        assert_eq!(registration.id, WATCHED_FILES_ID);
        // The schema dump is a constant and not derived from this configuration, which is the
        // property that makes it correct: this runs once, at `initialize`, and a pattern
        // computed from a `ya-lsp.toml` the user can reload would be stale for the life of the
        // process. It also cannot be spelled by `index.include`, which is Ruby's shapes.
        assert!(
            !index
                .include
                .iter()
                .any(|pattern| pattern == SCHEMA_DUMP_GLOB)
        );

        let watchers = watchers(&registration);
        // `kind` unset is create|change|delete. A `ya-lsp.toml` that is deleted, or written for
        // the first time, changes the configuration exactly as much as an edit does — and a
        // deleted `.rb` is the only way the index ever hears that a declaration has gone.
        assert!(watchers.iter().all(|watcher| watcher.kind.is_none()));
        assert_eq!(
            watchers
                .iter()
                .map(|watcher| watcher.glob_pattern.clone())
                .collect::<Vec<_>>(),
            vec![
                // A client without relative patterns gets absolute ones, with `/` separators.
                GlobPattern::String("/tmp/ya-lsp-watch/project/ya-lsp.toml".to_owned()),
                // The two constants come first and neither is `index.include`'s: both name a
                // file this server reads and never indexes.
                GlobPattern::String("/tmp/ya-lsp-watch/project/db/*structure.sql".to_owned()),
                GlobPattern::String("/tmp/ya-lsp-watch/project/**/*.rb".to_owned()),
                GlobPattern::String("/tmp/ya-lsp-watch/project/sig/**/*.rbs".to_owned()),
            ],
            "the config plus index.include verbatim: a project that widened it to cover sig/ \
             has its signatures watched too"
        );
    }

    #[test]
    fn a_client_that_takes_relative_patterns_gets_one() {
        // The absolute form is glob syntax all the way down, so a root under a directory named
        // `[wip]` would be read as a character class and match nothing. The relative form's base
        // is a URI, so the only glob in it is the part we wrote.
        let root = Path::new("/tmp/ya-lsp-watch/[wip]/project");
        let registration = watched_files(
            root,
            &IndexConfig::default(),
            &watching(Some(true), Some(true)),
        )
        .expect("a watcher is registered");
        let base = DocUri::from_path(root).expect("an absolute root").to_lsp();
        let relative = |pattern: &str| {
            GlobPattern::Relative(RelativePattern {
                base_uri: OneOf::Right(base.clone().expect("a uri")),
                pattern: pattern.to_owned(),
            })
        };
        // Derived from the default rather than spelled out: what this test is about is the
        // *form* of each pattern, and `the_watchers_cover_the_config_and_everything_the_index_
        // includes` above already pins that the list is `index.include` verbatim.
        let expected: Vec<GlobPattern> = [CONFIG_FILE_NAME.to_owned(), SCHEMA_DUMP_GLOB.to_owned()]
            .into_iter()
            .chain(IndexConfig::default().include)
            .map(|pattern| relative(&pattern))
            .collect();
        assert_eq!(
            watchers(&registration)
                .iter()
                .map(|watcher| watcher.glob_pattern.clone())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn a_relative_root_falls_back_to_the_pattern_it_can_still_spell() {
        // `workspace_root` ends at `.` when the client sent no folder and the process has no
        // working directory. There is no URI for that, so there is no relative pattern either —
        // and answering `None` here would drop the watcher over a case the absolute form
        // handles.
        let registration = watched_files(
            Path::new("."),
            &IndexConfig::default(),
            &watching(Some(true), Some(true)),
        )
        .expect("a watcher is registered");
        assert_eq!(
            watchers(&registration)[0].glob_pattern,
            GlobPattern::String("./ya-lsp.toml".to_owned())
        );
    }

    #[test]
    fn a_client_that_cannot_be_asked_is_not_asked() {
        // Nothing to fall back on: the protocol has no static form for file watching, so a
        // client that does not take a dynamic registration cannot be given a watcher at all.
        // The server says so in the log rather than leaving the user to discover it by editing
        // `ya-lsp.toml`, or checking out a branch, and watching nothing happen.
        let root = Path::new("/tmp/ya-lsp-watch/project");
        let index = IndexConfig::default();
        assert!(watched_files(root, &index, &ClientCapabilities::default()).is_none());
        assert!(
            watched_files(
                root,
                &index,
                &ClientCapabilities {
                    workspace: Some(lsp_types::WorkspaceClientCapabilities::default()),
                    ..ClientCapabilities::default()
                }
            )
            .is_none(),
            "a workspace section that says nothing about watched files is still a no"
        );
        assert!(watched_files(root, &index, &watching(Some(false), None)).is_none());
        assert!(
            watched_files(root, &index, &watching(None, Some(true))).is_none(),
            "relative patterns without dynamic registration are not an offer to watch"
        );
    }

    #[test]
    fn the_sync_kind_is_read_out_of_either_shape_the_protocol_allows() {
        // `sync_kind` is how the test above checks the contract, so it has to read both
        // spellings or the assertion could be passing on a `None` it produced itself.
        // `textDocumentSync` is either a bare kind or an options object, and swapping this
        // server to the options form must not quietly turn that test vacuous.
        assert_eq!(
            sync_kind(&ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    lsp_types::TextDocumentSyncOptions {
                        change: Some(TextDocumentSyncKind::FULL),
                        ..lsp_types::TextDocumentSyncOptions::default()
                    }
                )),
                ..ServerCapabilities::default()
            }),
            Some(TextDocumentSyncKind::FULL)
        );
        assert_eq!(
            sync_kind(&ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::NONE
                )),
                ..ServerCapabilities::default()
            }),
            Some(TextDocumentSyncKind::NONE),
            "the bare form, which this server does not send but a reader may meet"
        );
        assert_eq!(sync_kind(&ServerCapabilities::default()), None);
    }

    /// The table that generates the registration must cover everything the handshake advertised.
    ///
    /// This is the test the whole mechanism turns on. Text synchronisation is registrable on its
    /// own, so a capability missing from `DYNAMIC` gets a gem file **claimed for `didOpen` and
    /// silent for that one request** — which is worse than the file being silent for everything,
    /// because it looks like it is working. `NOT_A_DOCUMENT` is the other half: a capability nobody
    /// has ruled on fails here rather than being quietly left out.
    #[test]
    fn every_advertised_document_capability_can_be_registered_again() {
        let advertised = serde_json::to_value(advertised(PositionEncoding::Utf8))
            .expect("the advertised capabilities serialize");
        let advertised = advertised.as_object().expect("an object of capabilities");

        for name in advertised.keys() {
            assert!(
                DYNAMIC.iter().any(|entry| entry.advertised == name)
                    || NOT_A_DOCUMENT.contains(&name.as_str()),
                "{name} is advertised and nothing says whether a registration can carry it"
            );
        }
        for entry in &DYNAMIC {
            assert!(
                advertised.contains_key(entry.advertised),
                "{} is registered again and never advertised in the first place",
                entry.advertised
            );
        }
        for name in NOT_A_DOCUMENT {
            assert!(
                advertised.contains_key(name),
                "{name} is ruled out of the registration and is not a capability at all"
            );
        }
    }

    #[test]
    fn a_registration_claims_every_root_the_server_answers_about() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
        );
        let roots = [
            "file:///gems/activerecord-8.1.3.1/".to_owned(),
            "file:///ruby/4.0.0/".to_owned(),
        ];
        let registrations = document_registrations(&requested, &roots);

        assert_eq!(
            registrations.len(),
            DYNAMIC.len() + 4,
            "sixteen requests and the four halves of text synchronisation"
        );
        let hover = registrations
            .iter()
            .find(|registration| registration.method == "textDocument/hover")
            .expect("hover is registered again");
        let selector = hover.register_options.as_ref().expect("options")["documentSelector"]
            .as_array()
            .expect("a selector is a list of filters")
            .clone();

        assert_eq!(
            selector.len(),
            4,
            "two roots times the two languages served"
        );
        for filter in &selector {
            assert_eq!(filter["scheme"], "file");
            assert_eq!(
                filter["pattern"]["pattern"], "**/*",
                "everything under the root, which is what the server indexed"
            );
        }
        let claimed: Vec<&str> = selector
            .iter()
            .filter(|filter| filter["language"] == "ruby")
            .map(|filter| filter["pattern"]["baseUri"].as_str().expect("a URI string"))
            .collect();
        assert_eq!(
            claimed,
            vec!["file:///gems/activerecord-8.1.3.1", "file:///ruby/4.0.0"],
            "a trailing slash would make the pattern `.../gems//**/*`"
        );
        let languages: std::collections::BTreeSet<&str> = selector
            .iter()
            .map(|filter| filter["language"].as_str().expect("a language id"))
            .collect();
        assert_eq!(
            languages,
            LANGUAGE_IDS.into_iter().collect(),
            "a registration naming only Ruby leaves an engine's templates unclaimed"
        );
    }

    /// The pattern is the protocol's own shape, and this is what a client does with anything else.
    ///
    /// `vscode-languageclient` recognises a `baseUri` that is a **string** and drops every other
    /// shape — and a dropped pattern does not narrow, it *widens*, to language and scheme alone.
    /// So getting this wrong claims every Ruby file the editor has open anywhere, which is the
    /// failure the narrow per-folder selector exists to prevent, arriving from the server instead.
    #[test]
    fn the_pattern_is_a_uri_string_the_client_will_recognise() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
        );
        let registrations = document_registrations(&requested, &["file:///gems/".to_owned()]);
        let filter =
            &registrations[0].register_options.as_ref().expect("options")["documentSelector"][0];

        assert!(
            filter["pattern"]["baseUri"].is_string(),
            "an object here is what silently becomes no pattern at all"
        );
        assert!(
            filter["pattern"].get("base").is_none(),
            "`base` is the editor's own shape and is exactly what the client refuses"
        );
    }

    #[test]
    fn a_registration_carries_the_options_the_handshake_announced() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
        );
        let registrations = document_registrations(&requested, &["file:///gems/".to_owned()]);
        let options = |method: &str| {
            registrations
                .iter()
                .find(|registration| registration.method == method)
                .unwrap_or_else(|| panic!("{method} is registered"))
                .register_options
                .clone()
                .expect("options")
        };

        // Without these the popup never re-opens on a typed `.` inside a gem, which is the one
        // request whose trigger is the whole feature.
        assert_eq!(
            options("textDocument/completion")["triggerCharacters"],
            serde_json::json!([".", ":", "@", "$"])
        );
        assert_eq!(options("textDocument/completion")["resolveProvider"], true);
        // The legend is read as a list of indices, so a registration without it recolours every
        // token in every gem file, consistently.
        assert_eq!(
            options("textDocument/semanticTokens")["legend"]["tokenTypes"]
                .as_array()
                .expect("the legend")
                .len(),
            crate::analysis::tokens::LEGEND.len()
        );
        // A registration that dropped `syncKind` leaves the client at its own default, and
        // `position::Rebase` is handed whole-document changes it is not written for.
        assert_eq!(
            options("textDocument/didChange")["syncKind"],
            serde_json::json!(TextDocumentSyncKind::INCREMENTAL)
        );
        assert_eq!(options("textDocument/didSave")["includeText"], false);
        assert_eq!(options("textDocument/rename")["prepareProvider"], true);
        assert_eq!(options("textDocument/inlayHint")["resolveProvider"], true);
    }

    /// `didOpen` is registered last, because registering it is what sends the notifications.
    ///
    /// The client's own `didOpen` feature **back-fills** — it walks the documents already open and
    /// sends one for every file the new selector newly matches — so a gem file the user is looking
    /// at right now starts answering at registration rather than at the next tab switch. Every
    /// provider has to be in place before that happens.
    #[test]
    fn synchronisation_is_registered_after_the_requests_and_did_open_last_of_all() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
        );
        let methods: Vec<&str> = requested.iter().map(|entry| entry.method).collect();

        assert_eq!(methods.last(), Some(&"textDocument/didOpen"));
        let sync = methods
            .iter()
            .position(|method| method.starts_with("textDocument/did"))
            .expect("synchronisation is in the list");
        assert_eq!(sync, DYNAMIC.len(), "every request comes first");
    }

    #[test]
    fn every_registration_is_named_so_it_can_be_taken_back() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
        );
        let registrations = document_registrations(&requested, &["file:///gems/".to_owned()]);
        let ids: std::collections::BTreeSet<&str> = registrations
            .iter()
            .map(|registration| registration.id.as_str())
            .collect();

        assert_eq!(
            ids.len(),
            registrations.len(),
            "a reused id replaces a live provider"
        );
        assert!(
            ids.iter().all(|id| id.starts_with(DOCUMENTS_ID_PREFIX)),
            "the prefix is how an extension driving several servers tells these from the watcher's"
        );
        assert_ne!(
            *ids.iter().next().expect("at least one"),
            WATCHED_FILES_ID,
            "the watcher's registration is forwarded untouched and must not collide"
        );
    }

    /// A client that will not dynamically register `didOpen` is asked for nothing at all.
    ///
    /// All-or-nothing on purpose: a file the client never says is open cannot be asked about, so
    /// the sixteen requests over it would claim files and answer nothing. What such a client gets
    /// is exactly today's behaviour, plus one line saying what it does not have.
    #[test]
    fn a_client_that_will_not_sync_dynamically_is_asked_for_nothing() {
        let capabilities: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "textDocument": {
                "synchronization": { "dynamicRegistration": false },
                "hover": { "dynamicRegistration": true },
            }
        }))
        .expect("client capabilities");

        assert!(dynamic_documents(PositionEncoding::Utf16, &capabilities).is_empty());
        assert!(
            dynamic_documents(PositionEncoding::Utf16, &ClientCapabilities::default()).is_empty(),
            "a client that says nothing about textDocument at all"
        );
    }

    /// One capability declined leaves the rest of the batch standing.
    ///
    /// Not merely politeness about the protocol: `doRegisterCapability` rejects the *whole* array
    /// on the first method it has no feature for, so a registration the client never agreed to
    /// would take every method after it down with it.
    #[test]
    fn a_capability_the_client_declines_is_left_out_and_the_rest_stand() {
        let mut accepted = serde_json::to_value(every_dynamic_registration_accepted())
            .expect("client capabilities serialize");
        accepted["textDocument"]["semanticTokens"]["dynamicRegistration"] =
            serde_json::json!(false);
        let capabilities: ClientCapabilities =
            serde_json::from_value(accepted).expect("client capabilities");

        let methods: Vec<&str> = dynamic_documents(PositionEncoding::Utf16, &capabilities)
            .iter()
            .map(|entry| entry.method)
            .collect();

        assert!(!methods.contains(&"textDocument/semanticTokens"));
        assert!(methods.contains(&"textDocument/hover"));
        assert_eq!(methods.len(), DYNAMIC.len() - 1 + 4);
    }

    #[test]
    fn nothing_outside_the_workspace_means_no_registration() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
        );
        assert!(
            document_registrations(&requested, &[]).is_empty(),
            "a project with no bundle has nothing to widen to"
        );
    }
}
