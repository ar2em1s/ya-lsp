//! What the server tells the client it can do.
//!
//! A capability is announced only once it is implemented: advertising a feature we lack makes the
//! editor show an empty result instead of falling back to its own heuristics, which is worse than
//! not advertising at all.

use std::path::{Path, PathBuf};

use lsp_types::{
    CallHierarchyServerCapability, ClientCapabilities, CodeActionKind, CodeActionOptions,
    CodeActionProviderCapability, CompletionOptions, CompletionOptionsCompletionItem,
    DeclarationCapability, DidChangeWatchedFilesRegistrationOptions, DocumentLinkOptions,
    ExecuteCommandOptions, FileOperationFilter, FileOperationPattern, FileOperationPatternKind,
    FileOperationRegistrationOptions, FileSystemWatcher, FoldingRangeProviderCapability,
    GlobPattern, HoverProviderCapability, ImplementationProviderCapability, InlayHintOptions,
    InlayHintServerCapabilities, OneOf, Registration, RelativePattern, RenameOptions, SaveOptions,
    SelectionRangeProviderCapability, SemanticTokenType, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensServerCapabilities,
    ServerCapabilities, SignatureHelpOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability,
    WorkDoneProgressOptions, WorkspaceFileOperationsServerCapabilities,
    WorkspaceFoldersServerCapabilities,
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
/// `lsp-types` 0.97 (the latest release) has a field for `callHierarchyProvider` but none for
/// `typeHierarchyProvider` or `workspace.textDocumentContent`, so those two are added here beside
/// the ones it can spell. A typed struct, not a `serde_json::Map` insertion: this module is the
/// wire contract, and the contract should be impossible to misspell.
///
/// The alternative was `client/registerCapability`, which the protocol allows for both. It would
/// make each feature depend on a client that takes dynamic registrations (the dependency that
/// leaves the file watcher unavailable in several editors) just to work around a missing struct
/// field.
///
/// **The second cannot be flattened, which is why `workspace` is spelled out here.**
/// `typeHierarchyProvider` is a top-level key `ServerCapabilities` lacks, so `#[serde(flatten)]`
/// places it beside the rest with no collision. `textDocumentContent` sits *inside* `workspace`,
/// which `ServerCapabilities` does have, so a second flattened `workspace` field would write the
/// key twice, and a duplicated JSON key resolves to whichever one the reader keeps. So the whole
/// object is built here, from the two halves `lsp-types` can spell and the one it cannot, and
/// [`server_capabilities`] leaves its own `workspace` field `None` instead of writing a value this
/// would override.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Advertised {
    #[serde(flatten)]
    standard: ServerCapabilities,
    type_hierarchy_provider: bool,
    workspace: AdvertisedWorkspace,
}

/// The `workspace` half of the capabilities, with the field `lsp-types` has no name for.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AdvertisedWorkspace {
    workspace_folders: WorkspaceFoldersServerCapabilities,
    file_operations: WorkspaceFileOperationsServerCapabilities,
    text_document_content: TextDocumentContent,
}

/// Which schemes this server serves documents under.
///
/// One: the scheme every declaration ya-lsp wrote itself is filed under. Advertising it is the
/// whole registration: `workspace/textDocumentContent` needs no `client/registerCapability` and no
/// extension code, so a client that reads this field gets a read-only view of the RBS a macro
/// produced, just by asking.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TextDocumentContent {
    schemes: Vec<String>,
}

#[must_use]
pub fn advertised(encoding: PositionEncoding, root: &Path) -> Advertised {
    Advertised {
        standard: server_capabilities(encoding, root),
        // Nothing is taken away by this one: no editor guesses at a type hierarchy, so the command
        // just reports no results until a server answers.
        type_hierarchy_provider: true,
        workspace: AdvertisedWorkspace {
            workspace_folders: WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                // Announcing support for the notification without acting on it would be a lie the
                // client cannot detect.
                change_notifications: None,
            },
            file_operations: WorkspaceFileOperationsServerCapabilities {
                // One of the six, and the only one with an answer behind it. A move is the one file
                // operation Zeitwerk gives meaning to: a file's class is named after the file, so
                // renaming the file without renaming the class raises on the next boot. Creating
                // and deleting say nothing this server could act on.
                will_rename: Some(FileOperationRegistrationOptions {
                    filters: vec![FileOperationFilter {
                        // Ruby files only. A folder rename arrives as the folder, never as its
                        // children, so a server that took folders would be asked about a move whose
                        // contents it cannot see.
                        scheme: Some("file".to_owned()),
                        pattern: FileOperationPattern {
                            glob: "**/*.rb".to_owned(),
                            matches: Some(FileOperationPatternKind::File),
                            options: None,
                        },
                    }],
                }),
                ..WorkspaceFileOperationsServerCapabilities::default()
            },
            text_document_content: TextDocumentContent {
                // The scheme without its colon, which is how a client registers a provider for it;
                // `GENERATED_SCHEME` has the colon because the rest of the crate uses it as a
                // prefix on a whole URI.
                schemes: vec![
                    crate::analysis::synthesized::GENERATED_SCHEME
                        .trim_end_matches(':')
                        .to_owned(),
                ],
            },
        },
    }
}

