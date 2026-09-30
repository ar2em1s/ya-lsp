//! The VS Code manifest, checked against the server it configures.
//!
//! `editors/vscode/package.json` is where a user reads what a setting does and what it defaults to,
//! and neither language can see across that boundary. Two hazards:
//! - **Defaults.** A documented default that drifts from `Config::default()` is wrong documentation
//!   with no failing test behind it.
//! - **Rule names.** The keys of `[diagnostics.rules]` must be written in the manifest for the
//!   editor to complete them, and a second copy of a list goes stale.
//!
//! `editors/vscode/src/manifest.test.ts` holds the halves this cannot see: the scopes, and the
//! agreement between the manifest and the settings `config.ts` actually reads.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use ya_lsp::{
    DEFAULT_LOG_FILTER,
    analysis::diagnostics,
    analysis::{MIGRATION_PAIR, TEST_TREES},
    workspace::Config,
    workspace::Severity,
    workspace::config::{PartialConfig, Switch, Word},
    workspace::i18n::DEFAULT_LOCALE_PATHS,
};

fn manifest() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/editors/vscode/package.json");
    let text = std::fs::read_to_string(path).expect("the extension manifest");
    serde_json::from_str(&text).expect("the extension manifest is valid JSON")
}

/// Every `ya-lsp.*` property the manifest declares, flattened out of its categories.
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

/// Every language the server claims in a registration is one the extension wakes up for.
///
/// The server names these because it registers document selectors of its own, over roots the client
/// cannot know about (a gem's source, Ruby's library), and a filter naming a language the extension
/// never activates for claims nothing. Two lists in two languages, and the failure is silent: a
/// Rails engine's templates would never answer, with nothing logged.
#[test]
fn every_language_the_server_claims_is_one_the_extension_activates_for() {
    let manifest = manifest();
    let events: BTreeSet<&str> = manifest["activationEvents"]
        .as_array()
        .expect("the manifest declares its activation events")
        .iter()
        .filter_map(Value::as_str)
        .collect();

    for language in ya_lsp::server::capabilities::LANGUAGE_IDS {
        assert!(
            events.contains(format!("onLanguage:{language}").as_str()),
            "the server claims {language} and the extension never wakes up for it"
        );
    }
    assert_eq!(
        events.len(),
        ya_lsp::server::capabilities::LANGUAGE_IDS.len(),
        "an activation event for a language no registration claims is a server started for nothing"
    );
}

