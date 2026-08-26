//! What the server tells the client it can do.
//!
//! Capabilities are announced per milestone: advertising a feature we do not implement makes
//! the editor show an empty result instead of falling back to its own heuristics, which is a
//! worse experience than not advertising at all.

use std::path::Path;

use lsp_types::{
    ClientCapabilities, CompletionOptions, CompletionOptionsCompletionItem,
    DidChangeWatchedFilesRegistrationOptions, FileSystemWatcher, GlobPattern,
    HoverProviderCapability, OneOf, Registration, RelativePattern, SaveOptions, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, WorkspaceFileOperationsServerCapabilities,
    WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};

use crate::{
    analysis::position::PositionEncoding,
    workspace::{DocUri, config::CONFIG_FILE_NAME},
};

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

/// The id the `ya-lsp.toml` watcher is registered under.
///
/// Fixed rather than generated: the protocol identifies a registration by this string, so
/// anything that later unregisters it has to be able to name the same one.
const CONFIG_WATCHER_ID: &str = "ya-lsp-config-watcher";

/// Ask the client to watch the project's `ya-lsp.toml`, so a change to it reloads.
///
/// The protocol has no static form for file watching — `initialize` cannot announce it, which is
/// why this is not in `server_capabilities` — so `client/registerCapability` is the only way to
/// ask, and `None` here means the client did not say it accepts one. Until v0.2.0 nothing sent
/// this at all: the VS Code extension supplied a watcher of its own through
/// `synchronize.fileEvents`, so reload worked there and in no other editor.
#[must_use]
pub fn config_watcher(root: &Path, capabilities: &ClientCapabilities) -> Option<Registration> {
    let watched_files = capabilities
        .workspace
        .as_ref()?
        .did_change_watched_files
        .as_ref()?;
    if watched_files.dynamic_registration != Some(true) {
        return None;
    }
    let options = DidChangeWatchedFilesRegistrationOptions {
        watchers: vec![FileSystemWatcher {
            glob_pattern: config_glob(root, watched_files.relative_pattern_support == Some(true)),
            // All three kinds, which is what omitting `kind` means. A deleted `ya-lsp.toml` is a
            // configuration change — it means back to the defaults — and so is one written for
            // the first time in a project that never had one.
            kind: None,
        }],
    };
    Some(Registration {
        id: CONFIG_WATCHER_ID.to_owned(),
        method: "workspace/didChangeWatchedFiles".to_owned(),
        // Infallible for this shape — every field in it is a string — but the type is a
        // `Result`, and a registration whose options went missing asks the client to watch
        // nothing at all. Better no registration than one that silently watches nothing.
        register_options: Some(serde_json::to_value(options).ok()?),
    })
}

/// The pattern the watcher is registered with.
///
/// Relative when the client says it takes one (LSP 3.17). The base there is a URI rather than
/// glob syntax, so a root holding `[`, `{`, `*` or `?` — a directory named `[wip]` is enough —
/// cannot be read as a pattern. The absolute form has no defence against that: LSP's glob
/// syntax defines no escape. Its separators are `/` on every platform, Windows included, which
/// is why the path is not simply handed over as the OS spells it.
fn config_glob(root: &Path, relative_patterns: bool) -> GlobPattern {
    if relative_patterns
        && let Some(base) = DocUri::from_path(root).and_then(|uri| uri.to_lsp().ok())
    {
        return GlobPattern::Relative(RelativePattern {
            base_uri: OneOf::Right(base),
            pattern: CONFIG_FILE_NAME.to_owned(),
        });
    }
    GlobPattern::String(
        root.join(CONFIG_FILE_NAME)
            .to_string_lossy()
            .replace('\\', "/"),
    )
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
    fn the_watcher_is_registered_against_the_project_root() {
        // The whole point of item 8: without this the server never asks anyone to watch
        // anything, and `ya-lsp.toml` only reloads in the one editor that brought its own
        // watcher.
        let root = Path::new("/tmp/ya-lsp-watch/project");
        let registration =
            config_watcher(root, &watching(Some(true), None)).expect("a watcher is registered");
        assert_eq!(registration.method, "workspace/didChangeWatchedFiles");
        assert_eq!(registration.id, CONFIG_WATCHER_ID);

        let watchers = watchers(&registration);
        assert_eq!(watchers.len(), 1);
        // `kind` unset is create|change|delete. A `ya-lsp.toml` that is deleted, or written for
        // the first time, changes the configuration exactly as much as an edit does.
        assert_eq!(watchers[0].kind, None);
        assert_eq!(
            watchers[0].glob_pattern,
            GlobPattern::String("/tmp/ya-lsp-watch/project/ya-lsp.toml".to_owned()),
            "a client without relative patterns gets the absolute one, with `/` separators"
        );
    }

    #[test]
    fn a_client_that_takes_relative_patterns_gets_one() {
        // The absolute form is glob syntax all the way down, so a root under a directory named
        // `[wip]` would be read as a character class and match nothing. The relative form's base
        // is a URI, so the only glob in it is the part we wrote.
        let root = Path::new("/tmp/ya-lsp-watch/[wip]/project");
        let registration = config_watcher(root, &watching(Some(true), Some(true)))
            .expect("a watcher is registered");
        let base = DocUri::from_path(root).expect("an absolute root").to_lsp();
        assert_eq!(
            watchers(&registration)[0].glob_pattern,
            GlobPattern::Relative(RelativePattern {
                base_uri: OneOf::Right(base.expect("a uri")),
                pattern: CONFIG_FILE_NAME.to_owned(),
            })
        );
    }

    #[test]
    fn a_relative_root_falls_back_to_the_pattern_it_can_still_spell() {
        // `workspace_root` ends at `.` when the client sent no folder and the process has no
        // working directory. There is no URI for that, so there is no relative pattern either —
        // and answering `None` here would drop the watcher over a case the absolute form
        // handles.
        let registration = config_watcher(Path::new("."), &watching(Some(true), Some(true)))
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
        // `ya-lsp.toml` and watching nothing happen.
        let root = Path::new("/tmp/ya-lsp-watch/project");
        assert!(config_watcher(root, &ClientCapabilities::default()).is_none());
        assert!(
            config_watcher(
                root,
                &ClientCapabilities {
                    workspace: Some(lsp_types::WorkspaceClientCapabilities::default()),
                    ..ClientCapabilities::default()
                }
            )
            .is_none(),
            "a workspace section that says nothing about watched files is still a no"
        );
        assert!(config_watcher(root, &watching(Some(false), None)).is_none());
        assert!(
            config_watcher(root, &watching(None, Some(true))).is_none(),
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
