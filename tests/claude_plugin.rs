//! The Claude Code plugin's three manifests, checked against the server they configure.
//!
//! `editors/claude-code/.lsp.json` is the whole of the plugin's behaviour: which binary is spawned,
//! and which files are ever routed to it. Neither language can see across that boundary: the
//! manifests are JSON read by a client this repository does not build, and what they claim about
//! the server is a second copy of lists the server keeps in Rust. Every failure is silent in the
//! same direction: a file the plugin does not map is a file no agent can ask about, with nothing
//! logged, because no request is ever sent.
//!
//! `tests/vscode_manifest.rs` is the same test for the other editor. It has more to check because
//! VS Code exposes settings; this one is short because the answer to configuration here is
//! `ya-lsp.toml` and nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use ya_lsp::{analysis::erb, server::capabilities::LANGUAGE_IDS, workspace::Workspace};

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn plugin_root() -> PathBuf {
    repository().join("editors/claude-code")
}

fn json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{path:?} is not valid JSON: {error}"))
}

/// The one server the plugin declares, and the key it is declared under.
fn declared_server() -> (String, Value) {
    let servers = json(&plugin_root().join(".lsp.json"));
    let servers = servers
        .as_object()
        .expect("`.lsp.json` is an object keyed by server name");
    assert_eq!(
        servers.len(),
        1,
        "a second server in this file would be a second ya-lsp spawned per project"
    );
    let (name, server) = servers.iter().next().expect("the one server");
    (name.clone(), server.clone())
}

fn extension_to_language() -> BTreeMap<String, String> {
    let (_, server) = declared_server();
    server["extensionToLanguage"]
        .as_object()
        .expect("the plugin routes files to the server by extension and nothing else")
        .iter()
        .map(|(extension, language)| {
            (
                extension.clone(),
                language
                    .as_str()
                    .expect("a language id is a string")
                    .to_owned(),
            )
        })
        .collect()
}

/// Every extension the plugin routes to ya-lsp is one the default walk indexes.
///
/// `extensionToLanguage` is the only gate: Claude Code sends a file to a server because its
/// extension is on this list, and nowhere otherwise. An extension here that `index.include` does
/// not match is a file the agent can open and ask about, and get wrong answers for: the server
/// indexes the buffer it is handed, but the graph around it came from a walk that skipped every
/// other file of that kind.
///
/// The check is `Workspace::indexes`, not a second reading of the glob list, because that is the
/// predicate the walk and the watcher both ask.
///
/// **`.ru` is the sharp case.** Claude Code can only route by extension, so `index.include` must
/// name `**/*.ru`, not just the fixed `**/config.ru`. Otherwise every rackup file an application
/// mounts beside the conventional one (`admin.ru`, `sidekiq.ru`) is sent to ya-lsp and indexed by
/// nothing, in silence. The bottom of this test holds the two sides together.
#[test]
fn every_extension_the_plugin_routes_is_one_the_default_walk_indexes() {
    let directory = tempfile::tempdir().expect("a temporary workspace");
    let root = directory.path().to_path_buf();
    // No `ya-lsp.toml`, so this is `Config::default()`: what a project that configures nothing
    // gets, the case the plugin is written for.
    let (workspace, problems) = Workspace::load(root.clone(), None);
    assert!(problems.is_empty(), "{problems:?}");

    for extension in extension_to_language().keys() {
        let path = root.join(format!("probe{extension}"));
        std::fs::write(&path, "# frozen_string_literal: true\n").expect("the probe file");
        assert!(
            workspace.indexes(&path),
            "the plugin routes {extension} to ya-lsp and the default walk would not index it"
        );
    }

    // The guard, so the loop above cannot pass by accepting everything: `index.include` lists the
    // shapes Ruby is written in, and a file that is not Ruby must be refused.
    let refused = root.join("probe.py");
    std::fs::write(&refused, "pass\n").expect("the guard file");
    assert!(!workspace.indexes(&refused));

    // Both sides of `.ru`, at the root and at depth, because this is the one extension whose glob
    // could be a *name*, and a project mounting several rackup files is exactly the one that would
    // see nothing. The plugin routes both, so both must be indexed.
    for rackup in ["admin.ru", "ops/sidekiq.ru"] {
        let path = root.join(rackup);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the rackup directory");
        }
        std::fs::write(&path, "run App\n").expect("the rackup file");
        assert!(
            workspace.indexes(&path),
            "{rackup} is routed to ya-lsp by extension and the walk would not index it"
        );
    }
}