#[must_use]
pub fn server_capabilities(encoding: PositionEncoding, root: &Path) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.to_lsp()),
        // Incremental. rubydex reparses the whole buffer on every `index_source`, so this saves
        // transfer, not parsing, but on a large file sending the whole text per keystroke is
        // transfer the editor pays at typing speed.
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
        // Announced only once implemented: a client told a server provides hover stops showing its
        // own word-based fallback, so advertising early makes the editor worse.
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        // Announced by the same rule: it answers. Nothing is taken away either: no client has a
        // fallback for *what overrides this*, so without it the command is just greyed out. It is
        // also the one goto an agent asks for: Claude Code's `LSP` tool maps `goToImplementation`
        // onto it and maps nothing onto `declaration` or `typeDefinition`.
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        // The second of the three gotos, advertised by the same rule and taking nothing away: no
        // client has a fallback for *what class is this value*, and the editors that send this
        // request render the answer with their own definition formatter. Claude Code maps nothing
        // onto it, so it is here for editors, not the agent.
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        // The third goto, and the only one where *not* advertising leaves the client something:
        // every editor sending this falls back to `definition` when the server declines, which is
        // exactly what ya-lsp's own `null` does where it has no signature. So it is advertised for
        // the narrow population it can answer: a project with a `sig/`, a bundle with
        // `.gem_rbs_collection`, and Ruby's own library, where `String#split` has a declaration and
        // no definition anybody can open.
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        // `workspace/symbol` answers with `SymbolInformation`, which carries a full location, so no
        // `resolveProvider` and no `workspaceSymbol/resolve`. The lazy shape exists to avoid
        // reading a file per result; here that read costs a few milliseconds for a capped result
        // set, and every client understands the eager shape.
        references_provider: Some(OneOf::Left(true)),
        // Announced for the same reason as the rest: a client told a server highlights occurrences
        // stops matching words itself, and a word match (which lights up the name in comments,
        // strings and unrelated scopes) is better than nothing. ya-lsp answers `null` wherever it
        // does not know, which puts the client's own fallback back in play at exactly those
        // positions.
        document_highlight_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        // `.` and `:` change what a completion *means* instead of just narrowing it, and a client
        // only re-asks mid-word for characters listed here. `:` covers `Foo::`: LSP trigger
        // characters are single characters, so there is no way to say `::`, and a lone `:` is
        // classified and answered with nothing. `@` and `$` are here because a sigil starts a name
        // the client's word pattern would not treat as one, so without them instance and global
        // variables would never be asked for.
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![
                ".".to_owned(),
                ":".to_owned(),
                "@".to_owned(),
                "$".to_owned(),
            ]),
            // Documentation is the expensive half of a completion item, and the user reads it for
            // one row out of hundreds, so it is filled in on demand.
            resolve_provider: Some(true),
            completion_item: Some(CompletionOptionsCompletionItem {
                label_details_support: Some(false),
            }),
            ..CompletionOptions::default()
        }),
        // `(` and `,` are where a Ruby call gains an argument (the second also covers the
        // paren-less form: `link_to "x", ` is where the next one goes). `)` only re-triggers, i.e.
        // it is asked while the popup is already up: the call it closes has no more arguments,
        // ya-lsp answers `null`, and the popup closes instead of describing a call the cursor has
        // left.
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_owned(), ",".to_owned()]),
            retrigger_characters: Some(vec![")".to_owned()]),
            ..SignatureHelpOptions::default()
        }),
        // Expand-selection has nothing to take away (in a Ruby file the command otherwise does
        // nothing), so this is pure addition.
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        // `prepareProvider` is the half that matters: it lets ya-lsp say "not here" *before* the
        // editor asks for a new name, the only point where declining costs the user nothing. A
        // client without it sends `textDocument/rename` directly, so every refusal must be
        // reachable from both.
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        // The kinds are listed instead of `Simple(true)` because a client filters on them *before*
        // asking: VS Code's Refactor… menu sends `only: ["refactor"]`, and a server advertising no
        // kinds is asked nothing. Listing exactly the two implemented kinds follows this module's
        // opening rule: an advertised `quickfix` would put an empty entry under the lightbulb on
        // every diagnostic.
        //
        // `CodeActionKind::EMPTY` is the third entry, and it matters: a client reads this list to
        // decide whether to *ask* at all when it filters a menu, and the one action here that is
        // neither extraction nor rewrite (the read-only jump into the RBS a macro generated) has no
        // honest kind in the protocol's hierarchy. Empty is the protocol's kindless action, so
        // advertising it keeps the lightbulb asking. In return, `requests::code_actions` honours
        // `only`: a client asking for `quickfix` alone gets an empty list, not four refactorings it
        // will discard.
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![
                CodeActionKind::REFACTOR_EXTRACT,
                CodeActionKind::REFACTOR_REWRITE,
                CodeActionKind::EMPTY,
            ]),
            resolve_provider: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        // One command: the door to the one document nothing else in the protocol can open. A
        // generated declaration lives under a scheme with no file behind it, so no `Location` may
        // name it and no link may point at it. The action carries the URI as a **command argument**
        // (never a `Location`, symbol row or diagnostic), and the server answers by asking the
        // client to show that document. `DocUri::from_graph_uri` still refuses the scheme, so the
        // backstop `synthesized.md` names is untouched.
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![crate::analysis::show_generated_command(root)],
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        // The one capability here that *removes* a fallback instead of filling an absence: a client
        // with a folding provider stops guessing from indentation, and on well-formatted Ruby that
        // guess is decent. So `analysis::ranges` covers the shapes the guess gets right as well as
        // those it cannot see, and answers `null` (never an empty array) where it found nothing,
        // which hands the guess back.
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        // `resolveProvider: false`, the only interesting field here: a link's target is known when
        // the link is made (the graph has the required file or it does not), so a second round trip
        // adds nothing. Advertising resolve would cost the client a request per link for an answer
        // it already has.
        //
        // Nothing is taken away by this one either: an editor underlines a `require` path in Ruby
        // only if a grammar guessed at it, and none does.
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        // A bare `true`, not the options struct, because its only field is `workDoneProgress`, and
        // this server reports progress for indexing, not per request. The type hierarchy is
        // announced further out, in `Advertised`, which exists because `lsp-types` 0.97 can spell
        // this capability but not that one.
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        // `resolveProvider: false`, as for document links: a hint is its label, and there is no
        // tooltip to fetch. A tooltip said how the type was found, which no surface says any more
        // (decided 2026-09-29), and a guess is never drawn.
        //
        // Nothing is taken away by this one: no editor draws Ruby types in the margin otherwise.
        inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
            InlayHintOptions {
                resolve_provider: Some(false),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        ))),
        // The legend is the wire contract twice over: a client reads every token's type as an index
        // into this list, so it must be exactly `tokens::LEGEND`, in exactly its order. A mismatch
        // recolours every token in every file, consistently, the hardest kind of wrong to see.
        // `the_v0_4_0_semantic_token_legend_is_the_one_the_tokens_are_numbered_against` holds the
        // two together.
        //
        // `full: Bool(true)` and no `delta`, on purpose: a delta is a wire optimisation over an
        // answer the server computes anyway, paid for with a cache of every response per document
        // and an id to invalidate on every edit. See `tokens`.
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
        // **Deliberately `None`; everything is in [`Advertised`].** The two halves `lsp-types` can
        // spell are written there beside the one it cannot, because a second flattened `workspace`
        // key would collide with this one.
        workspace: None,
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

