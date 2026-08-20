//! How ya-lsp spells Ruby constructs for humans.
//!
//! rubydex's names are built for lookup, not for reading: a singleton method is
//! `Person::<Person>#build()`, and every method carries empty parentheses whether or not it
//! takes arguments. Editors show these strings directly, so they get translated back into
//! something a Ruby developer would have written.
//!
//! Everything here is pure formatting, shared by `hover` and `documentSymbol` so the two can
//! never disagree about what a construct is called.

use rubydex::model::{
    comment::Comment,
    definitions::{Parameter, Signatures},
    graph::Graph,
};

/// Turn a rubydex declaration name into Ruby.
///
/// `Person::<Person>#build()` is how rubydex says `Person.build`, because it models singleton
/// methods as members of a synthetic singleton class. Instance methods keep the `#` spelling,
/// which is what Ruby documentation has always used.
#[must_use]
pub fn qualified_name(name: &str) -> String {
    let Some((owner, method)) = name.rsplit_once('#') else {
        return name.to_owned();
    };
    let method = method.strip_suffix("()").unwrap_or(method);

    match singleton_parts(owner) {
        // The path, not the last segment: an instance method of `Foo::Bar` is spelled
        // `Foo::Bar#baz`, so its singleton method has to be `Foo::Bar.baz` and not `Bar.baz`.
        // Only a top-level class has no path, and there `singleton` is the whole name.
        Some((prefix, singleton)) => {
            let owner = if prefix.is_empty() { singleton } else { prefix };
            format!("{owner}.{method}")
        }
        None => format!("{owner}#{method}"),
    }
}

/// Split a declaration name into the pair a symbol list shows: the label, and the container
/// printed beside it.
///
/// The label matches what the outline calls the same construct, so a symbol reads identically
/// whether it was found in one file or across the project. The container is the *full* path —
/// `self.baz` alone is ambiguous, and the picker has a column for exactly this.
#[must_use]
pub fn split_qualified(name: &str) -> (String, Option<String>) {
    if let Some((owner, method)) = name.rsplit_once('#') {
        let method = simple_name(method);
        return match singleton_parts(owner) {
            // A singleton method: `class << self` is an implementation detail, so it is spelled
            // the way it was written, and the container is the class it hangs off.
            Some((prefix, singleton)) => (
                format!("self.{method}"),
                Some(if prefix.is_empty() { singleton } else { prefix }.to_owned()),
            ),
            None => (method.to_owned(), non_empty(owner)),
        };
    }
    match name.rsplit_once("::") {
        Some((owner, simple)) => (simple.to_owned(), non_empty(owner)),
        None => (name.to_owned(), None),
    }
}

/// `Foo::Bar::<Bar>` -> `("Foo::Bar", "Bar")`, and a top-level `<Foo>` -> `("", "Foo")`.
///
/// `None` when the name is not a singleton class, which is every name rubydex spells without
/// angle brackets — they are its only use for them.
fn singleton_parts(owner: &str) -> Option<(&str, &str)> {
    let rest = owner.strip_suffix('>')?;
    let (prefix, singleton) = rest.rsplit_once('<')?;
    let prefix = prefix.strip_suffix("::").unwrap_or(prefix);
    // `Foo::<Bar>` is not `Bar`'s singleton class written the long way round; refusing it keeps
    // an unexpected shape from being rendered as something it is not.
    (prefix.is_empty() || prefix.ends_with(singleton)).then_some((prefix, singleton))
}

fn non_empty(name: &str) -> Option<String> {
    (!name.is_empty()).then(|| name.to_owned())
}

/// The simple name of a definition, without rubydex's method parentheses.
#[must_use]
pub fn simple_name(raw: &str) -> &str {
    raw.strip_suffix("()").unwrap_or(raw)
}

/// Whether a declaration has a name a person could have written.
///
/// Angle brackets are rubydex's only punctuation for names it invented, and it invents two
/// kinds: `Foo::<Foo>` for a singleton class, and `<uri>:<offset><anonymous>` for a `Class.new`
/// with nothing to call it. Neither can be typed, so neither belongs in a list of things to type
/// — measured, a workspace of 17,557 files answered `::` with a page of
/// `10042574982090812855:14001<anonymous>`.
#[must_use]
pub fn is_nameable(name: &str) -> bool {
    !last_segment(name).contains('<')
}

/// The last segment of a declaration name: the part a person types.
///
/// `Foo::Bar#baz()` -> `baz`, `Foo::Bar` -> `Bar`, `Parent#@var` -> `@var`. Shared by the symbol
/// picker, which matches against it, and by completion, which shows it — a name that is searched
/// for one way and inserted another is a bug waiting to happen.
#[must_use]
pub fn last_segment(name: &str) -> &str {
    let tail = match name.rsplit_once('#') {
        Some((_, member)) => member,
        None => name.rsplit_once("::").map_or(name, |(_, simple)| simple),
    };
    simple_name(tail)
}

