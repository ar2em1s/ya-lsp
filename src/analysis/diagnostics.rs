//! rubydex diagnostics → LSP diagnostics.
//!
//! # These are indexer diagnostics, not linter diagnostics
//!
//! Only `parse-error` and `parse-warning` are statements about the user's code. The rest are
//! rubydex saying *it* gave up: `class Foo < base` is perfectly good Ruby but not statically
//! resolvable, so rubydex records `dynamic-ancestor` and moves on. Squiggling that by default would
//! put permanent warnings on correct code (the fastest way to get a language server uninstalled),
//! so those rules ship `Off` and are opt-in through `[diagnostics.rules]`. They stay reportable
//! because they are the only signal explaining *why* navigation fails at a given spot.
//!
//! Two observations set the table:
//! - `dynamic-ancestor` alone is the overwhelming majority of what a real Rails workspace would
//!   report, every one on working code;
//! - `undefined-method-visibility-target` fires on `private_class_method :new`, standard Ruby that
//!   rubydex flags only because it does not model the implicit `Class#new`.
//!
//! A check with no known true positives does not earn a squiggle, so both resolution rules ship
//! off.

use lsp_types::DiagnosticSeverity;
use rubydex::diagnostic::Rule;

use crate::workspace::Severity;

/// Reported as the `source` of every diagnostic, so users can tell ours from RuboCop's.
pub const SOURCE: &str = "ya-lsp";

/// The name a rule is reported under, and the severity ya-lsp gives it when the user has not
/// configured one.
///
/// The match is exhaustive on purpose: rubydex is pre-1.0, and a new rule upstream must break this
/// build, not quietly inherit some fallback severity. Keep [`ALL`] in step;
/// `names_match_rubydexs_own_spelling` checks the strings.
fn describe(rule: Rule) -> (&'static str, Severity) {
    match rule {
        // The file does not parse. Unambiguously about the user's code.
        Rule::ParseError => ("parse-error", Severity::Error),
        // Prism's own warnings, e.g. "assigned but unused variable".
        Rule::ParseWarning => ("parse-warning", Severity::Warning),

        // Indexer limitations. All four fire on legal, working Ruby.
        Rule::DynamicConstantReference => ("dynamic-constant-reference", Severity::Off),
        Rule::DynamicSingletonDefinition => ("dynamic-singleton-definition", Severity::Off),
        Rule::DynamicAncestor => ("dynamic-ancestor", Severity::Off),
        Rule::TopLevelMixinSelf => ("top-level-mixin-self", Severity::Off),

        // Mixed buckets: mostly genuine misuse ("`private` does not accept `attr_*` arguments",
        // "`module_function` can only be used in modules"), but the same rule also covers "called
        // with a non-literal argument", which is rubydex giving up, not a defect. `Hint` reports
        // them without claiming the code is wrong.
        // rubydex calls this variant `InvalidConstantVisibility`; ya-lsp deliberately keeps the
        // name `invalid-private-constant`. The string is a key a user writes in `ya-lsp.toml` and a
        // `code` a client shows, so it is ya-lsp's to keep, like the severity beside it: a rename
        // would be a config break bought with nothing.
        Rule::InvalidConstantVisibility => ("invalid-private-constant", Severity::Hint),
        Rule::InvalidMethodVisibility => ("invalid-method-visibility", Severity::Hint),

        // Genuine bugs in principle (`private :typo` where `typo` does not exist), but that needs a
        // complete graph, which this is not. In practice the method variant fires on
        // `private_class_method :new`, correct Ruby, and the constant variant fires the same way on
        // a class whose superclass could not be resolved. Measure before turning either on.
        Rule::UndefinedMethodVisibilityTarget => {
            ("undefined-method-visibility-target", Severity::Off)
        }
        Rule::UndefinedConstantVisibilityTarget => {
            ("undefined-constant-visibility-target", Severity::Off)
        }
    }
}