/// The schema dumps `index.include` can never name, and the only non-Ruby file this server reads.
///
/// `db/*structure.sql`, not `**/*.sql`, which would sweep up every fixture, seed and migration for
/// one file. The shape mirrors Rails' `schema_dump`, which names the primary database's dump
/// `structure.sql` and every other `<database>_structure.sql`, the same names `rails::is_structure`
/// matches. That predicate, not this glob, has the last word, as `Workspace::indexes` does for the
/// Ruby patterns.
const SCHEMA_DUMP_GLOB: &str = "db/*structure.sql";

/// The application's database configuration, which says which adapter its connection is. YAML, so
/// never indexed, and read by the generator pass like a schema dump.
const DATABASE_CONFIG_GLOB: &str = "config/database.yml";

/// The project's locale files, YAML, read beside the pass like `database.yml`. The
/// `.rb` ones are Ruby, which `index.include` already watches.
const LOCALES_GLOB: &str = "**/config/locales/**/*.yml";

/// The id the watcher registration is made under.
///
/// Fixed, not generated: the protocol identifies a registration by this string, so anything that
/// later unregisters it must name the same one.
const WATCHED_FILES_ID: &str = "ya-lsp-watched-files";

/// Ask the client to watch the project's `ya-lsp.toml`, the files it indexes and its lockfile.
///
/// The protocol has no static form for file watching (`initialize` cannot announce it), so
/// `client/registerCapability` is the only way, and `None` here means the client did not say it
/// accepts one. Without it, reload works only in an editor whose extension brings its own watcher.
///
/// The Ruby patterns are `index.include` itself, so a project that widened it to cover `sig/` gets
/// its signatures watched too. `index.exclude` has no counterpart (LSP watchers cannot say "not
/// this"), so the registration is deliberately the *wider* of the two, and `Workspace::indexes`
/// narrows it on arrival. Watching too much costs dropped notifications; watching too little leaves
/// a file that never refreshes.
///
/// **Four patterns are constants, and none is indexed**, by design. `ya-lsp.toml` is watched and
/// never indexed, and [`SCHEMA_DUMP_GLOB`], [`DATABASE_CONFIG_GLOB`] and [`LOCALES_GLOB`] sit
/// beside it for the same reason: a `db/structure.sql`, a `config/database.yml` and a locale file
/// are read beside the graph, are not Ruby, and must never reach rubydex. Being constants makes
/// them safe: this registration is made once, at `initialize`, so anything derived from reloadable
/// configuration would be stale for the life of the process.
///
/// **The lockfiles come last, one watcher each** (`Workspace::lockfiles`): every lockfile Bundler
/// could use, existing or not, since a fresh clone has none until `bundle install` writes it. They
/// are as safe as the constants: the list follows `BUNDLE_GEMFILE`, which is read once, never the
/// configuration. Each is watched from its own directory, so a `BUNDLE_GEMFILE` outside the project
/// is watched too.
#[must_use]
pub fn watched_files(
    root: &Path,
    index: &IndexConfig,
    lockfiles: &[PathBuf],
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
    let project = [
        CONFIG_FILE_NAME,
        SCHEMA_DUMP_GLOB,
        DATABASE_CONFIG_GLOB,
        LOCALES_GLOB,
    ]
    .into_iter()
    .chain(index.include.iter().map(String::as_str))
    .map(|pattern| watch_glob(root, pattern, relative));
    let lockfiles = lockfiles.iter().map(|lockfile| {
        // Both fall back only for a path no lockfile list holds: `gems::lockfiles` joins a file
        // name onto a directory.
        let directory = lockfile.parent().unwrap_or(root);
        let name = lockfile.file_name().unwrap_or_default().to_string_lossy();
        watch_glob(directory, &name, relative)
    });
    let options = DidChangeWatchedFilesRegistrationOptions {
        watchers: project
            .chain(lockfiles)
            .map(|glob_pattern| FileSystemWatcher {
                glob_pattern,
                // All three kinds, which is what omitting `kind` means. A deleted `ya-lsp.toml` is
                // a configuration change (back to defaults), and so is one written for the first
                // time; a deleted `.rb` is the one case the index cannot learn any other way, since
                // nothing else says a declaration has gone.
                kind: None,
            })
            .collect(),
    };
    Some(Registration {
        id: WATCHED_FILES_ID.to_owned(),
        method: "workspace/didChangeWatchedFiles".to_owned(),
        // Infallible for this shape (every field is a string), but the type is a `Result`, and a
        // registration whose options went missing would ask the client to watch nothing. Better no
        // registration than one that silently watches nothing.
        register_options: Some(serde_json::to_value(options).ok()?),
    })
}

