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
                Parameter::RestPositional(_) => sigil("*", &name),
                Parameter::RequiredKeyword(_) => format!("{name}:"),
                Parameter::OptionalKeyword(_) => format!("{name}: ..."),
                Parameter::RestKeyword(_) => sigil("**", &name),
                Parameter::Block(_) => sigil("&", &name),
                Parameter::Forward(_) => "...".to_owned(),
            }
        })
        .collect();

    format!("({})", rendered.join(", "))
}

/// A rest, keyword-rest or block parameter, written once.
///
/// Ruby 3.x lets all three be anonymous — `def f(*, **, &)` — and rubydex records those under
/// the sigil itself rather than under an empty name, so prepending unconditionally spells `**`
/// as `****`. A parameter whose recorded name already *is* its sigil is written out as it
/// stands.
fn sigil(sigil: &str, name: &str) -> String {
    if name == sigil {
        sigil.to_owned()
    } else {
        format!("{sigil}{name}")
    }
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
    let body = to_markdown(&lines.join("\n"));
    if call_seq.is_empty() {
        return Some(body);
    }
    Some(format!("```ruby\n{}\n```\n\n{body}", call_seq.join("\n")))
}

/// RDoc's markup, as markdown a client will actually render.
///
/// Ruby's own signatures carry the documentation RDoc extracted from the C source, and it is
/// HTML in places — 1,867 `<code>` spans in the vendored copy alone, plus `<em>`, `<strong>`,
/// `<tt>`, `<b>` and `<i>`. A `MarkupContent` is markdown, and every client sanitises the HTML
/// out of it, so `<code><=></code>` reaches the user as a bare `<=>` that has lost its markup —
/// and a tag that is not markup at all (`<vowel>`, `<rhs>`, `<main>` and `<html>` all appear in
/// prose here) takes itself and its angle brackets away entirely, silently.
///
/// RDoc's links go nowhere either: `[Case Mapping](rdoc-ref:case_mapping.rdoc)` points into a
/// documentation tree the editor has never seen. 912 of them in `core/`, every one a dead word
/// the user can click.
///
/// Code is left exactly as written — a fenced block, an indented block, a backtick span. What
/// is inside them is Ruby, and `Hash<Symbol, untyped>` in an example must not grow a backslash.
fn to_markdown(text: &str) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let mut fenced = false;

    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        if fence {
            fenced = !fenced;
        }
        // A blank line is prose, so a verbatim block broken by one stays two blocks and the
        // run that gets converted stays as long as the sentence a link is written across.
        if fenced || fence || line.starts_with("    ") || line.starts_with('\t') {
            if !prose.is_empty() {
                chunks.push(converted(&prose.join("\n")));
                prose.clear();
            }
            chunks.push(line.to_owned());
        } else {
            prose.push(line);
        }
    }
    if !prose.is_empty() {
        chunks.push(converted(&prose.join("\n")));
    }
    chunks.join("\n")
}

/// One run of prose, converted. Takes whole lines, because RDoc wraps its links across them.
fn converted(prose: &str) -> String {
    let mut out = String::with_capacity(prose.len());
    let mut at = 0;

    while at < prose.len() {
        let rest = &prose[at..];
        if rest.starts_with('`') {
            // Already code, and its contents are not markup. Copied out whole.
            let span = code_span(rest);
            out.push_str(span);
            at += span.len();
        } else if let Some((label, taken)) = rdoc_link(rest) {
            out.push_str(&converted(label));
            at += taken;
        } else if let Some((rendered, taken)) = inline_tag(rest) {
            out.push_str(&rendered);
            at += taken;
        } else {
            let ch = rest
                .chars()
                .next()
                .expect("a non-empty remainder has a char");
            // Anything still angled here is not a tag markdown knows, and a renderer would eat
            // it and everything up to the next `>`. `Array<Integer>` is prose in these
            // comments far more often than it is markup.
            if ch == '<' {
                out.push('\\');
            }
            out.push(ch);
            at += ch.len_utf8();
        }
    }
    out
}

