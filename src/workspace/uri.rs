//! Canonical document URIs.
//!
//! rubydex keys documents by `url::Url::from_file_path(path).to_string()` (see
//! `rubydex::indexing::IndexingJob::run`). A client's URI forwarded verbatim can differ from that
//! spelling by percent-encoding or drive-letter case, and `didOpen` on an already-indexed file
//! would then fork a second document instead of replacing the first. So every URI entering the
//! server is round-tripped through `Url::from_file_path`, guaranteeing a byte-identical key.
//!
//! **Not every document is a file.** An unsaved buffer arrives as `untitled:` with no path to
//! round-trip, so its own spelling is the key. The scheme list in [`DocUri::adopt`] keeps that from
//! admitting every other scheme too.

use std::path::{Path, PathBuf};

use lsp_types::Uri;
use url::Url;

/// The path characters `url` writes literally and `lsp_types::Uri` rejects.
///
/// See [`DocUri::to_lsp`]. Kept beside the escape table it drives, so the two cannot drift.
const RFC_3986_REJECTS: [char; 4] = ['[', ']', '^', '|'];

/// A document URI spelled exactly the way rubydex spells it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocUri(String);

impl DocUri {
    /// Canonicalize a client-supplied URI. `None` for anything that is neither a local file nor an
    /// unsaved buffer: a remote scheme, or one of the server's own documents.
    #[must_use]
    pub fn from_lsp(uri: &Uri) -> Option<Self> {
        Self::adopt(&Url::parse(uri.as_str()).ok()?)
    }

    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        Url::from_file_path(path)
            .ok()
            .map(|url| Self(url.to_string()))
    }

    /// Adopt a URI string rubydex already holds, e.g. `Document::uri`.
    ///
    /// Still round-tripped through `Url`, not wrapped blindly: it costs nothing, and it rejects
    /// rubydex's synthetic `rubydex:built-in` document, which has no file behind it and must never
    /// reach the client.
    ///
    /// Named for where the string comes from, not what it is, because that is each caller's
    /// question: a URI from the graph may be a document this crate generated, and a URI from
    /// anywhere else should not be adopted here at all.
    #[must_use]
    pub fn from_graph_uri(uri: &str) -> Option<Self> {
        Self::adopt(&Url::parse(uri).ok()?)
    }

    /// The gate both string constructors go through: the scheme decides, and the list is closed.
    /// - `file:` is every document on disk, spelled as rubydex spells it.
    /// - `untitled:` is a buffer the user has not saved: no path until they save it. Admitted
    ///   anyway, because somebody typing Ruby into a new tab beside a Rails application wants its
    ///   constants resolved. Its own spelling is the key, since there is nothing to round-trip it
    ///   through.
    ///
    /// Everything else is refused, and the two that matter are refused **by name**, not by
    /// accident: rubydex files its built-in declarations under `rubydex:built-in`, and this crate
    /// files what a generator wrote under `ya-lsp-generated:`. Neither has a file behind it, and
    /// neither may become a `Location`, a symbol row or a diagnostic. Without this match they would
    /// be refused only because `Url::to_file_path` refuses an unknown scheme: a backstop that would
    /// quietly widen whenever the door does.
    ///
    /// A `file:` URI that is not a local path (`file://server/share/a.rb` off Windows) still
    /// answers `None`, not its own spelling: this process cannot read that file, and a key it
    /// cannot read from is worse than no key.
    fn adopt(url: &Url) -> Option<Self> {
        match url.scheme() {
            "file" => Self::from_path(&url.to_file_path().ok()?),
            "untitled" => Some(Self(url.to_string())),
            _ => None,
        }
    }

    /// Whether this document is a buffer with no file behind it.
    ///
    /// The one question `to_file_path` returning `None` does not answer by itself: a path is absent
    /// for a document that never had one, and *only* then. Asking directly says so at the call site
    /// instead of leaving a bare `None` arm to be read twice.
    #[must_use]
    pub fn is_untitled(&self) -> bool {
        self.0.starts_with("untitled:")
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The file behind this document, or `None` for a buffer that has none.
    ///
    /// Named for what it answers, not what it converts. **An untitled buffer is a document, not a
    /// file**, so every caller reading a path off one has a case to decide, not an `Option` whose
    /// `None` arm never runs.
    #[must_use]
    pub fn to_file_path(&self) -> Option<PathBuf> {
        Url::parse(&self.0).ok()?.to_file_path().ok()
    }

    /// Back to the wire type for responses.
    ///
    /// Four characters are re-encoded on the way out, and only on the way out:
    /// - `url` follows the WHATWG URL Standard, which leaves `[`, `]`, `^` and `|` in a path as
    ///   they are;
    /// - `lsp_types::Uri` is `fluent-uri`, which follows RFC 3986, where none of the four is a
    ///   legal path character, so it rejects the whole URI.
    ///
    /// All four are legal in directory names on macOS and Linux (three on Windows), and every
    /// caller here drops the error. Without the escape, a project under a directory named `[wip]`
    /// would publish no diagnostics, answer no definition and list no reference, with nothing said
    /// anywhere.
    ///
    /// The stored string keeps rubydex's spelling (the whole point of the type), and `from_lsp`
    /// decodes the escape straight back to it, so the key does not move.
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
/// Prefers the first workspace folder; falls back to the deprecated `rootUri`/`rootPath`, then to
/// the process working directory, so the server is still usable when launched by hand.
#[must_use]
pub fn workspace_root(params: &lsp_types::InitializeParams) -> PathBuf {
    root_from(params, std::env::current_dir().ok())
}

/// The same, with the working directory passed in.
///
/// Its own function for `gems::split_path_list`'s reason: the last rung reads process-wide state,
/// and a test making `current_dir` fail would have to delete the directory the whole test binary
/// runs in. Lifting it to the boundary keeps the decision testable and the syscall at the edge.
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

/// Whether two URIs name one document once every `%XX` escape is resolved.
///
/// **The URI a client asks about is not always the URI the server sent it.** It bites for a
/// document served under a scheme of the server's own (see
/// [`Synthesized::content`](crate::analysis::synthesized::Synthesized::content)). VS Code parses
/// every URI it is handed into a `vscode.Uri`, which percent-*decodes* each component, and
/// re-spells it on the way out with its own encoder, which keeps the unreserved characters plus `/`
/// and escapes the rest. A `:` inside a path is not in that set, so
/// `ya-lsp-generated:file:///…#class:Story` comes back as
/// `ya-lsp-generated:file%3A///…#class%3AStory`: the same URI, a different string, and a `HashMap`
/// lookup that misses.
///
/// So this is not `DocUri`'s canonicalisation under another name. `DocUri` round-trips through
/// `Url` to get **rubydex's** spelling of a `file:` URI, and cannot be used here at all: the whole
/// point of the generated scheme is that `Url::to_file_path` refuses it. This compares two
/// spellings without producing a third. It is deliberately one-way: it answers whether two strings
/// mean one URI and never returns a canonical form, because nothing is the authority on what that
/// form would be.
///
/// Byte-wise, not character-wise, so a malformed escape needs no decision: `%` followed by anything
/// but two hex digits is a literal `%`, and two URIs that both spell it that way still compare
/// equal.
#[must_use]
pub fn same_uri(left: &str, right: &str) -> bool {
    let mut left = unescaped(left);
    let mut right = unescaped(right);
    loop {
        let (left, right) = (left.next(), right.next());
        if left != right {
            return false;
        }
        if left.is_none() {
            return true;
        }
    }
}

/// One URI's bytes with every `%XX` resolved and everything else left exactly as it is.
fn unescaped(uri: &str) -> impl Iterator<Item = u8> + '_ {
    let bytes = uri.as_bytes();
    let mut at = 0;
    std::iter::from_fn(move || {
        let byte = *bytes.get(at)?;
        at += 1;
        if byte != b'%' {
            return Some(byte);
        }
        let (Some(high), Some(low)) = (hex(bytes.get(at)), hex(bytes.get(at + 1))) else {
            return Some(byte);
        };
        at += 2;
        Some(high * 16 + low)
    })
}