/// What the plugin cannot reach, stated as a test instead of left to be discovered.
/// - `Rakefile` and `Gemfile` are on `index.include` and have no extension, so
///   `extensionToLanguage` cannot name them and never will. A cursor in one is unreachable from
///   Claude Code, though reachable from VS Code.
/// - `.rbs` is indexed and deliberately not routed: a signature file is read *for* an answer, not
///   asked about.
///
/// Adding one of these to `.lsp.json` fails here and gets thought about, instead of the gap being
/// rediscovered from a user's report.
#[test]
fn the_three_indexed_shapes_the_plugin_cannot_route_are_the_ones_named_here() {
    let mapped: BTreeSet<String> = extension_to_language().into_keys().collect();

    assert!(
        !mapped.contains(".rbs"),
        "routing .rbs needs its own decision"
    );
    // Every rackup file is reachable, because `.ru` is an extension; the two extensionless names
    // are not, and no shape of `extensionToLanguage` will make them so.
    assert!(mapped.contains(".ru"));
}

/// The extensions the plugin calls a template are the extensions the server blanks.
///
/// The same pair as `vscode_manifest::the_editor_and_the_server_agree_on_what_an_erb_template_is`,
/// with the same sharp failure: a file routed as Ruby that `erb::is_template` also claims would be
/// indexed twice over, and a template the server blanks but the plugin never routes opens as text
/// an agent cannot query. ya-lsp reads no `languageId` anywhere in `src/`, so the id itself costs
/// nothing, but a reader of this file believes it, so it must be true.
#[test]
fn the_plugin_and_the_server_agree_on_what_a_template_is() {
    for (extension, language) in extension_to_language() {
        let name = format!("index.html{extension}");
        let template = erb::is_template(Path::new(&name));
        assert_eq!(
            template,
            language == "erb",
            "{extension} is mapped to {language} and the server disagrees about whether it is a template"
        );
        assert!(
            LANGUAGE_IDS.contains(&language.as_str()),
            "{extension} is mapped to {language}, which the server never claims in a registration"
        );
    }
}

/// The command is the bare binary, found on `PATH`, with no configurable path.
///
/// Claude Code does not apply a `userConfig` default on the LSP path: it reads the *stored* options
/// and throws "Plugin option ... isn't set" for one never saved, and the server is then never
/// registered. So a `server_path` option would leave every user who accepted its default with no
/// server and no explanation. A binary kept elsewhere goes on `PATH`.
#[test]
fn the_command_is_the_bare_binary_with_no_configurable_path() {
    let (name, server) = declared_server();
    assert_eq!(name, "ya-lsp");
    assert_eq!(server["command"], "ya-lsp");
    assert_eq!(server["args"], serde_json::json!(["--stdio"]));

    let text = std::fs::read_to_string(plugin_root().join(".lsp.json")).expect(".lsp.json");
    assert!(
        !text.contains("userConfig") && !text.contains("server_path"),
        "a plugin option whose default is never applied means no server at all"
    );
}

/// `initializationOptions` is either absent or something the server can actually read.
///
/// `PartialConfig` is deserialized with `deny_unknown_fields`, which does not degrade: one unknown
/// key rejects the *entire* layer, so every setting in it silently stops working, not just the bad
/// one. There is no such key (the answer here is `ya-lsp.toml`, which outranks every other layer
/// anyway); this test makes adding one a checked change.
#[test]
fn any_initialization_options_are_ones_the_server_can_read() {
    use ya_lsp::workspace::config::PartialConfig;

    let (_, server) = declared_server();
    if let Some(options) = server.get("initializationOptions") {
        serde_json::from_value::<PartialConfig>(options.clone())
            .expect("the plugin sends initializationOptions the server rejects whole");
    }

    // The guard, so the absence above is not what makes this pass: the check must bite.
    let bogus = serde_json::json!({ "index": { "this-is-not-a-field": 1 } });
    assert!(serde_json::from_value::<PartialConfig>(bogus).is_err());
}