/// A backtick span, from its opening run to the matching closing run of the same length.
///
/// An opener with no closer is a stray backtick, and is one character of text.
fn code_span(rest: &str) -> &str {
    let fence = rest.len() - rest.trim_start_matches('`').len();
    match rest[fence..].find(&"`".repeat(fence)) {
        Some(end) => &rest[..fence + end + fence],
        None => &rest[..fence],
    }
}

/// `[label](rdoc-ref:…)` — the label, and how much of `rest` it accounted for.
///
/// Only RDoc's own scheme. An `https:` link in a comment is a link the editor can follow, and
/// is left exactly as it was written.
fn rdoc_link(rest: &str) -> Option<(&str, usize)> {
    let separator = rest.strip_prefix('[')?.find("](")? + 1;
    let target = &rest[separator + 2..];
    let end = target.find(')')?;
    target
        .starts_with("rdoc-ref:")
        .then(|| (&rest[1..separator], separator + 2 + end + 1))
}

/// An inline HTML tag as the markdown that means the same thing, and how much it accounted for.
fn inline_tag(rest: &str) -> Option<(String, usize)> {
    let close = rest.strip_prefix('<')?.find('>')?;
    let name = &rest[1..=close];
    let marker = match name {
        "code" | "tt" => "`",
        "em" | "i" => "*",
        "strong" | "b" => "**",
        _ => return None,
    };
    let opened = name.len() + 2;
    let closing = format!("</{name}>");
    let end = rest[opened..].find(&closing)?;
    let inner = &rest[opened..opened + end];
    let rendered = if marker == "`" {
        fenced_code(inner)
    } else {
        format!("{marker}{}{marker}", converted(inner))
    };
    Some((rendered, opened + end + closing.len()))
}