/// One hex digit's value, in either case, or `None` for anything else.
fn hex(byte: Option<&u8>) -> Option<u8> {
    match byte? {
        digit @ b'0'..=b'9' => Some(digit - b'0'),
        letter @ b'a'..=b'f' => Some(letter - b'a' + 10),
        letter @ b'A'..=b'F' => Some(letter - b'A' + 10),
        _ => None,
    }
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
        // caller of `to_lsp` drops the error, so without the escape a project under a directory
        // named `[wip]` would have no diagnostics, no go-to-definition and no references, silently
        // and all at once.
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
        // And back: the escape must decode to the same key, or a `didOpen` answering our own
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
    fn two_spellings_of_one_uri_are_one_uri() {
        let ours = "ya-lsp-generated:file:///project/db/schema.rb#class:Story";
        // What VS Code hands back: `Uri.parse` decodes every component, and `toString` escapes
        // everything outside the unreserved set plus `/`, which excludes `:` in a path.
        let theirs = "ya-lsp-generated:file%3A///project/db/schema.rb#class%3AStory";
        assert!(same_uri(ours, theirs));
        assert!(same_uri(theirs, ours));
        assert!(same_uri(ours, ours));
        // Either case of hex digit, which the two ends need not agree on.
        assert!(same_uri("file:///a%c3%a9.rb", "file:///a%C3%A9.rb"));

        // Two documents, not one. The second differs after the escapes are resolved, and the third
        // is a prefix of the first: the case a comparison that stopped at the shorter string would
        // get wrong.
        assert!(!same_uri(
            ours,
            "ya-lsp-generated:file%3A///project/db/schema.rb#class%3AWidget"
        ));
        assert!(!same_uri(
            ours,
            "ya-lsp-generated:file:///project/db/schema.rb"
        ));
        assert!(!same_uri(
            "ya-lsp-generated:file:///project/db/schema.rb",
            ours
        ));
    }

    #[test]
    fn a_percent_that_is_not_an_escape_is_a_percent() {
        // `%25` is the escape for `%`, the one every encoder agrees on.
        assert!(same_uri("file:///100%25.rb", "file:///100%.rb"));

        // And a `%` not followed by two hex digits needs no decision, because none is made: it
        // compares as the literal byte it is, in either position and at the very end, so two ends
        // that spell it alike agree, and two that do not are two URIs.
        assert!(same_uri("file:///a%zz.rb", "file:///a%zz.rb"));
        assert!(same_uri("file:///a%4", "file:///a%4"));
        assert!(!same_uri("file:///a%zz.rb", "file:///a.rb"));
        assert!(!same_uri("file:///a%4", "file:///a"));
    }

    #[test]
    fn an_unsaved_buffer_is_a_document_and_every_other_scheme_is_not() {
        // The door: an unsaved buffer keeps its own spelling, because there is no path to
        // canonicalize it through, and answers `None` for the file it does not have.
        let untitled: Uri = "untitled:Untitled-1".parse().unwrap();
        let doc = DocUri::from_lsp(&untitled).expect("an unsaved buffer is a document");
        assert_eq!(doc.as_str(), "untitled:Untitled-1");
        assert!(doc.is_untitled());
        assert_eq!(doc.to_file_path(), None);
        assert_eq!(doc.to_lsp().unwrap().as_str(), "untitled:Untitled-1");

        // And the list is closed. Anything else a client may send is still refused, including the
        // two schemes `Url::to_file_path` would refuse anyway.
        for refused in [
            "rubydex:built-in",
            "ya-lsp-generated:file:///project/db/schema.rb#class:Story",
            "vscode-notebook-cell:/tmp/a.rb#W1",
            "https://example.test/person.rb",
        ] {
            let uri: Uri = refused.parse().unwrap();
            assert!(
                DocUri::from_lsp(&uri).is_none(),
                "{refused} has no file behind it"
            );
            assert!(DocUri::from_graph_uri(refused).is_none(), "{refused}");
        }

        // A `file:` URI this process cannot read a path out of is refused too, instead of falling
        // through to its own spelling as an untitled buffer does.
        #[cfg(not(target_os = "windows"))]
        assert!(DocUri::from_graph_uri("file://server/share/person.rb").is_none());

        // A real file is untouched by any of it.
        let ordinary = DocUri::from_path(Path::new("/tmp/ya-lsp-test/lib/person.rb")).unwrap();
        assert!(!ordinary.is_untitled());
    }

    #[test]
    fn a_graph_uri_round_trips_but_the_synthetic_document_does_not() {
        let path = Path::new("/tmp/ya-lsp-test/lib/foo.rb");
        let canonical = DocUri::from_path(path).unwrap();
        assert_eq!(
            DocUri::from_graph_uri(canonical.as_str()).as_ref(),
            Some(&canonical)
        );
        // rubydex registers a built-in document under its own scheme; it has no file and must never
        // be published to an editor.
        assert!(DocUri::from_graph_uri("rubydex:built-in").is_none());
    }

    #[test]
    fn the_workspace_root_falls_back_through_every_shape_a_client_may_send() {
        // All three rungs. A client sending neither `workspaceFolders` nor `rootUri` is not
        // hypothetical (`rootPath` predates both, and editors other than VS Code still send it),
        // and *everything* downstream hangs off the answer: which `ya-lsp.toml` is read, which gem
        // roots are searched, and which files count as the user's own for diagnostics. Getting it
        // wrong indexes the wrong tree and reports nothing about the right one.
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
        // process started rather than nowhere.
        assert_eq!(
            workspace_root(&lsp_types::InitializeParams::default()),
            std::env::current_dir().expect("a working directory")
        );

        // The last rung of all: no folders, no root, and no working directory either (the directory
        // the editor launched from was deleted under it). `.` keeps the server answering about open
        // buffers instead of failing to start.
        assert_eq!(
            root_from(&lsp_types::InitializeParams::default(), None),
            PathBuf::from(".")
        );
    }

    #[test]
    fn path_round_trip() {
        let path = Path::new("/tmp/ya-lsp-test/lib/foo.rb");
        let uri = DocUri::from_path(path).unwrap();
        assert_eq!(uri.to_file_path().as_deref(), Some(path));
    }
}
