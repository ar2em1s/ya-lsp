//! What the server tells the client it can do.
//!
//! Capabilities are announced per milestone: advertising a feature we do not implement makes
//! the editor show an empty result instead of falling back to its own heuristics, which is a
//! worse experience than not advertising at all.

use lsp_types::{
    CompletionOptions, CompletionOptionsCompletionItem, HoverProviderCapability, OneOf,
    SaveOptions, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions,
    WorkspaceFileOperationsServerCapabilities, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};

use crate::analysis::position::PositionEncoding;

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

#[must_use]
pub fn sync_kind(capabilities: &ServerCapabilities) -> Option<TextDocumentSyncKind> {
    match capabilities.text_document_sync.as_ref()? {
        TextDocumentSyncCapability::Kind(kind) => Some(*kind),
        TextDocumentSyncCapability::Options(options) => options.change,
    }
}

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
}