/// The marketplace entry names the plugin that is actually here.
///
/// The root marketplace installs the plugin from this repository before the official entry's pinned
/// commit moves: the only way to test a plugin change at all. Its `source` is a path into this
/// repository, so it is the one cross-file claim here a rename would break silently:
/// `/plugin marketplace add` reports a missing plugin, and no build ever asks.
#[test]
fn the_marketplace_entry_points_at_the_plugin_that_is_here() {
    let marketplace = json(&repository().join(".claude-plugin/marketplace.json"));
    let plugins = marketplace["plugins"]
        .as_array()
        .expect("the marketplace lists its plugins");
    assert_eq!(
        plugins.len(),
        1,
        "one plugin, and this repository is its home"
    );

    let entry = &plugins[0];
    let source = entry["source"].as_str().expect("the entry names a source");
    let source_path = repository().join(source.trim_start_matches("./"));
    assert_eq!(
        source_path,
        plugin_root(),
        "{source} is not where the plugin is"
    );

    let manifest = json(&plugin_root().join(".claude-plugin/plugin.json"));
    assert_eq!(entry["name"], manifest["name"]);
    assert_eq!(marketplace["name"], manifest["name"]);
}

/// The plugin's version is the server's version.
///
/// The server, the VS Code extension and this plugin ship as one version (`CHANGELOG.md` says so in
/// its first line), and a manifest version is the only one of the three a human types. It is also
/// what an installed copy shows in `/plugin`, so a stale one means a user reading the wrong release
/// notes for the binary they run.
#[test]
fn the_plugin_version_is_the_crate_version() {
    let manifest = json(&plugin_root().join(".claude-plugin/plugin.json"));
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
}

/// Nothing under the plugin directory is executable, because no binary is committed here.
///
/// Distribution is the release archives and `SHA256SUMS`. `.lsp.json` names a command and cannot
/// select one per platform, so a committed binary would be the wrong one for somebody. This check
/// catches a binary added "just for testing": the plugin directory is three JSON files and the
/// skill's markdown.
#[cfg(unix)]
#[test]
fn nothing_under_the_plugin_directory_is_executable() {
    use std::os::unix::fs::PermissionsExt;

    fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("the plugin directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(&plugin_root(), &mut files);
    assert!(!files.is_empty(), "the plugin directory is empty");

    for path in files {
        let mode = std::fs::metadata(&path)
            .expect("a plugin file")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o111,
            0,
            "{path:?} is executable; no binary is committed with the plugin"
        );
    }
}

/// The one skill the plugin ships, and its frontmatter.
fn skill() -> String {
    let path = plugin_root().join("skills/ya-lsp-setup/SKILL.md");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"))
}

/// Claude Code finds a plugin's skills at `skills/<name>/SKILL.md` and reads the `name` from the
/// frontmatter, so the two spellings must agree, or the skill is listed under one name and invoked
/// under another.
///
/// The description decides whether the skill is ever *reached*: it is all a model sees before
/// loading the body, so it must name the symptoms as well as the task.
#[test]
fn the_skill_declares_the_name_its_directory_spells() {
    let text = skill();
    let frontmatter = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .expect("the skill opens with frontmatter")
        .0;

    assert!(frontmatter.contains("name: ya-lsp-setup"));
    let description = frontmatter
        .lines()
        .find_map(|line| line.strip_prefix("description: "))
        .expect("the frontmatter carries a description");
    assert!(
        description.len() > 80,
        "a one-line description is what a model matches a request against"
    );
}