/// A method's parameter list, rendered as Ruby: `(volume = ..., *rest, sep:, **opts, &blk)`.
///
/// Empty for a method that takes nothing, so `Person#shout` reads the way it is called.
/// Default *values* are not available — rubydex records that a parameter is optional, not what
/// it falls back to — so `= ...` stands in for the expression.
#[must_use]
pub fn parameter_list(graph: &Graph, signatures: &Signatures) -> String {
    // Ruby has exactly one signature per method; overloads only come from RBS.
    let Some(signature) = signatures.as_slice().first() else {
        return String::new();
    };
    if signature.is_empty() {
        return String::new();
    }

    let rendered: Vec<String> = signature
        .iter()
        .map(|parameter| {
            let name = graph
                .strings()
                .get(parameter.inner().str())
                .map_or_else(String::new, |string| string.as_str().to_owned());
            match parameter {
                Parameter::RequiredPositional(_) | Parameter::Post(_) => name,
                Parameter::OptionalPositional(_) => format!("{name} = ..."),
                Parameter::RestPositional(_) => format!("*{name}"),
                Parameter::RequiredKeyword(_) => format!("{name}:"),
                Parameter::OptionalKeyword(_) => format!("{name}: ..."),
                Parameter::RestKeyword(_) => format!("**{name}"),
                Parameter::Block(_) => format!("&{name}"),
                Parameter::Forward(_) => "...".to_owned(),
            }
        })
        .collect();

    format!("({})", rendered.join(", "))
}

/// The documentation comment above a definition, as markdown.
///
/// `None` when there is nothing left after the magic comments are dropped.
#[must_use]
pub fn documentation(comments: &[Comment]) -> Option<String> {
    let mut lines: Vec<&str> = comments
        .iter()
        .map(|comment| strip_marker(comment.string()))
        .collect();

    // rubydex attaches whatever comment block sits above a definition, and it allows one blank
    // line in between — which means `# frozen_string_literal: true` at the top of a file
    // becomes the documentation for that file's first class. Directives only ever lead, so
    // dropping them from the front leaves prose (and YARD tags) untouched.
    let leading = lines.iter().take_while(|line| is_directive(line)).count();
    lines.drain(..leading);

    let call_seq = take_rdoc_header(&mut lines);

    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }

    if lines.is_empty() && call_seq.is_empty() {
        return None;
    }
    if call_seq.is_empty() {
        return Some(lines.join("\n"));
    }
    Some(format!(
        "```ruby\n{}\n```\n\n{}",
        call_seq.join("\n"),
        lines.join("\n")
    ))
}

/// Take RDoc's header off the front of an RBS comment, keeping the call-seq lines.
///
/// Ruby's core signatures carry the documentation RDoc extracted from the C source, and it
/// arrives wrapped:
///
/// ```text
/// <!--
///   rdoc-file=string.c
///   - upcase(mapping = :ascii) -> new_string
/// -->
/// Returns a new string containing the upcased characters in `self`:
/// ```
///
/// Rendered as markdown that whole block disappears, HTML comments being invisible — taking the
/// call-seq with it. For a method implemented in C the call-seq is the only place the block
/// forms are written down at all (`each {|element| ... } -> self`), and it says more than the
/// RBS signature does, so it is lifted out as code and the rest of the wrapper is dropped.
fn take_rdoc_header<'a>(lines: &mut Vec<&'a str>) -> Vec<&'a str> {
    if lines.first().is_none_or(|line| line.trim() != "<!--") {
        return Vec::new();
    }
    let Some(end) = lines.iter().position(|line| line.trim() == "-->") else {
        // An opener with no closer is not RDoc's header; leave the comment exactly as it was.
        return Vec::new();
    };

    let call_seq: Vec<&str> = lines[1..end]
        .iter()
        .filter_map(|line| line.trim().strip_prefix("- "))
        .collect();
    lines.drain(..=end);
    call_seq
}

/// `# Say hello.` -> `Say hello.`, keeping any indentation the author used for code samples.
fn strip_marker(comment: &str) -> &str {
    let body = comment.trim_start().strip_prefix('#').unwrap_or(comment);
    body.strip_prefix(' ').unwrap_or(body)
}

