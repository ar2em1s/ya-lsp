//! Canonical document URIs.
//!
//! rubydex keys documents by `url::Url::from_file_path(path).to_string()` (see
//! `rubydex::indexing::IndexingJob::run`). If we forward a client's URI verbatim it can differ
//! from that spelling by percent-encoding or drive-letter case, and `didOpen` on an
//! already-indexed file would fork a second document instead of replacing the first. So every
//! URI that enters the server is round-tripped through `Url::from_file_path` to guarantee a
//! byte-identical key.

use std::path::{Path, PathBuf};

use lsp_types::Uri;
use url::Url;

/// A document URI spelled exactly the way rubydex spells it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocUri(String);

impl DocUri {
    /// Canonicalize a client-supplied URI. Returns `None` for anything that is not a local
    /// file (untitled buffers, remote schemes) — we have nothing to index for those.
    #[must_use]
    pub fn from_lsp(uri: &Uri) -> Option<Self> {
        let parsed = Url::parse(uri.as_str()).ok()?;
        let path = parsed.to_file_path().ok()?;
        Self::from_path(&path)
    }

    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        Url::from_file_path(path)
            .ok()
            .map(|url| Self(url.to_string()))
    }

    /// Adopt a URI string that rubydex already holds, e.g. `Document::uri`.
    ///
    /// Still round-tripped through `Url` rather than wrapped blindly: it costs nothing and it
    /// rejects rubydex's synthetic `rubydex:built-in` document, which has no file behind it and
    /// must never reach the client.
    #[must_use]
    pub fn from_uri_str(uri: &str) -> Option<Self> {
        let path = Url::parse(uri).ok()?.to_file_path().ok()?;
        Self::from_path(&path)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn to_path(&self) -> Option<PathBuf> {
        Url::parse(&self.0).ok()?.to_file_path().ok()
    }

    /// Back to the wire type for responses.
    ///
    /// # Errors
    ///
    /// Only if the stored string is not a valid URI, which cannot happen for a `DocUri` built
    /// through the constructors above.
    pub fn to_lsp(&self) -> Result<Uri, <Uri as std::str::FromStr>::Err> {
        self.0.parse()
    }
}

impl std::fmt::Display for DocUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolve the workspace root from `initialize` params.
///
/// Prefers the first workspace folder; falls back to the deprecated `rootUri`/`rootPath`, then
/// to the process working directory so the server is still usable when launched by hand.
#[must_use]
pub fn workspace_root(params: &lsp_types::InitializeParams) -> PathBuf {
    #[allow(deprecated)]
    let from_params = params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .and_then(|folder| Url::parse(folder.uri.as_str()).ok())
        .and_then(|url| url.to_file_path().ok())
        .or_else(|| {
            params
                .root_uri
                .as_ref()
                .and_then(|uri| Url::parse(uri.as_str()).ok())
                .and_then(|url| url.to_file_path().ok())
        })
        .or_else(|| params.root_path.as_ref().map(PathBuf::from));

    from_params.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_to_the_same_spelling_rubydex_uses() {
        let path = Path::new("/tmp/ya-lsp-test/a b/caf\u{e9}.rb");
        let ours = DocUri::from_path(path).unwrap();
        let rubydex_style = Url::from_file_path(path).unwrap().to_string();
        assert_eq!(ours.as_str(), rubydex_style);
    }

    #[test]
    fn client_uri_round_trips_to_the_canonical_form() {
        let path = Path::new("/tmp/ya-lsp-test/a b/caf\u{e9}.rb");
        let canonical = DocUri::from_path(path).unwrap();

        // A client that percent-encodes differently still lands on the same key.
        let client_spelling: Uri = "file:///tmp/ya-lsp-test/a%20b/caf%C3%A9.rb"
            .parse()
            .unwrap();
        assert_eq!(DocUri::from_lsp(&client_spelling).unwrap(), canonical);
    }

    #[test]
    fn non_file_schemes_are_rejected() {
        let untitled: Uri = "untitled:Untitled-1".parse().unwrap();
        assert!(DocUri::from_lsp(&untitled).is_none());
    }

    #[test]
    fn a_graph_uri_round_trips_but_the_synthetic_document_does_not() {
        let path = Path::new("/tmp/ya-lsp-test/lib/foo.rb");
        let canonical = DocUri::from_path(path).unwrap();
        assert_eq!(
            DocUri::from_uri_str(canonical.as_str()).as_ref(),
            Some(&canonical)
        );
        // rubydex registers a built-in document under its own scheme; it has no file and no
        // business being published to an editor.
        assert!(DocUri::from_uri_str("rubydex:built-in").is_none());
    }

    #[test]
    fn path_round_trip() {
        let path = Path::new("/tmp/ya-lsp-test/lib/foo.rb");
        let uri = DocUri::from_path(path).unwrap();
        assert_eq!(uri.to_path().as_deref(), Some(path));
    }
}