/// Every `ya-lsp.toml` the skill prints is one the server can actually read.
///
/// `PartialConfig` is deserialized with `deny_unknown_fields`, which does not degrade: one mistyped
/// key rejects the whole file, so a user who pastes the skill's block loses every setting in it at
/// once, and is told so in a sentence Claude Code then drops. The skill is a page of settings
/// nothing compiles, and this is the only check on them.
#[test]
fn the_settings_the_skill_prints_are_ones_the_server_reads() {
    let text = skill();
    let mut blocks: Vec<String> = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("```toml\n") {
        let after = &rest[start + "```toml\n".len()..];
        let end = after
            .find("```")
            .expect("an unterminated toml block in the skill");
        blocks.push(after[..end].to_owned());
        rest = &after[end..];
    }
    assert!(!blocks.is_empty(), "the skill prints no configuration");

    for block in &blocks {
        let directory = tempfile::tempdir().expect("a temporary workspace");
        std::fs::write(directory.path().join("ya-lsp.toml"), block).expect("the config file");
        let loaded = ya_lsp::workspace::config::load(directory.path(), None);
        assert!(
            loaded.problems.is_empty(),
            "the skill prints a ya-lsp.toml the server refuses: {:?}\n{block}",
            loaded.problems
        );
    }

    // The guard, so the loop above cannot pass by accepting anything: a key with a typo must be
    // refused, or this test says nothing about the blocks it just read.
    let directory = tempfile::tempdir().expect("a temporary workspace");
    std::fs::write(
        directory.path().join("ya-lsp.toml"),
        "[gems]\npathz = [\"/usr/local/bundle\"]\n",
    )
    .expect("the config file");
    assert!(
        !ya_lsp::workspace::config::load(directory.path(), None)
            .problems
            .is_empty()
    );
}

/// The archives the skill tells a user to download are the archives the release actually builds.
///
/// The names are the release matrix's Rust target triples, and nothing else connects the two: the
/// workflow is YAML read by GitHub, and the skill is markdown read by a model. A target added,
/// dropped or renamed leaves the skill pointing at a URL that 404s, after the user agreed to an
/// install: the worst moment for it.
///
/// It checks both directions:
/// - a target the skill does not name is a platform silently told to build from source;
/// - an archive the skill names that the matrix does not build is the 404.
#[test]
fn the_archives_the_skill_names_are_the_ones_the_release_builds() {
    let workflow = std::fs::read_to_string(repository().join(".github/workflows/release.yml"))
        .expect("the release workflow");

    let built: BTreeSet<String> = workflow
        .lines()
        .filter_map(|line| line.split("rust:").nth(1))
        .map(|rest| rest.trim().trim_end_matches('}').trim().to_owned())
        .map(|target| {
            let extension = if target.contains("windows") {
                "zip"
            } else {
                "tar.gz"
            };
            format!("ya-lsp-{target}.{extension}")
        })
        .collect();
    assert_eq!(built.len(), 5, "the release matrix is five rows: {built:?}");

    let text = skill();
    for archive in &built {
        assert!(
            text.contains(archive.as_str()),
            "the release builds {archive} and the skill never names it"
        );
    }

    // The other direction: every `ya-lsp-…` archive the skill spells exists. A scan, not a second
    // list, so a name invented in prose is caught too.
    for (at, _) in text.match_indices("ya-lsp-") {
        let named: String = text[at..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c))
            .collect();
        let named = named.trim_end_matches('.');
        if named.ends_with(".tar.gz") || named.ends_with(".zip") {
            assert!(
                built.contains(named),
                "the skill names {named}, which the release does not build"
            );
        }
    }
}

/// The skill names every extension the plugin routes, because those are the files it can probe.
///
/// Step 3 asks the user's own project a question, and the file it picks must be one Claude Code
/// sends to this server. An extension added to `.lsp.json` without a line here leaves the skill
/// probing with only some of the shapes, which fails as an empty answer, not an error.
#[test]
fn the_skill_names_the_extensions_the_plugin_routes() {
    let text = skill();
    for extension in extension_to_language().keys() {
        assert!(
            text.contains(&format!("`{extension}`")),
            "the plugin routes {extension} and the skill never mentions it"
        );
    }
}