/// One pattern the watcher is registered with, relative to `root`.
///
/// Relative when the client supports it (LSP 3.17). Its base is a URI, not glob syntax, so a root
/// holding `[`, `{`, `*` or `?` (a directory named `[wip]` is enough) cannot be misread as a
/// pattern. The absolute form has no defence, since LSP's glob syntax defines no escape. Its
/// separators are `/` on every platform, Windows included, which is why the path is not passed
/// through as the OS spells it.
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
/// The client's own selector names the same two (`package.json` contributes `erb` with the id and
/// extensions ruby-lsp uses), and `vscode_manifest.rs` keeps the two lists in step. A registration
/// naming only `ruby` would leave a Rails engine's templates unclaimed: the very silence this
/// mechanism exists to end.
pub const LANGUAGE_IDS: [&str; 2] = ["ruby", "erb"];

/// The prefix every document registration's id is made under.
///
/// Fixed and recognisable, for [`WATCHED_FILES_ID`]'s reason and one more: an extension driving
/// several servers must tell *these* registrations from the watcher's, because it handles them
/// oppositely. The watcher's is forwarded untouched; a document registration may need narrowing,
/// since two folders on one Ruby ask for the same gem roots, and two servers answering one hover is
/// exactly what the narrow selector was protecting against.
pub const DOCUMENTS_ID_PREFIX: &str = "ya-lsp-documents/";

/// A document capability that can be requested again, over more files than the client's own
/// selector named.
///
/// Three names, never the same word twice: `advertised` is the *server* capability's key in
/// [`advertised`], `client` is the *client* capability's key under `textDocument`, and `method` is
/// what a registration is made with. For the two hierarchies that is the `prepare` half, not any of
/// the four follow-up requests, and for semantic tokens it is `textDocument/semanticTokens`, not
/// the `/full` the server answers.
struct Dynamic {
    advertised: &'static str,
    client: &'static str,
    method: &'static str,
}