/// `inner` as a backtick span, whatever backticks it holds.
///
/// `<code>$`</code>` is in Ruby's own signatures — the global that holds what a match was
/// preceded by — and a one-backtick fence around it ends the span in the middle of the name.
/// CommonMark's answer is a longer fence, plus a space at each end when the content itself
/// starts or ends with one.
fn fenced_code(inner: &str) -> String {
    let longest = inner
        .split(|ch: char| ch != '`')
        .fold(0, |longest: usize, run| longest.max(run.len()));
    let fence = "`".repeat(longest + 1);
    let pad = if inner.starts_with('`') || inner.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{inner}{pad}{fence}")
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

#[cfg_attr(coverage_nightly, coverage(off))]
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
    fn a_method_with_no_signature_at_all_renders_no_parameter_list() {
        // `Signatures` is `Simple(one)` or `Overloaded(many)`, and the second is a boxed slice
        // that the type permits to be empty even though rubydex builds it from RBS overloads
        // and so never does. It is a pre-1.0 dependency: the answer to an empty one has to be
        // `Person#shout`, the same as for `def shout`, rather than an index out of range.
        let graph = Graph::new();
        assert_eq!(
            parameter_list(&graph, &Signatures::Overloaded(Box::default())),
            ""
        );
    }

    #[test]
    fn a_top_level_singleton_method_is_named_after_its_own_class() {
        // The path is what a nested class needs (`Foo::Bar.baz`), and a top-level class has no
        // path at all — there the singleton *is* the whole name. Prepending an empty prefix
        // would spell it `.build`.
        assert_eq!(qualified_name("<Person>#build()"), "Person.build");
        assert_eq!(qualified_name("Object::<Object>#puts()"), "Object.puts");
    }

    #[test]
    fn the_other_two_shapes_a_directive_takes() {
        // `magic_comments_are_not_documentation` covers `word: value`. These are the two the
        // word test cannot reach: an emacs modeline, and a line whose colon has no word before
        // it — which is prose, not a directive.
        assert_eq!(documentation(&comments(&["# -*- coding: utf-8 -*-"])), None);
        assert_eq!(
            documentation(&comments(&["# : not a directive"])).as_deref(),
            Some(": not a directive")
        );
    }

    #[test]
    fn an_rdoc_call_sequence_survives_with_no_prose_under_it() {
        // `rdocs_header_becomes_a_signature_block` always has prose below. With none, every
        // remaining line has been drained — and there is still something worth showing, which
        // is what stops `documentation` answering `None`.
        let rendered = documentation(&comments(&[
            "# <!--",
            "#   rdoc-file=string.c",
            "#   - obj.freeze -> obj",
            "# -->",
        ]))
        .expect("a call sequence is documentation");
        assert_eq!(rendered, "```ruby\nobj.freeze -> obj\n```\n\n");
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
    fn an_anonymous_rest_parameter_is_written_once() {
        // `def initialize(*, **, &)` is ordinary Ruby 3 and rubydex records each of the three
        // under its own sigil, which `format!("**{name}")` turns into `****`.
        assert_eq!(sigil("*", "*"), "*");
        assert_eq!(sigil("**", "**"), "**");
        assert_eq!(sigil("&", "&"), "&");
        assert_eq!(sigil("**", "options"), "**options");
        // Not a blanket strip: a parameter really named `*args` is not a thing, but a name that
        // merely starts with the sigil must not lose it either.
        assert_eq!(sigil("*", "*args"), "**args");
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
        // No closer: dropping to the end of the block would eat the documentation. The
        // opener survives as text — escaped, because an HTML comment a renderer *does*
        // understand takes the rest of the card away with it and says nothing.
        let unclosed = comments(&["# <!--", "# still prose, somehow"]);
        assert_eq!(
            documentation(&unclosed).unwrap(),
            "\\<!--\nstill prose, somehow"
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

    #[test]
    fn rdocs_html_becomes_the_markdown_that_means_the_same_thing() {
        // A `MarkupContent` is markdown and every client sanitises the HTML out of it, so a
        // `<code>` span reaches the user having lost its markup — and there are 1,867 of them
        // in the vendored signatures alone.
        assert_eq!(to_markdown("<code>:ascii</code>"), "`:ascii`");
        assert_eq!(to_markdown("<tt>nil</tt>"), "`nil`");
        assert_eq!(to_markdown("<em>self</em>"), "*self*");
        assert_eq!(to_markdown("<i>self</i>"), "*self*");
        assert_eq!(to_markdown("<strong>not</strong>"), "**not**");
        assert_eq!(to_markdown("<b>not</b>"), "**not**");
        // Nested, because emphasis around code is how RDoc writes a warning about a method.
        assert_eq!(
            to_markdown("<strong><code>nil</code></strong>"),
            "**`nil`**"
        );
    }

    #[test]
    fn a_tag_that_is_not_markup_keeps_its_angle_brackets() {
        // `<vowel>`, `<rhs>`, `<main>` and a whole `<html>` document all appear in the prose of
        // Ruby's own signatures. A renderer eats each of them along with everything up to the
        // next `>` and says nothing, which is the silent half of this finding.
        assert_eq!(
            to_markdown("matches <vowel> here"),
            "matches \\<vowel> here"
        );
        // An opener with no `>` at all, and a tag ya-lsp knows with no closer.
        assert_eq!(to_markdown("a < b"), "a \\< b");
        assert_eq!(to_markdown("<code>unclosed"), "\\<code>unclosed");
    }

    #[test]
    fn code_is_left_exactly_as_it_was_written() {
        // The escape above must not reach a code sample: `Hash<Symbol, untyped>` in an example
        // is Ruby, and a backslash in front of it is a visible bug rather than a silent one.
        assert_eq!(
            to_markdown("Prose <b>bold</b>:\n\n    Hash<Symbol, untyped>\n\n    more <em>x</em>"),
            "Prose **bold**:\n\n    Hash<Symbol, untyped>\n\n    more <em>x</em>"
        );
        // A tab is verbatim too, and a fenced block is verbatim including its fences.
        assert_eq!(to_markdown("\tHash<Symbol>"), "\tHash<Symbol>");
        assert_eq!(
            to_markdown("```ruby\nHash<Symbol>\n```\nafter <em>x</em>"),
            "```ruby\nHash<Symbol>\n```\nafter *x*"
        );
        // And a backtick span is already code, so what is inside it is not markup.
        assert_eq!(
            to_markdown("`Array<Integer>` and <b>b</b>"),
            "`Array<Integer>` and **b**"
        );
        // A stray opener is one character of text, not the start of a span that never ends.
        assert_eq!(to_markdown("a ` b <b>c</b>"), "a ` b **c**");
    }

    #[test]
    fn a_backtick_inside_a_code_tag_gets_a_fence_long_enough_to_hold_it() {
        // `<code>$`</code>` is in Ruby's own signatures — the global holding what a match was
        // preceded by — and a one-backtick fence ends the span in the middle of the name.
        assert_eq!(to_markdown("<code>$`</code>"), "`` $` ``");
        assert_eq!(to_markdown("<code>`</code>"), "`` ` ``");
        assert_eq!(to_markdown("<code>a`b</code>"), "``a`b``");
    }

    #[test]
    fn rdocs_own_links_go_nowhere_and_are_flattened_to_their_words() {
        // 912 of them in `core/` alone, every one pointing into a documentation tree the editor
        // has never seen. RDoc wraps them across lines, so the whole prose run is one unit.
        assert_eq!(
            to_markdown("see [Case Mapping](rdoc-ref:case_mapping.rdoc):"),
            "see Case Mapping:"
        );
        assert_eq!(
            to_markdown("see [Case\nMappings](rdoc-ref:case_mapping.rdoc@Case+Mappings)."),
            "see Case\nMappings."
        );
        // The label is prose too.
        assert_eq!(
            to_markdown("[the <code>x</code> form](rdoc-ref:a.rdoc)"),
            "the `x` form"
        );
        // A link the editor *can* follow is not RDoc's problem and is left alone.
        assert_eq!(
            to_markdown("[docs](https://ruby-lang.org)"),
            "[docs](https://ruby-lang.org)"
        );
        // And a bracket that is not a link at all stays a bracket, closed or not.
        assert_eq!(to_markdown("a[0] and b"), "a[0] and b");
        assert_eq!(to_markdown("see [x](rdoc-ref:a"), "see [x](rdoc-ref:a");
    }

    #[test]
    fn a_real_rdoc_comment_comes_out_readable() {
        // Lifted from `String#upcase` in the vendored signatures, which is the shape this whole
        // conversion exists for: a call-seq header, prose with backticks RDoc already wrote,
        // an indented example, an HTML span and a dead link — in one comment.
        let card = documentation(&comments(&[
            "# <!--",
            "#   rdoc-file=string.c",
            "#   - upcase(mapping = :ascii) -> new_string",
            "# -->",
            "# Returns a new string containing the upcased characters in `self`:",
            "#",
            "#     'hello'.upcase        # => \"HELLO\"",
            "#",
            "# The casing is affected by the given `mapping`, which may be",
            "# <code>:ascii</code>; see [Case",
            "# Mappings](rdoc-ref:case_mapping.rdoc@Case+Mappings).",
        ]))
        .unwrap();
        assert_eq!(
            card,
            "```ruby\nupcase(mapping = :ascii) -> new_string\n```\n\nReturns a new string \
             containing the upcased characters in `self`:\n\n    'hello'.upcase        # => \
             \"HELLO\"\n\nThe casing is affected by the given `mapping`, which may be\n\
             `:ascii`; see Case\nMappings."
        );
    }
}
