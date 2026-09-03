//! The VS Code manifest, checked against the server it configures.
//!
//! `editors/vscode/package.json` is where a user reads what a setting does and what it defaults
//! to, and nothing in either language can see across that boundary. A documented default that
//! drifts from `Config::default()` is wrong documentation with no failing test behind it, which
//! is how `ya-lsp.logLevel` shipped two releases saying `warn` about a server whose fallback is
//! `info`. The rule names of `[diagnostics.rules]` are the same shape of hazard from the other
//! side: they have to be written down in the manifest for the editor to complete them, and a
//! second copy of a list is a list that goes stale.
//!
//! `editors/vscode/src/manifest.test.ts` holds the halves this cannot see — the scopes, and the
//! agreement between the manifest and the settings `config.ts` actually reads.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use ya_lsp::{
    DEFAULT_LOG_FILTER, analysis::diagnostics, workspace::Config, workspace::Severity,
    workspace::config::PartialConfig,
};

fn manifest() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/editors/vscode/package.json");
    let text = std::fs::read_to_string(path).expect("the extension manifest");
    serde_json::from_str(&text).expect("the extension manifest is valid JSON")
}

/// Every `ya-lsp.*` property the manifest declares, flattened out of its five categories.
fn properties(manifest: &Value) -> BTreeMap<String, Value> {
    manifest["contributes"]["configuration"]
        .as_array()
        .expect("the configuration is an array of titled categories")
        .iter()
        .flat_map(|category| {
            category["properties"]
                .as_object()
                .expect("a category declares properties")
                .iter()
                .map(|(name, property)| (name.clone(), property.clone()))
        })
        .collect()
}

#[test]
fn every_documented_default_is_the_one_the_server_actually_uses() {
    let manifest = manifest();
    let properties = properties(&manifest);
    let config = Config::default();

    // An empty map is what `{}` in the manifest documents, and it is the only default here that
    // cannot be written as a `json!` of the field itself — `Severity` is deserialized, never
    // serialized, because nothing in the server ever sends one back.
    assert!(config.diagnostics.rules.is_empty());

    // Left: what the settings UI tells the user. Right: what the server does when nobody has set
    // anything. Reading settings with `inspect` means the left half is never sent, so it is
    // documentation and nothing else — and documentation of a default rots without being noticed.
    let expected: Vec<(&str, Value)> = vec![
        ("ya-lsp.logLevel", json!(DEFAULT_LOG_FILTER)),
        ("ya-lsp.gems.enabled", json!(config.gems.enabled)),
        ("ya-lsp.gems.defaultGems", json!(config.gems.default_gems)),
        // `None` is spelled `""`. "Detect it" and "find one yourself" have no value to show, and
        // a setting with no default at all reads as one nobody thought about.
        (
            "ya-lsp.gems.rubyVersion",
            json!(config.gems.ruby_version.clone().unwrap_or_default()),
        ),
        ("ya-lsp.gems.paths", json!(config.gems.paths)),
        ("ya-lsp.rbs.enabled", json!(config.rbs.enabled)),
        ("ya-lsp.rbs.stdlib", json!(config.rbs.stdlib)),
        (
            "ya-lsp.rbs.path",
            json!(config.rbs.path.clone().unwrap_or_default()),
        ),
        (
            "ya-lsp.diagnostics.enabled",
            json!(config.diagnostics.enabled),
        ),
        ("ya-lsp.diagnostics.rules", json!({})),
        ("ya-lsp.index.include", json!(config.index.include)),
        ("ya-lsp.index.exclude", json!(config.index.exclude)),
        ("ya-lsp.index.loadPaths", json!(config.index.load_paths)),
        ("ya-lsp.index.maxFiles", json!(config.index.max_files)),
        (
            "ya-lsp.index.respectGitignore",
            json!(config.index.respect_gitignore),
        ),
    ];

    for (setting, server_default) in &expected {
        let declared = properties
            .get(*setting)
            .unwrap_or_else(|| panic!("{setting} is not declared in the manifest at all"));
        assert_eq!(
            &declared["default"], server_default,
            "{setting} documents a default the server does not have"
        );
    }

    // The other direction: nothing may be added to the manifest without landing in the table
    // above. The two exceptions are named rather than matched by pattern, because both are the
    // extension's own business and neither has a server-side default to drift from — where the
    // binary lives, and whether the client traces its own traffic.
    let checked: BTreeSet<&str> = expected.iter().map(|(setting, _)| *setting).collect();
    let unchecked: Vec<&str> = properties
        .keys()
        .map(String::as_str)
        .filter(|setting| !checked.contains(setting))
        .collect();
    assert_eq!(unchecked, ["ya-lsp.serverPath", "ya-lsp.trace.server"]);
}