/// A magic comment, a linter pragma, or a shebang — never documentation.
///
/// The test is deliberately narrow: an all-lowercase word followed immediately by a colon.
/// `TODO: rewrite` and `Note: this is fine` are prose and survive.
fn is_directive(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('!') || line.starts_with("-*-") {
        return true;
    }
    let Some((word, _)) = line.split_once(':') else {
        return false;
    };
    !word.is_empty()
        && word
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rubydex::offset::Offset;

    fn comments(lines: &[&str]) -> Vec<Comment> {
        lines
            .iter()
            .map(|line| Comment::new(Offset::new(0, 0), (*line).to_owned()))
            .collect()
    }

    #[test]
    fn singleton_methods_are_spelled_the_way_ruby_writes_them() {
        // rubydex models `def self.build` as a member of a synthetic singleton class. Showing
        // that spelling to a user would be showing them an implementation detail.
        assert_eq!(qualified_name("Person::<Person>#build()"), "Person.build");
        assert_eq!(qualified_name("Person#shout()"), "Person#shout");
        // The whole path, the same as the instance-method spelling above it: hover on two
        // methods of one class must not name the class two different ways.
        assert_eq!(qualified_name("Foo::Bar::<Bar>#baz()"), "Foo::Bar.baz");
        // Not a method at all: namespaces and constants pass through untouched.
        assert_eq!(qualified_name("Person::MAX_AGE"), "Person::MAX_AGE");
        assert_eq!(qualified_name("Person"), "Person");
    }

    #[test]
    fn a_symbol_list_gets_a_label_and_the_full_path_beside_it() {
        // The label matches the outline's spelling; the container is the *whole* path, because
        // `self.baz` on its own does not say which class it hangs off.
        assert_eq!(
            split_qualified("Foo::Bar::<Bar>#baz()"),
            ("self.baz".to_owned(), Some("Foo::Bar".to_owned()))
        );
        // A top-level `class << Foo` has no prefix to fall back on, only the attached name.
        assert_eq!(
            split_qualified("<Person>#build()"),
            ("self.build".to_owned(), Some("Person".to_owned()))
        );
        assert_eq!(
            split_qualified("Person#shout()"),
            ("shout".to_owned(), Some("Person".to_owned()))
        );
        assert_eq!(
            split_qualified("Person::MAX_AGE"),
            ("MAX_AGE".to_owned(), Some("Person".to_owned()))
        );
        // Top level: a container of `""` would render as an empty column.
        assert_eq!(split_qualified("Person"), ("Person".to_owned(), None));
    }

    #[test]
    fn magic_comments_are_not_documentation() {
        // Regression guard: rubydex allows one blank line between a comment block and the
        // definition below it, so the pragma at the top of a file attaches to the first class.
        let dropped = comments(&[
            "# frozen_string_literal: true",
            "# typed: strict",
            "# rubocop:disable Style/Documentation",
            "#",
            "# A person.",
        ]);
        assert_eq!(documentation(&dropped).unwrap(), "A person.");

        assert!(documentation(&comments(&["# encoding: utf-8"])).is_none());
        assert!(documentation(&comments(&["#!/usr/bin/env ruby"])).is_none());
        assert!(documentation(&[]).is_none());
    }

    #[test]
    fn rdocs_header_becomes_a_signature_block_instead_of_disappearing() {
        // Exactly the shape rbs core carries, one comment line each.
        let string_upcase = comments(&[
            "# <!--",
            "#   rdoc-file=string.c",
            "#   - upcase(mapping = :ascii) -> new_string",
            "# -->",
            "# Returns a new string containing the upcased characters in `self`.",
        ]);
        assert_eq!(
            documentation(&string_upcase).unwrap(),
            "```ruby\nupcase(mapping = :ascii) -> new_string\n```\n\nReturns a new string \
             containing the upcased characters in `self`."
        );

        // The block forms are the whole reason to keep the call-seq: the RBS signature has the
        // types, and this is the only place `each {|element| ... }` is spelled out.
        let array_each = comments(&[
            "# <!--",
            "#   rdoc-file=array.c",
            "#   - each {|element| ... } -> self",
            "#   - each -> new_enumerator",
            "# -->",
            "# Iterates over the elements of `self`.",
        ]);
        assert!(
            documentation(&array_each)
                .unwrap()
                .starts_with("```ruby\neach {|element| ... } -> self\neach -> new_enumerator\n```")
        );
    }

    #[test]
    fn an_html_comment_that_is_not_rdocs_header_is_left_alone() {
        // No closer: dropping to the end of the block would eat the documentation.
        let unclosed = comments(&["# <!--", "# still prose, somehow"]);
        assert_eq!(
            documentation(&unclosed).unwrap(),
            "<!--\nstill prose, somehow"
        );
        // And a header with nothing but the file name leaves no stray code block behind.
        let bare = comments(&["# <!--", "#   rdoc-file=string.c", "# -->", "# Prose."]);
        assert_eq!(documentation(&bare).unwrap(), "Prose.");
    }

    #[test]
    fn prose_that_merely_contains_a_colon_survives() {
        assert_eq!(
            documentation(&comments(&["# TODO: explain this", "# Note: it is fine"])).unwrap(),
            "TODO: explain this\nNote: it is fine"
        );
    }

    #[test]
    fn indentation_inside_a_comment_block_is_preserved() {
        // Doc comments carry indented code samples; collapsing them would break the fences.
        assert_eq!(
            documentation(&comments(&["# Example:", "#     Person.new", "#"])).unwrap(),
            "Example:\n    Person.new"
        );
    }
}