#[test]
fn every_documented_default_is_the_one_the_server_actually_uses() {
    let manifest = manifest();
    let properties = properties(&manifest);
    let config = Config::default();

    // `"auto"` is written out below because `Switch` cannot be serialized; this holds the word to
    // the server's own default, so the two cannot drift apart silently.
    assert_eq!(config.rails.enabled, Switch::Word(Word::Auto));
    assert_eq!(config.rspec.enabled, Switch::Word(Word::Auto));
    assert_eq!(config.i18n.enabled, Switch::Word(Word::Auto));
    assert_eq!(config.i18n.paths, None);

    // An empty map is what `{}` in the manifest documents, and the only default here that cannot be
    // written as a `json!` of the field itself: `Severity` is deserialized, never serialized,
    // because the server never sends one back.
    assert!(config.diagnostics.rules.is_empty());

    // Left: what the settings UI tells the user. Right: what the server does when nobody set
    // anything. Settings are read with `inspect`, so the left half is never sent: it is
    // documentation only, and documentation of a default rots unnoticed.
    let expected: Vec<(&str, Value)> = vec![
        ("ya-lsp.logLevel", json!(DEFAULT_LOG_FILTER)),
        ("ya-lsp.log.file", json!(config.log.file)),
        ("ya-lsp.log.filePath", json!(config.log.file_path)),
        ("ya-lsp.log.fileLevel", json!(config.log.file_level)),
        ("ya-lsp.gems.enabled", json!(config.gems.enabled)),
        ("ya-lsp.gems.defaultGems", json!(config.gems.default_gems)),
        // `None` is spelled `""`. "Detect it" and "find one yourself" have no value to show, and a
        // setting with no default at all reads as one nobody thought about.
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
            "ya-lsp.types.guessFromNames",
            json!(config.types.guess_from_names),
        ),
        ("ya-lsp.types.structs", json!(config.types.structs)),
        ("ya-lsp.types.annotations", json!(config.types.annotations)),
        ("ya-lsp.types.factories", json!(config.types.factories)),
        // `Switch` is deserialized and never serialized (the server never sends one back), so the
        // word is written out here, as `diagnostics.rules`' empty map is. The line below, not this
        // one, holds it to the server.
        ("ya-lsp.rails.enabled", json!("auto")),
        ("ya-lsp.rails.schema", json!(config.rails.schema)),
        ("ya-lsp.rails.models", json!(config.rails.models)),
        ("ya-lsp.rails.routes", json!(config.rails.routes)),
        ("ya-lsp.rails.entrypoints", json!(config.rails.entrypoints)),
        ("ya-lsp.rails.views", json!(config.rails.views)),
        ("ya-lsp.rspec.enabled", json!("auto")),
        ("ya-lsp.i18n.enabled", json!("auto")),
        ("ya-lsp.i18n.locale", json!(config.i18n.locale)),
        // The built-in list, read from the code, for `trees.test`'s reason: a project replaces it.
        ("ya-lsp.i18n.paths", json!(DEFAULT_LOCALE_PATHS)),
        // The two lists the manifest shows are the built-in ones, read from the code, not written
        // out: a project *replaces* them, so what the settings UI shows someone about to do that
        // must be what they are actually replacing. `Config::default()` holds `None` for both: the
        // same rule, said as an absence.
        ("ya-lsp.trees.test", json!(TEST_TREES)),
        ("ya-lsp.trees.migration", json!([MIGRATION_PAIR])),
        // Additive, so its default really is empty. The built-in name is documented in the
        // description, because a list showing it would read as replaceable.
        ("ya-lsp.trees.testSupport", json!(config.trees.test_support)),
        (
            "ya-lsp.hints.blockParameters",
            json!(config.hints.block_parameters),
        ),
        ("ya-lsp.hints.locals", json!(config.hints.locals)),
        ("ya-lsp.hints.returns", json!(config.hints.returns)),
        (
            "ya-lsp.diagnostics.enabled",
            json!(config.diagnostics.enabled),
        ),
        ("ya-lsp.diagnostics.rules", json!({})),
        ("ya-lsp.index.include", json!(config.index.include)),
        ("ya-lsp.index.exclude", json!(config.index.exclude)),
        ("ya-lsp.index.loadPaths", json!(config.index.load_paths)),
        ("ya-lsp.index.maxFiles", json!(config.index.max_files)),
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

    // The other direction: nothing may be added to the manifest without landing in the table above.
    // The three exceptions are named, not matched by pattern, because each is the extension's own
    // business with no server-side default to drift from:
    // - where the binary lives;
    // - whether the client traces its own traffic;
    // - whether the client offers RuboCop's extension to a project that lints with RuboCop.
    //
    // That last one must never reach the server: `initializationOptions` is deserialized with
    // `deny_unknown_fields`, so a client-only key leaking into the layer would reject every setting
    // in it, not just itself.
    let checked: BTreeSet<&str> = expected.iter().map(|(setting, _)| *setting).collect();
    let unchecked: Vec<&str> = properties
        .keys()
        .map(String::as_str)
        .filter(|setting| !checked.contains(setting))
        .collect();
    assert_eq!(
        unchecked,
        [
            "ya-lsp.rubocop.hint",
            "ya-lsp.serverPath",
            "ya-lsp.trace.server"
        ]
    );
}