#[test]
fn the_rule_map_documents_exactly_the_rules_that_exist() {
    // `[diagnostics.rules]` is the one setting whose *keys* an editor cannot guess: an open
    // object completes its values and never its names, which is why nobody finds `parse-warning`
    // without being told it exists. Declaring the ten fixes that and makes the list a second
    // copy; `describe`'s match already breaks the build when rubydex adds an eleventh rule, and
    // this is what makes it break the extension too rather than shipping a list one short.
    let manifest = manifest();
    let mut declared: Vec<String> = properties(&manifest)["ya-lsp.diagnostics.rules"]["properties"]
        .as_object()
        .expect("the rules are declared one by one, which is what completes their names")
        .keys()
        .cloned()
        .collect();
    let mut expected: Vec<String> = diagnostics::known_names().map(str::to_owned).collect();

    declared.sort();
    expected.sort();
    assert_eq!(declared, expected);
}

#[test]
fn every_severity_the_manifest_offers_is_one_the_server_can_read() {
    // The values are the same hazard from the other end. `deny_unknown_fields` does not degrade:
    // a severity the manifest offers and `Severity` cannot parse rejects the *entire* settings
    // layer, so every other setting silently stops working over one word in a drop-down.
    let manifest = manifest();
    let rules = properties(&manifest)["ya-lsp.diagnostics.rules"].clone();
    let severities = rules["additionalProperties"]["enum"]
        .as_array()
        .expect("an eleventh rule from rubydex still has to validate")
        .clone();

    // Five, because `Severity` has five variants. A sixth is a deliberate change to both files.
    assert_eq!(severities.len(), 5);
    for severity in &severities {
        serde_json::from_value::<Severity>(severity.clone()).unwrap_or_else(|_| {
            panic!("the manifest offers {severity}, which the server cannot read")
        });
    }

    for (name, rule) in rules["properties"].as_object().expect("the declared rules") {
        assert_eq!(
            rule["enum"].as_array(),
            Some(&severities),
            "{name} offers a different set of severities from the open case"
        );
    }
}

#[test]
fn every_setting_the_server_reads_is_one_the_editor_can_set() {
    // The defect with no natural test: five settings existed in `ya-lsp.toml` and nowhere in the
    // editor, so a VS Code user who needed one had to discover a file the settings UI never
    // mentions. What can be enforced is not "expose everything" — that is a judgement each time —
    // but that a new server setting is either exposed or deliberately left out *here*, rather than
    // forgotten the way these five were across three releases.
    let manifest = manifest();
    let declared = properties(&manifest);

    // The guard: if serde ever stops naming the fields it would have accepted, this test would
    // otherwise pass by finding nothing to check.
    assert!(fields_of("index").iter().any(|field| field == "load_paths"));

    // `gems.max_files` is the deliberate omission. It is a ceiling on gem files nobody has needed
    // to tune, `index.max_files` is the one that actually fires, and every exposed setting is a
    // branch in `config.ts` and a test forever.
    let file_only = ["gems.max_files"];

    let mut missing = Vec::new();
    for table in ["index", "gems", "rbs", "diagnostics"] {
        for field in fields_of(table) {
            let key = format!("{table}.{field}");
            if file_only.contains(&key.as_str()) {
                continue;
            }
            if !declared.contains_key(&format!("ya-lsp.{}", camel(&key))) {
                missing.push(key);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{missing:?} can be set in ya-lsp.toml and nowhere in the editor"
    );
}

/// Every field one of `PartialConfig`'s tables accepts, read out of serde's own complaint.
///
/// There is no other way to enumerate them — `PartialConfig` is deserialized and never
/// serialized — and a list written out here would be the third copy of the same thing, which is
/// the copy that goes stale the moment somebody adds a field. `deny_unknown_fields` already names
/// every field it would have taken; only the prose around the names varies with how many there
/// are ("one of `a`, `b`" against "`a` or `b`"), so the names are taken and the prose is not.
fn fields_of(table: &str) -> Vec<String> {
    let text = format!("[{table}]\nthis-is-not-a-field = 1\n");
    let error = toml::from_str::<PartialConfig>(&text)
        .expect_err("`deny_unknown_fields` has to reject it")
        .to_string();
    let names: Vec<String> = error
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    // The first backticked name is the unknown field itself; the rest are what it could have been.
    assert_eq!(
        names.first().map(String::as_str),
        Some("this-is-not-a-field")
    );
    names[1..].to_vec()
}

/// `load_paths` in the file is `loadPaths` in the editor. The two conventions are why every
/// description names its twin, and this is the only other place that maps them.
fn camel(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut upper = false;
    for character in snake.chars() {
        match character {
            '_' => upper = true,
            _ if upper => {
                out.extend(character.to_uppercase());
                upper = false;
            }
            _ => out.push(character),
        }
    }
    out
}