/// Every request method this server answers that a document selector gates.
///
/// **Generated from, and checked against, [`advertised`]**:
/// `every_advertised_document_capability_can_be_registered_again` is the test, and the reason this
/// is a table instead of a hand-written list: a capability advertised but missing here would be
/// claimed for `didOpen` and silent for that one request, which looks like it works and is worse
/// than a file that is silent for everything.
///
/// `completionItem/resolve` and the four hierarchy walks are deliberately absent: the protocol
/// registers each through its parent's options, so they arrive with `resolveProvider` and the
/// `prepare` entry, not under their own methods.
const DYNAMIC: [Dynamic; 19] = [
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
        advertised: "implementationProvider",
        client: "implementation",
        method: "textDocument/implementation",
    },
    Dynamic {
        advertised: "typeDefinitionProvider",
        client: "typeDefinition",
        method: "textDocument/typeDefinition",
    },
    Dynamic {
        advertised: "declarationProvider",
        client: "declaration",
        method: "textDocument/declaration",
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

/// The advertised capabilities no document registration carries, each with its reason for being
/// here and not in [`DYNAMIC`].
///
/// A list, not an omission, because the test walking [`advertised`] must fail on any capability
/// nobody has ruled on, and "this one is not a document request" is a ruling. `cfg(test)` because
/// only that test reads it.
#[cfg(test)]
const NOT_A_DOCUMENT: [&str; 5] = [
    // Negotiated once, in the handshake. A registration cannot change the coordinates answers
    // already went out in.
    "positionEncoding",
    // Four notifications, not one capability, built by `synchronization` below.
    "textDocumentSync",
    // A search of the whole graph. The request names no document, so no selector gates it, and it
    // already answers inside gems.
    "workspaceSymbolProvider",
    // Workspace folders, file operations, and the scheme generated documents are served under. None
    // is about a document the client owns.
    "workspace",
    // A command the client runs by name; the request that runs it names no document.
    "executeCommandProvider",
];

/// One registration the client will accept, waiting for the files it should cover.
///
/// The selector is deliberately *not* here. What the server has answers about is the gem roots,
/// Ruby's own library and the RBS beside them, none of which is known until the bundle is
/// discovered, on the analysis thread, long after the handshake. A `Registration` with no selector
/// would fall back to the client's own (the narrow folder), so the half-built value is a separate
/// type that cannot be sent by mistake.
#[derive(Debug, Clone)]
pub struct Requested {
    method: &'static str,
    options: serde_json::Map<String, serde_json::Value>,
}

/// What this client will accept a second registration of, derived from what the server advertised.
///
/// Empty when the client cannot dynamically register text synchronisation, deliberately
/// all-or-nothing: a document the client never sends `didOpen` for cannot be asked about, so
/// registering the requests over it would claim files and answer nothing.
///
/// Every entry is filtered by the client's *own* `dynamicRegistration` flag for that capability.
/// Read from the serialized `textDocument` object instead of many typed accessor chains: the flag
/// is spelled the same in each, and the table above is already where the two vocabularies are
/// matched.
#[must_use]
pub fn dynamic_documents(
    encoding: PositionEncoding,
    capabilities: &ClientCapabilities,
    root: &Path,
) -> Vec<Requested> {
    // `Null` for a client that said nothing about documents, which answers `Null` to every lookup
    // below and so declines everything: one path, not two.
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

    // The whole advertised object, not [`server_capabilities`], because one document capability
    // (`typeHierarchyProvider`) is a field `lsp-types` cannot spell and exists only in
    // [`Advertised`]. `root` rides along only for that: nothing below reads the `workspace` half
    // this builds, and a command is not a document.
    let advertised =
        serde_json::to_value(advertised(encoding, root)).unwrap_or(serde_json::Value::Null);
    let mut requested: Vec<Requested> = DYNAMIC
        .iter()
        .filter(|entry| dynamic(entry.client))
        .map(|entry| Requested {
            method: entry.method,
            // The advertised value *is* the registration's options, minus the selector: a
            // `CompletionOptions` and a `CompletionRegistrationOptions` differ by exactly that
            // field. A capability advertised as a bare `true` carries nothing and registers with
            // nothing; `every_advertised_document_capability_can_be_registered_again` rules out the
            // third case, a method here advertised nowhere.
            options: match &advertised[entry.advertised] {
                serde_json::Value::Object(options) => options.clone(),
                _ => serde_json::Map::new(),
            },
        })
        .collect();
    // Synchronisation last, and `didOpen` last within it: registering `didOpen` makes the client
    // walk the already-open documents and send one for every file the new selector newly matches,
    // so every provider above must be in place before the client is told the file exists.
    requested.extend(synchronization(&advertised["textDocumentSync"]));
    requested
}

/// The four notifications `textDocumentSync` is registered as, in the order they must arrive.
///
/// `syncKind` and `includeText` are copied from the advertised options, not restated: a
/// registration without `syncKind` would leave the client on its own default, and
/// `position::Rebase` would get whole-document changes it is not written for.
fn synchronization(sync: &serde_json::Value) -> Vec<Requested> {
    // Reads, not questions: `textDocumentSync` is a constant in `server_capabilities`, so both
    // fields always exist. `a_registration_carries_the_options_the_handshake_announced` asserts the
    // values, so a change to the constant's shape fails there instead of shipping a registration
    // with a null in it.
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
/// `prefixes` are directory URIs (the gem roots, Ruby's own library, the RBS root), and the
/// workspace's own is **not** among them: the client already claimed it with its original selector,
/// and a second provider over the same file means one server answering a hover twice. Empty
/// `prefixes` means nothing to widen to, so nothing is sent.
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
                    // `vscode-languageclient` recognises only this shape, and it does not ignore
                    // anything else but drops the pattern, which widens the filter to scheme and
                    // language alone, claiming every Ruby file the editor has open anywhere.
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
/// **Built from [`DYNAMIC`], not written out.** VS Code's client accepts all of these, so this is
/// the realistic shape, and deriving it from the table means a new row there is covered by every
/// harness test, without anyone remembering to widen this.
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
            // not either: a client that takes semantic tokens must say which.
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
        // `ServerCapabilities` has no `typeHierarchyProvider` field in any published version of the
        // crate, so this is the one capability added on the way to JSON. The second half of the
        // assertion is the point: a flatten that stopped flattening would announce *only* the type
        // hierarchy, and every other feature would silently stop being offered.
        let advertised =
            serde_json::to_value(advertised(PositionEncoding::Utf8, Path::new("/project")))
                .expect("plain data");
        assert_eq!(advertised["typeHierarchyProvider"], serde_json::json!(true));
        assert_eq!(advertised["hoverProvider"], serde_json::json!(true));
        assert!(advertised["completionProvider"].is_object());
        assert!(advertised["textDocumentSync"].is_object());
    }

    #[test]
    fn the_v0_3_0_rename_provider_asks_to_be_consulted_before_the_editor_prompts() {
        // A bare `renameProvider: true` would still work, but would move every refusal to *after*
        // the user has typed a new name. `prepareProvider` buys the earlier question, and it is
        // this provider's only option.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
        let OneOf::Right(rename) = capabilities.rename_provider.expect("a rename provider") else {
            panic!("announced without options, so nothing asks before the prompt");
        };
        assert_eq!(rename.prepare_provider, Some(true));
    }

    #[test]
    fn the_m5_provider_is_announced_with_its_triggers() {
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
        let completion = capabilities
            .completion_provider
            .expect("a completion provider");
        assert_eq!(completion.resolve_provider, Some(true));
        // Without `.` here, a client narrows the list it already had instead of asking again, and
        // `foo.` would complete against whatever `foo` matched.
        let triggers = completion.trigger_characters.expect("trigger characters");
        assert!(triggers.contains(&".".to_owned()));
        assert!(triggers.contains(&":".to_owned()));
    }

    #[test]
    fn the_v0_3_0_highlight_provider_is_announced() {
        // Announced without options: `documentHighlight` has none, and the whole decision is that a
        // client stops matching words itself as soon as it is told.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
        assert!(capabilities.document_highlight_provider.is_some());
    }

    #[test]
    fn the_v0_3_0_provider_is_announced_with_its_triggers() {
        // `(` and `,` are where an argument list gains an argument; `)` only re-triggers, so the
        // popup is asked once more as the call closes and takes ya-lsp's `null` as its cue to
        // close. A character on neither list asks nothing.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
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
        // Neither takes options, and they are announced for opposite reasons: expand-selection has
        // nothing to displace in a Ruby file, while folding takes the editor's indentation guess
        // out of play as soon as it is told, which is why `analysis::ranges` covers what that guess
        // got right as well as what it could not see.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
        assert!(capabilities.selection_range_provider.is_some());
        assert!(capabilities.folding_range_provider.is_some());
    }

    #[test]
    fn the_call_hierarchy_is_announced_and_the_type_hierarchy_beside_it() {
        // The two hierarchies are announced by different mechanisms (one is an `lsp-types` field,
        // the other flattened in beside it), and a client that gets one but not the other shows a
        // "show call hierarchy" command with no results. So both are asserted here, in one test,
        // against the struct that goes on the wire.
        let advertised =
            serde_json::to_value(advertised(PositionEncoding::Utf16, Path::new("/project")))
                .expect("the advertised capabilities serialize");
        assert_eq!(advertised["callHierarchyProvider"], true);
        assert_eq!(advertised["typeHierarchyProvider"], true);
    }

    #[test]
    fn the_document_link_provider_says_it_resolves_nothing() {
        // `Some(false)`, not `None`. The protocol treats absent and `false` the same, but a server
        // that *decided* not to resolve and one that forgot to say look identical on the wire. The
        // target is known when the link is made, so a round trip would buy nothing.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
        let links = capabilities
            .document_link_provider
            .expect("a document link provider");
        assert_eq!(links.resolve_provider, Some(false));
    }

    #[test]
    fn the_inlay_hint_provider_resolves_nothing() {
        // A hint is its label: no tooltip says how the type was found, so there is nothing to
        // fetch, and `Some(false)` says so outright, as the document link's does.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
        let Some(OneOf::Right(InlayHintServerCapabilities::Options(hints))) =
            capabilities.inlay_hint_provider
        else {
            panic!("an inlay hint provider with options");
        };
        assert_eq!(hints.resolve_provider, Some(false));
    }

    #[test]
    fn the_v0_4_0_semantic_token_legend_is_the_one_the_tokens_are_numbered_against() {
        // The legend *is* the wire contract: a client reads every token's type as an index into
        // this list. A mismatch with `tokens::Kind` recolours every token in every file,
        // consistently and plausibly, the hardest kind of wrong to notice, so the two are asserted
        // against each other, not each against a hand-written list.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
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
        // server computes anyway; announcing it would oblige ya-lsp to keep every response sent,
        // per document, keyed by an id invalidated on every edit.
        assert!(matches!(
            options.full,
            Some(lsp_types::SemanticTokensFullOptions::Bool(true))
        ));
        assert!(options.range.is_none());
    }

    #[test]
    fn the_m4_providers_are_announced() {
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
        assert!(capabilities.references_provider.is_some());
        assert!(capabilities.workspace_symbol_provider.is_some());
    }

    #[test]
    fn text_sync_is_incremental_and_the_m2_providers_are_announced() {
        // These four are a contract with the client, fixed at `initialize` and never renegotiated.
        // Getting `change` wrong is the costly one: the client would send ranges while the server
        // expected whole buffers, and every edit would silently corrupt the document.
        let capabilities = server_capabilities(PositionEncoding::Utf8, Path::new("/project"));
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
        // Without this, the server never asks anyone to watch anything: `ya-lsp.toml` reloads only
        // in the one editor that brings its own watcher, and no editor notices a `git checkout`.
        let root = Path::new("/tmp/ya-lsp-watch/project");
        let index = IndexConfig {
            include: vec!["**/*.rb".to_owned(), "sig/**/*.rbs".to_owned()],
            ..IndexConfig::default()
        };
        let registration = watched_files(root, &index, &[], &watching(Some(true), None))
            .expect("a watcher is registered");
        assert_eq!(registration.method, "workspace/didChangeWatchedFiles");
        assert_eq!(registration.id, WATCHED_FILES_ID);
        // The schema dump is a constant, not derived from this configuration, which makes it
        // correct: this runs once, at `initialize`, and a pattern computed from a reloadable
        // `ya-lsp.toml` would be stale for the life of the process. `index.include` cannot spell it
        // anyway; that is for Ruby's shapes.
        assert!(
            !index
                .include
                .iter()
                .any(|pattern| pattern == SCHEMA_DUMP_GLOB)
        );

        let watchers = watchers(&registration);
        // `kind` unset means create, change and delete. A `ya-lsp.toml` deleted, or written for the
        // first time, changes the configuration as much as an edit, and a deleted `.rb` is the only
        // way the index hears that a declaration has gone.
        assert!(watchers.iter().all(|watcher| watcher.kind.is_none()));
        assert_eq!(
            watchers
                .iter()
                .map(|watcher| watcher.glob_pattern.clone())
                .collect::<Vec<_>>(),
            vec![
                // A client without relative patterns gets absolute ones, with `/` separators.
                GlobPattern::String("/tmp/ya-lsp-watch/project/ya-lsp.toml".to_owned()),
                // The constants come first, and none is from `index.include`: each names a file
                // this server reads and never indexes.
                GlobPattern::String("/tmp/ya-lsp-watch/project/db/*structure.sql".to_owned()),
                GlobPattern::String("/tmp/ya-lsp-watch/project/config/database.yml".to_owned()),
                GlobPattern::String(
                    "/tmp/ya-lsp-watch/project/**/config/locales/**/*.yml".to_owned(),
                ),
                GlobPattern::String("/tmp/ya-lsp-watch/project/**/*.rb".to_owned()),
                GlobPattern::String("/tmp/ya-lsp-watch/project/sig/**/*.rbs".to_owned()),
            ],
            "the config plus index.include verbatim: a project that widened it to cover sig/ \
             has its signatures watched too"
        );
    }

    #[test]
    fn a_client_that_takes_relative_patterns_gets_one() {
        // The absolute form is glob syntax throughout, so a root under a directory named `[wip]`
        // would be read as a character class and match nothing. The relative form's base is a URI,
        // so the only glob in it is the part we wrote.
        let root = Path::new("/tmp/ya-lsp-watch/[wip]/project");
        let registration = watched_files(
            root,
            &IndexConfig::default(),
            &[],
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
        // Derived from the default, not spelled out: this test is about the *form* of each pattern,
        // and `the_watchers_cover_the_config_and_everything_the_index_includes` above already pins
        // that the list is `index.include` verbatim.
        let expected: Vec<GlobPattern> = [
            CONFIG_FILE_NAME.to_owned(),
            SCHEMA_DUMP_GLOB.to_owned(),
            DATABASE_CONFIG_GLOB.to_owned(),
            LOCALES_GLOB.to_owned(),
        ]
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
        // `workspace_root` is `.` when the client sent no folder and the process has no working
        // directory. There is no URI for that, so no relative pattern either, and answering `None`
        // here would drop the watcher over a case the absolute form handles.
        let registration = watched_files(
            Path::new("."),
            &IndexConfig::default(),
            &[],
            &watching(Some(true), Some(true)),
        )
        .expect("a watcher is registered");
        assert_eq!(
            watchers(&registration)[0].glob_pattern,
            GlobPattern::String("./ya-lsp.toml".to_owned())
        );
    }

    #[test]
    fn every_lockfile_is_watched_from_its_own_directory() {
        // Without these, a `bundle install` adds gems nothing ever indexes: gem discovery runs once
        // per session. Every lockfile Bundler could use, after everything else, and each from its
        // own directory, because `BUNDLE_GEMFILE` may name one outside the project.
        let root = Path::new("/tmp/ya-lsp-watch/project");
        let elsewhere = Path::new("/tmp/ya-lsp-watch/shared");
        let lockfiles = [
            elsewhere.join("Gemfile.ci.lock"),
            root.join("Gemfile.lock"),
            root.join("gems.locked"),
        ];
        let tail = |capabilities: &ClientCapabilities| -> Vec<GlobPattern> {
            let registration =
                watched_files(root, &IndexConfig::default(), &lockfiles, capabilities)
                    .expect("a watcher is registered");
            let watchers = watchers(&registration);
            watchers[watchers.len() - lockfiles.len()..]
                .iter()
                .map(|watcher| watcher.glob_pattern.clone())
                .collect()
        };

        assert_eq!(
            tail(&watching(Some(true), None)),
            vec![
                GlobPattern::String("/tmp/ya-lsp-watch/shared/Gemfile.ci.lock".to_owned()),
                GlobPattern::String("/tmp/ya-lsp-watch/project/Gemfile.lock".to_owned()),
                GlobPattern::String("/tmp/ya-lsp-watch/project/gems.locked".to_owned()),
            ]
        );
        let relative = |base: &Path, pattern: &str| {
            GlobPattern::Relative(RelativePattern {
                base_uri: OneOf::Right(
                    DocUri::from_path(base)
                        .expect("an absolute directory")
                        .to_lsp()
                        .expect("a uri"),
                ),
                pattern: pattern.to_owned(),
            })
        };
        assert_eq!(
            tail(&watching(Some(true), Some(true))),
            vec![
                relative(elsewhere, "Gemfile.ci.lock"),
                relative(root, "Gemfile.lock"),
                relative(root, "gems.locked"),
            ]
        );
    }

    #[test]
    fn a_client_that_cannot_be_asked_is_not_asked() {
        // Nothing to fall back on: the protocol has no static form for file watching, so a client
        // that takes no dynamic registration cannot get a watcher at all. The server says so in the
        // log, instead of leaving the user to discover it by editing `ya-lsp.toml` or switching
        // branches and seeing nothing happen.
        let root = Path::new("/tmp/ya-lsp-watch/project");
        let index = IndexConfig::default();
        assert!(watched_files(root, &index, &[], &ClientCapabilities::default()).is_none());
        assert!(
            watched_files(
                root,
                &index,
                &[],
                &ClientCapabilities {
                    workspace: Some(lsp_types::WorkspaceClientCapabilities::default()),
                    ..ClientCapabilities::default()
                }
            )
            .is_none(),
            "a workspace section that says nothing about watched files is still a no"
        );
        assert!(watched_files(root, &index, &[], &watching(Some(false), None)).is_none());
        assert!(
            watched_files(root, &index, &[], &watching(None, Some(true))).is_none(),
            "relative patterns without dynamic registration are not an offer to watch"
        );
    }

    #[test]
    fn the_sync_kind_is_read_out_of_either_shape_the_protocol_allows() {
        // `sync_kind` is how the test above checks the contract, so it must read both spellings, or
        // the assertion could pass on a `None` it produced itself. `textDocumentSync` is either a
        // bare kind or an options object, and switching this server to the options form must not
        // quietly make that test vacuous.
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

    /// The table that generates the registrations must cover everything the handshake advertised.
    ///
    /// The test the whole mechanism turns on. Text synchronisation is registrable on its own, so a
    /// capability missing from `DYNAMIC` gets a gem file **claimed for `didOpen` and silent for
    /// that one request**, which is worse than silent for everything, because it looks like it
    /// works. `NOT_A_DOCUMENT` is the other half: a capability nobody has ruled on fails here
    /// instead of being quietly left out.
    #[test]
    fn every_advertised_document_capability_can_be_registered_again() {
        let advertised =
            serde_json::to_value(advertised(PositionEncoding::Utf8, Path::new("/project")))
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
            Path::new("/project"),
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
    /// shape, and a dropped pattern does not narrow but *widens*, to language and scheme alone.
    /// Getting this wrong claims every Ruby file the editor has open anywhere: the failure the
    /// narrow per-folder selector exists to prevent, caused by the server instead.
    #[test]
    fn the_pattern_is_a_uri_string_the_client_will_recognise() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
            Path::new("/project"),
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
            Path::new("/project"),
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

        // Without these, the popup never re-opens on a typed `.` inside a gem, the one request
        // whose trigger is the whole feature.
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
        // A registration without `syncKind` leaves the client on its own default, and
        // `position::Rebase` would get whole-document changes it is not written for.
        assert_eq!(
            options("textDocument/didChange")["syncKind"],
            serde_json::json!(TextDocumentSyncKind::INCREMENTAL)
        );
        assert_eq!(options("textDocument/didSave")["includeText"], false);
        assert_eq!(options("textDocument/rename")["prepareProvider"], true);
        assert_eq!(options("textDocument/inlayHint")["resolveProvider"], false);
    }

    /// `didOpen` is registered last, because registering it is what sends the notifications.
    ///
    /// The client's `didOpen` feature **back-fills**: it walks the documents already open and sends
    /// one for every file the new selector newly matches, so a gem file the user is looking at
    /// starts answering at registration, not at the next tab switch. Every provider must be in
    /// place before that.
    #[test]
    fn synchronisation_is_registered_after_the_requests_and_did_open_last_of_all() {
        let requested = dynamic_documents(
            PositionEncoding::Utf16,
            &every_dynamic_registration_accepted(),
            Path::new("/project"),
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
            Path::new("/project"),
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
    /// All-or-nothing on purpose: a file the client never reports open cannot be asked about, so
    /// registering the requests over it would claim files and answer nothing. Such a client keeps
    /// the unregistered behaviour, plus one log line saying what it lacks.
    #[test]
    fn a_client_that_will_not_sync_dynamically_is_asked_for_nothing() {
        let capabilities: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "textDocument": {
                "synchronization": { "dynamicRegistration": false },
                "hover": { "dynamicRegistration": true },
            }
        }))
        .expect("client capabilities");

        assert!(
            dynamic_documents(
                PositionEncoding::Utf16,
                &capabilities,
                Path::new("/project")
            )
            .is_empty()
        );
        assert!(
            dynamic_documents(
                PositionEncoding::Utf16,
                &ClientCapabilities::default(),
                Path::new("/project")
            )
            .is_empty(),
            "a client that says nothing about textDocument at all"
        );
    }

    /// One declined capability leaves the rest of the batch standing.
    ///
    /// Not just protocol politeness: `doRegisterCapability` rejects the *whole* array at the first
    /// method it has no feature for, so a registration the client never agreed to would take down
    /// every method after it.
    #[test]
    fn a_capability_the_client_declines_is_left_out_and_the_rest_stand() {
        let mut accepted = serde_json::to_value(every_dynamic_registration_accepted())
            .expect("client capabilities serialize");
        accepted["textDocument"]["semanticTokens"]["dynamicRegistration"] =
            serde_json::json!(false);
        let capabilities: ClientCapabilities =
            serde_json::from_value(accepted).expect("client capabilities");

        let methods: Vec<&str> = dynamic_documents(
            PositionEncoding::Utf16,
            &capabilities,
            Path::new("/project"),
        )
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
            Path::new("/project"),
        );
        assert!(
            document_registrations(&requested, &[]).is_empty(),
            "a project with no bundle has nothing to widen to"
        );
    }
}
