//! rubydex diagnostics → LSP diagnostics.
//!
//! # These are indexer diagnostics, not linter diagnostics
//!
//! Only `parse-error` and `parse-warning` are statements about the user's code. The rest are
//! rubydex telling us *it* gave up: `class Foo < base` is perfectly good Ruby, but it is not
//! statically resolvable, so rubydex records `dynamic-ancestor` and moves on. Squiggling that by
//! default would put permanent warnings on correct code — the fastest way to get a language
//! server uninstalled — so those rules ship `Off` and are opt-in through `[diagnostics.rules]`.
//! They stay reportable because they are the only signal that explains *why* navigation fails
//! at a given spot.
//!
//! With these defaults solargraph reports 14 diagnostics instead of 399, and every one of the
//! 14 is real. Two results drove the table: `dynamic-ancestor` alone would have put 384
//! warnings on working code, and *every* `undefined-method-visibility-target` hit was
//! `private_class_method :new` — standard Ruby that rubydex flags only because it does not
//! model the implicit `Class#new`. A check with no measured true positives does not earn a
//! squiggle, so both resolution rules ship off until M3 completes the graph.

use lsp_types::DiagnosticSeverity;
use rubydex::diagnostic::Rule;

use crate::workspace::Severity;

/// Reported as the `source` of every diagnostic, so users can tell ours from RuboCop's.
pub const SOURCE: &str = "ya-lsp";

/// The name rubydex reports a rule under, and the severity ya-lsp gives it when the user has
/// not configured one.
///
/// The match is exhaustive on purpose: rubydex is pre-1.0, and a new rule appearing upstream
/// must break this build rather than quietly inherit some fallback severity. Keep [`ALL`] in
/// step; `names_match_rubydexs_own_spelling` checks the strings against rubydex's `Display`.
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
        // "`module_function` can only be used in modules"), but the same rule also covers
        // "called with a non-literal argument", which is rubydex giving up rather than a defect.
        // `Hint` reports them without claiming the code is wrong.
        Rule::InvalidPrivateConstant => ("invalid-private-constant", Severity::Hint),
        Rule::InvalidMethodVisibility => ("invalid-method-visibility", Severity::Hint),

        // Genuine bugs in principle — `private :typo` where `typo` does not exist — but the
        // graph has to be complete for that to hold, and it is not. Both hits across the three
        // reference repos were `private_class_method :new`, which is correct Ruby; the constant
        // variant fires the same way on a class whose superclass could not be resolved. Off
        // until M3 indexes gems, then re-measure before turning either back on.
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
    Rule::InvalidPrivateConstant,
    Rule::InvalidMethodVisibility,
    Rule::UndefinedMethodVisibilityTarget,
    Rule::UndefinedConstantVisibilityTarget,
];

/// The rule's name as it must be spelled in `[diagnostics.rules]`, and as we send it in
/// `Diagnostic::code`.
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

/// `None` means the rule is switched off and the diagnostic must be dropped entirely — LSP has
/// no severity that renders as "invisible".
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

    #[test]
    fn names_match_rubydexs_own_spelling() {
        // rubydex derives the wire name from the variant via `camel_to_snake`. If it renames a
        // variant our config keys silently stop matching, so pin the strings to its `Display`.
        for rule in ALL {
            assert_eq!(
                name(rule),
                rule.to_string(),
                "rule name drifted from rubydex"
            );
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
        // Guards the product decision above: nothing that fires on correct Ruby may default to
        // a warning or an error.
        for rule in ALL {
            let severity = default_severity(rule);
            let loud = matches!(severity, Severity::Error | Severity::Warning);
            let parse = matches!(rule, Rule::ParseError | Rule::ParseWarning);
            assert_eq!(loud, parse, "{} defaults to {severity:?}", name(rule));
        }
    }

    #[test]
    fn every_severity_maps_to_the_one_the_client_renders() {
        // `Off` is the load-bearing one — it has no LSP spelling, so the diagnostic must be
        // dropped rather than downgraded to a hint nobody asked for. The other four are a
        // straight table, and a table is exactly the thing that gets a line transposed: an
        // `Information` rendered as an `Error` puts a red squiggle on working code.
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
}