/// Every rule rubydex can report. Used to reject typos in `[diagnostics.rules]`, where a
/// misspelling would otherwise be silently ignored forever.
const ALL: [Rule; 10] = [
    Rule::ParseError,
    Rule::ParseWarning,
    Rule::DynamicConstantReference,
    Rule::DynamicSingletonDefinition,
    Rule::DynamicAncestor,
    Rule::TopLevelMixinSelf,
    Rule::InvalidConstantVisibility,
    Rule::InvalidMethodVisibility,
    Rule::UndefinedMethodVisibilityTarget,
    Rule::UndefinedConstantVisibilityTarget,
];

/// The rule's name as spelled in `[diagnostics.rules]`, and as sent in `Diagnostic::code`.
///
/// Not `Rule::to_string`: that allocates, and this runs once per diagnostic per publish.
#[must_use]
pub fn name(rule: Rule) -> &'static str {
    describe(rule).0
}

/// What this rule reports as when the user has not configured it.
#[must_use]
pub fn default_severity(rule: Rule) -> Severity {
    describe(rule).1
}

/// Every configurable rule name, in the order they are documented.
pub fn known_names() -> impl Iterator<Item = &'static str> {
    ALL.into_iter().map(name)
}

#[must_use]
pub fn is_known_name(candidate: &str) -> bool {
    ALL.into_iter().any(|rule| name(rule) == candidate)
}

