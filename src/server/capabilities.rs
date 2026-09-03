//! What the server tells the client it can do.
//!
//! Capabilities are announced per milestone: advertising a feature we do not implement makes
//! the editor show an empty result instead of falling back to its own heuristics, which is a
//! worse experience than not advertising at all.

use std::path::Path;

use lsp_types::{
    ClientCapabilities, CompletionOptions, CompletionOptionsCompletionItem,
    DidChangeWatchedFilesRegistrationOptions, FileSystemWatcher, FoldingRangeProviderCapability,
    GlobPattern, HoverProviderCapability, OneOf, Registration, RelativePattern, RenameOptions,
    SaveOptions, SelectionRangeProviderCapability, ServerCapabilities, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, WorkDoneProgressOptions,
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
        // v0.3.0. Nothing is taken away by this one: no editor guesses at a type hierarchy, so
        // the command simply reports that there are no results until a server answers it.
        type_hierarchy_provider: true,
    }
}

#[must_use]
pub fn server_capabilities(encoding: PositionEncoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.to_lsp()),
        // Incremental since M2. rubydex reparses the whole buffer on every `index_source`, so
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
        // M2. Announced only now that they are implemented: a client that is told a server
        // provides hover will stop showing its own word-based fallback, so advertising early
        // makes the editor worse, not better.
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        // M4. `workspace/symbol` answers with `SymbolInformation`, which carries a full
        // location — so no `resolveProvider`, and no `workspaceSymbol/resolve`. The lazy shape
        // exists to avoid reading a file per result; measured here that read is a few
        // milliseconds for a capped result set, and every client understands the eager one.
        references_provider: Some(OneOf::Left(true)),
        // v0.3.0. Announced with the rest of them and for the same reason: a client told a
        // server highlights occurrences stops matching words itself, and a word match — which
        // lights up the name inside a comment, inside a string, and in an unrelated scope — is
        // better than nothing at all. ya-lsp answers `null` wherever it does not know, which is
        // what puts the client's own fallback back in play for exactly those positions.
        document_highlight_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        // M5. `.` and `:` are the two characters that change what a completion *means* rather
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
        // v0.3.0. `(` and `,` are where a Ruby call gains an argument — the second covers the
        // paren-less form too, since `link_to "x", ` is where the next one goes. `)` only
        // re-triggers, which is to say it is asked while the popup is already up: the call it
        // closes has no further arguments, ya-lsp answers `null`, and the popup goes away
        // rather than standing there describing a call the cursor has left.
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_owned(), ",".to_owned()]),
            retrigger_characters: Some(vec![")".to_owned()]),
            ..SignatureHelpOptions::default()
        }),
        // v0.3.0. Expand-selection has nothing to take away — in a Ruby file the command does
        // nothing at all today — so this one is pure addition.
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        // v0.3.0. `prepareProvider` is the half of this that matters: it is what lets ya-lsp
        // answer "not here" *before* the editor asks the user for a new name, which is the only
        // point at which declining costs the user nothing. A client that does not support it
        // sends `textDocument/rename` straight off, so every refusal is reachable from both.
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        // v0.3.0, and the one capability here that *removes* a fallback rather than replacing
        // an absence: a client with a folding provider stops guessing from indentation, and on
        // well-formatted Ruby that guess is decent. So `analysis::ranges` covers the shapes the
        // guess gets right as well as the ones it cannot see, and answers `null` — never an
        // empty array — where it found nothing, which is what hands the guess back.
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                // Multi-root arrives with M6; announcing support for the notification without
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

/// The id the watcher registration is made under.
///
/// Fixed rather than generated: the protocol identifies a registration by this string, so
/// anything that later unregisters it has to be able to name the same one.
const WATCHED_FILES_ID: &str = "ya-lsp-watched-files";

/// Ask the client to watch the project's `ya-lsp.toml` and the files it indexes.
///
/// The protocol has no static form for file watching — `initialize` cannot announce it, which is
/// why this is not in `server_capabilities` — so `client/registerCapability` is the only way to
/// ask, and `None` here means the client did not say it accepts one. Until v0.2.0 nothing sent
/// this at all: the VS Code extension supplied a watcher of its own through
/// `synchronize.fileEvents`, so reload worked there and in no other editor.
///
/// The Ruby patterns are `index.include` itself, so a project that widened it to cover `sig/`
/// gets its signatures watched too. `index.exclude` has no counterpart here — LSP watchers
/// cannot say "not this" — so the registration is deliberately the *wider* of the two, and
/// `Workspace::indexes` narrows it back down on arrival. Watching too much costs notifications
/// the server drops; watching too little is a file that never refreshes.
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
        watchers: std::iter::once(CONFIG_FILE_NAME)
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
        assert_eq!(
            watchers(&registration)
                .iter()
                .map(|watcher| watcher.glob_pattern.clone())
                .collect::<Vec<_>>(),
            vec![relative(CONFIG_FILE_NAME), relative("**/*.rb")]
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
}
