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

/// The path characters `url` writes literally and `lsp_types::Uri` will not accept.
///
/// See [`DocUri::to_lsp`]. Kept beside the escape table it drives so the two cannot drift.
const RFC_3986_REJECTS: [char; 4] = ['[', ']', '^', '|'];

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
    /// Four characters are re-encoded on the way out, and only on the way out. `url` follows the
    /// WHATWG URL Standard, which leaves `[`, `]`, `^` and `|` in a path exactly as they are;
    /// `lsp_types::Uri` is `fluent-uri`, which follows RFC 3986, where none of the four is a
    /// legal path character — so it rejects the whole URI rather than the character. All four
    /// are legal in a directory name on macOS and Linux, and three of them on Windows, and every
    /// caller here drops the error: a project under a directory named `[wip]` published no
    /// diagnostics, answered no definition and listed no reference, with nothing said anywhere.
    /// The stored string keeps rubydex's spelling — that is what the whole type exists for — and
    /// `from_lsp` decodes the escape straight back to it, so the key is unmoved.
    ///
    /// # Errors
    ///
    /// Only if the stored string is not a valid URI, which cannot happen for a `DocUri` built
    /// through the constructors above.
    pub fn to_lsp(&self) -> Result<Uri, <Uri as std::str::FromStr>::Err> {
        if !self.0.contains(RFC_3986_REJECTS) {
            return self.0.parse();
        }
        let mut encoded = String::with_capacity(self.0.len());
        for character in self.0.chars() {
            match character {
                '[' => encoded.push_str("%5B"),
                ']' => encoded.push_str("%5D"),
                '^' => encoded.push_str("%5E"),
                '|' => encoded.push_str("%7C"),
                other => encoded.push(other),
            }
        }
        encoded.parse()
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
    root_from(params, std::env::current_dir().ok())
}

/// The same, with the working directory passed in.
///
/// Its own function for the reason `gems::split_path_list` is: the last rung reads process-wide
/// state, and a test that made `current_dir` fail would have to delete the directory the whole
/// test binary is running in. Lifting it to the boundary leaves the decision testable and the
/// syscall at the edge.
fn root_from(params: &lsp_types::InitializeParams, cwd: Option<PathBuf>) -> PathBuf {
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

    // `.` rather than a panic: a server with no root still answers about open buffers, and an
    // editor that cannot start one is worse than one that indexes nothing.
    from_params.or(cwd).unwrap_or_else(|| PathBuf::from("."))
}

#[cfg_attr(coverage_nightly, coverage(off))]
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
    fn the_four_characters_rfc_3986_forbids_survive_the_trip_to_the_client() {
        // `url` and `lsp_types::Uri` follow different standards, and these four are where they
        // disagree: `url` writes them literally, `fluent-uri` rejects the URI outright. Every
        // caller of `to_lsp` drops the error, so a project under a directory named `[wip]` had
        // no diagnostics, no go-to-definition and no references — silently, and all at once.
        let path = Path::new("/tmp/ya-lsp-test/[wip]^v1|old/person.rb");
        let uri = DocUri::from_path(path).expect("an absolute path");
        assert_eq!(
            uri.as_str(),
            "file:///tmp/ya-lsp-test/[wip]^v1|old/person.rb",
            "the stored spelling is rubydex's, unchanged — that is what the key is for"
        );

        let on_the_wire = uri.to_lsp().expect("a uri the client can parse");
        assert_eq!(
            on_the_wire.as_str(),
            "file:///tmp/ya-lsp-test/%5Bwip%5D%5Ev1%7Cold/person.rb"
        );
        // And back: the escape has to decode to the same key, or a `didOpen` answering our own
        // location would fork a second document.
        assert_eq!(DocUri::from_lsp(&on_the_wire).expect("a file uri"), uri);
    }

    #[test]
    fn a_uri_displays_as_the_string_it_holds() {
        // Every log line naming a document goes through this, and a document forking into two
        // spellings is diagnosed by reading exactly those lines.
        let uri = DocUri::from_path(Path::new("/tmp/ya-lsp-test/lib/person.rb")).unwrap();
        assert_eq!(format!("{uri}"), uri.as_str());
        assert_eq!(uri.to_lsp().unwrap().as_str(), uri.as_str());
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
    fn the_workspace_root_falls_back_through_every_shape_a_client_may_send() {
        // Three rungs, and only the first had a test. A client that sends neither
        // `workspaceFolders` nor `rootUri` is not hypothetical — `rootPath` predates both and
        // editors that are not VS Code still send it — and *everything* downstream hangs off
        // the answer: which `ya-lsp.toml` is read, which gem roots are searched, and which
        // files count as the user's own for diagnostics. Getting it wrong indexes the wrong
        // tree and reports nothing about the right one.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let uri: Uri = Url::from_file_path(root).unwrap().as_str().parse().unwrap();

        let folders = lsp_types::InitializeParams {
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: uri.clone(),
                name: "ws".to_owned(),
            }]),
            ..lsp_types::InitializeParams::default()
        };
        assert_eq!(workspace_root(&folders), root);

        #[allow(deprecated)]
        let root_uri = lsp_types::InitializeParams {
            root_uri: Some(uri),
            ..lsp_types::InitializeParams::default()
        };
        assert_eq!(workspace_root(&root_uri), root);

        // The deprecated third rung: a plain path, not a URI.
        #[allow(deprecated)]
        let root_path = lsp_types::InitializeParams {
            root_path: Some(root.to_string_lossy().into_owned()),
            ..lsp_types::InitializeParams::default()
        };
        assert_eq!(workspace_root(&root_path), root);

        // And a client that says nothing at all still gets a usable server, rooted where the
        // process was started rather than nowhere.
        assert_eq!(
            workspace_root(&lsp_types::InitializeParams::default()),
            std::env::current_dir().expect("a working directory")
        );

        // The last rung of all: no folders, no root, and no working directory either — the
        // directory the editor launched from having been deleted under it. `.` keeps the
        // server answering about open buffers instead of failing to start.
        assert_eq!(
            root_from(&lsp_types::InitializeParams::default(), None),
            PathBuf::from(".")
        );
    }

    #[test]
    fn path_round_trip() {
        let path = Path::new("/tmp/ya-lsp-test/lib/foo.rb");
        let uri = DocUri::from_path(path).unwrap();
        assert_eq!(uri.to_path().as_deref(), Some(path));
    }
}