#[test]
fn the_rule_map_documents_exactly_the_rules_that_exist() {
    // `[diagnostics.rules]` is the one setting whose *keys* an editor cannot guess: an open object
    // completes its values, never its names, so nobody finds `parse-warning` without being told.
    // Declaring the ten fixes that and makes the list a second copy. `describe`'s match already
    // breaks the build when rubydex adds an eleventh rule; this makes it break the extension too,
    // instead of shipping a list one short.
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
    // The values are the same hazard from the other end. `deny_unknown_fields` does not degrade: a
    // severity the manifest offers that `Severity` cannot parse rejects the *entire* settings
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
    // The defect with no natural test: a setting that exists in `ya-lsp.toml` and nowhere in the
    // editor, so a VS Code user who needs it must discover a file the settings UI never mentions.
    // "Expose everything" is a judgement each time and cannot be enforced; what can be is that a
    // new server setting is either exposed or deliberately left out *here*, not forgotten.
    let manifest = manifest();
    let declared = properties(&manifest);

    // The guard: if serde ever stops naming the fields it would have accepted, this test would
    // otherwise pass by finding nothing to check.
    assert!(fields_of("index").iter().any(|field| field == "load_paths"));

    // `gems.max_files` is the deliberate omission: a ceiling on gem files nobody has needed to
    // tune. `index.max_files` is the one that actually fires, and every exposed setting is a branch
    // in `config.ts` and a test forever.
    // `log.level` is the one key whose editor spelling differs from its TOML spelling.
    // `ya-lsp.logLevel` predates the file setting and the extension is on the Marketplace, so the
    // key stays put instead of costing a deprecation and a window in which two keys can disagree.
    let renamed = [("log.level", "ya-lsp.logLevel")];
    let file_only = ["gems.max_files"];

    let mut missing = Vec::new();
    for table in [
        "index",
        "log",
        "gems",
        "rbs",
        "types",
        "hints",
        "diagnostics",
    ] {
        for field in fields_of(table) {
            let key = format!("{table}.{field}");
            if file_only.contains(&key.as_str()) {
                continue;
            }
            let setting = renamed.iter().find(|(field, _)| *field == key).map_or_else(
                || format!("ya-lsp.{}", camel(&key)),
                |(_, name)| (*name).to_owned(),
            );
            if !declared.contains_key(&setting) {
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
/// There is no other way to enumerate them (`PartialConfig` is deserialized, never serialized), and
/// a list written out here would be a third copy, the one that goes stale the moment somebody adds
/// a field. `deny_unknown_fields` already names every field it would have taken; only the prose
/// around the names varies with their count ("one of `a`, `b`" against "`a` or `b`"), so the names
/// are taken and the prose is not.
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

/// The extensions the editor calls a template are the extensions the server blanks.
///
/// The other side of `every_documented_default_is_the_one_the_server_actually_uses`, with a sharper
/// failure:
/// - a file the manifest claims and `erb::is_template` does not is indexed as Ruby, so all its
///   markup reaches rubydex as code, and its call sites are replaced by parse errors;
/// - an extension the server blanks and the editor never associates is a template that opens as
///   plain text and starts no server at all.
///
/// Neither is visible from either language alone.
#[test]
fn the_editor_and_the_server_agree_on_what_an_erb_template_is() {
    use ya_lsp::analysis::erb;

    let manifest = manifest();
    let languages = manifest["contributes"]["languages"]
        .as_array()
        .expect("the manifest contributes languages");
    let template = languages
        .iter()
        .find(|language| language["id"] == json!("erb"))
        .expect("an `erb` language, or `.erb` files open as plain text and nothing activates");

    let extensions: Vec<&str> = template["extensions"]
        .as_array()
        .expect("the language claims file extensions")
        .iter()
        .map(|value| value.as_str().expect("an extension is a string"))
        .collect();
    assert!(!extensions.is_empty());

    for extension in &extensions {
        let name = format!("index.html{extension}");
        assert!(
            erb::is_template(std::path::Path::new(&name)),
            "the manifest claims {extension} and the server would index it as Ruby"
        );
    }

    // And the guard, so the loop above cannot pass by accepting everything.
    assert!(!erb::is_template(std::path::Path::new("story.rb")));

    // Activation is what turns the contribution into a running server: without it, opening a
    // template in a folder whose Ruby nobody has touched starts nothing.
    let events = manifest["activationEvents"]
        .as_array()
        .expect("activation events");
    assert!(events.contains(&json!("onLanguage:erb")), "{events:?}");
}