/// `None` means the rule is off and the diagnostic must be dropped entirely: LSP has no severity
/// that renders as "invisible".
#[must_use]
pub fn to_lsp_severity(severity: Severity) -> Option<DiagnosticSeverity> {
    match severity {
        Severity::Off => None,
        Severity::Error => Some(DiagnosticSeverity::ERROR),
        Severity::Warning => Some(DiagnosticSeverity::WARNING),
        Severity::Information => Some(DiagnosticSeverity::INFORMATION),
        Severity::Hint => Some(DiagnosticSeverity::HINT),
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    #[test]
    fn names_are_the_variants_own_spelling_but_one() {
        // rubydex's `Display` prints the variant verbatim (`ParseError`), so there is no shared
        // spelling to pin to: these names are entirely ya-lsp's, a key a user writes in
        // `ya-lsp.toml` and a `code` a client shows. What is worth asserting is that none drifted
        // by accident, so the rule is the variant hyphenated, with exactly one exception, kept
        // here, not in a comment, because an exception nothing enforces goes stale unnoticed.
        for rule in ALL {
            let hyphenated: String = rule
                .to_string()
                .char_indices()
                .flat_map(|(at, character)| {
                    let dash = (at > 0 && character.is_uppercase()).then_some('-');
                    dash.into_iter().chain(character.to_lowercase())
                })
                .collect();
            // The one exception: rubydex's `InvalidConstantVisibility` is ya-lsp's
            // `invalid-private-constant`; `describe` says why.
            let expected = match rule {
                Rule::InvalidConstantVisibility => "invalid-private-constant".to_owned(),
                _ => hyphenated,
            };
            assert_eq!(name(rule), expected, "rule name drifted");
        }
    }

    #[test]
    fn all_rules_are_listed() {
        // A tripwire for `describe` gaining an arm without `ALL` gaining an entry.
        assert_eq!(ALL.len(), 10);
        let mut names: Vec<&str> = known_names().collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len(), "duplicate rule name");
    }

    #[test]
    fn parse_failures_are_the_only_rules_that_shout_by_default() {
        // Guards the product decision above: nothing that fires on correct Ruby may default to a
        // warning or an error.
        for rule in ALL {
            let severity = default_severity(rule);
            let loud = matches!(severity, Severity::Error | Severity::Warning);
            let parse = matches!(rule, Rule::ParseError | Rule::ParseWarning);
            assert_eq!(loud, parse, "{} defaults to {severity:?}", name(rule));
        }
    }

    #[test]
    fn every_severity_maps_to_the_one_the_client_renders() {
        // `Off` is the load-bearing one: it has no LSP spelling, so the diagnostic must be dropped,
        // not downgraded to a hint nobody asked for. The other four are a straight table, and a
        // table is exactly what gets a line transposed: an `Information` rendered as an `Error`
        // puts a red squiggle on working code.
        assert_eq!(to_lsp_severity(Severity::Off), None);
        for (configured, rendered) in [
            (Severity::Error, DiagnosticSeverity::ERROR),
            (Severity::Warning, DiagnosticSeverity::WARNING),
            (Severity::Information, DiagnosticSeverity::INFORMATION),
            (Severity::Hint, DiagnosticSeverity::HINT),
        ] {
            assert_eq!(
                to_lsp_severity(configured),
                Some(rendered),
                "{configured:?}"
            );
        }
    }

    #[test]
    fn parse_errors_read_the_way_prism_wrote_them() {
        // ya-lsp owns the severity and the `code` of a diagnostic, and **not one word of the
        // text**: `diagnostic.message()` is forwarded verbatim. That is the decision, and the right
        // one: rewriting a parser's diagnostics is a real cost and a real risk of saying something
        // false about code the rewriter did not parse.
        //
        // So the actual sentences are pinned here, not just "the message is non-empty" (the
        // mechanism tested, the content not). If Prism rewrites one, this fails, and someone reads
        // the new wording and decides whether users are better off: the point of a pass-through
        // being deliberate.
        let mut harness = Harness::new();
        let uri = harness.write("lib/broken.rb", UNTERMINATED);
        harness.index();

        let items = harness.latest(&uri).expect("diagnostics");
        let said: Vec<(Option<String>, &str)> = items
            .iter()
            .map(|item| {
                (
                    match &item.code {
                        Some(lsp_types::NumberOrString::String(name)) => Some(name.clone()),
                        _ => None,
                    },
                    item.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            said,
            vec![
                (
                    Some("parse-error".to_owned()),
                    "expected an `end` to close the `class` statement",
                ),
                (
                    Some("parse-error".to_owned()),
                    "expected an `end` to close the `def` statement",
                ),
                (
                    Some("parse-warning".to_owned()),
                    "mismatched indentations at '\n' with 'def' at 2",
                ),
                (
                    Some("parse-error".to_owned()),
                    "unexpected end-of-input, assuming it is closing the parent top level \
                     context",
                ),
            ],
            "{items:?}"
        );
        // Two of these are worth reading twice. The indentation warning names the character it
        // mismatched against, and that character is a newline, so the user sees a message with a
        // line break in the middle. The last says "assuming it is closing the parent top level
        // context": Prism explaining its own error recovery to someone who did not ask. Neither is
        // ya-lsp's to fix, but writing them down is how anyone notices.
    }

    #[test]
    fn rules_that_fire_on_correct_ruby_are_off_until_asked_for() {
        // `class Child < base` is legal Ruby that rubydex cannot resolve statically. Squiggling it
        // by default would put a permanent warning on working code.
        let source = "base = Object\nclass Child < base\nend\n";
        let mut harness = Harness::new();
        let uri = harness.write("lib/dynamic.rb", source);
        harness.index();

        assert!(
            harness.latest(&uri).is_none_or(|items| items
                .iter()
                .all(|item| item.code != code("dynamic-ancestor"))),
            "dynamic-ancestor must be silent by default"
        );

        // ... but turning it on in config must actually work.
        std::fs::write(
            harness
                .root
                .path()
                .join(crate::workspace::config::CONFIG_FILE_NAME),
            "[diagnostics.rules]\ndynamic-ancestor = \"warning\"\n",
        )
        .unwrap();
        harness.run(Task::ReloadConfig);

        let items = harness.latest(&uri).expect("now reported");
        let dynamic: Vec<_> = items
            .iter()
            .filter(|item| item.code == code("dynamic-ancestor"))
            .collect();
        assert!(!dynamic.is_empty(), "{items:?}");
        assert!(
            dynamic
                .iter()
                .all(|item| item.severity == Some(DiagnosticSeverity::WARNING))
        );
    }
}
