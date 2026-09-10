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
/// which is Ruby documentation's own spelling.
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
    signatures
        .as_slice()
        .first()
        .map_or_else(String::new, |signature| {
            signature_label(graph, "", signature).label
        })
}

/// A method's signature as Ruby, and where inside it each parameter was written.
///
/// The two are produced together on purpose. `signatureHelp` highlights a parameter by handing
/// the client a pair of offsets into this very string, and LSP's other spelling — the parameter
/// as a substring to search for — mis-highlights the moment a label holds the same token twice,
/// which `def each(key, value = key)` already does. So the function that writes the label is
/// the function that says where it wrote each piece, and nothing downstream counts characters.
///
/// **The offsets are UTF-16 code units**, which is what a client indexes the label by: the
/// protocol ties `Position` to the negotiated encoding and says nothing about these, and every
/// client that renders them is holding the label as a UTF-16 string. Ruby names can be
/// non-ASCII — `def приветствие(имя)` is legal — so the two counts genuinely differ.
#[must_use]
pub fn signature_label(graph: &Graph, name: &str, signature: &[Parameter]) -> Signature {
    let mut label = name.to_owned();
    let mut at = utf16_len(name);
    if signature.is_empty() {
        return Signature {
            label,
            parameters: Vec::new(),
        };
    }

    let mut parameters = Vec::with_capacity(signature.len());
    label.push('(');
    at += 1;
    for (index, parameter) in signature.iter().enumerate() {
        if index > 0 {
            label.push_str(", ");
            at += 2;
        }
        let written = spell(graph, parameter);
        let width = utf16_len(&written);
        parameters.push((at, at + width));
        label.push_str(&written);
        at += width;
    }
    label.push(')');

    Signature { label, parameters }
}

/// One rendered signature: the whole line, and one span per parameter inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    /// Start and end of each parameter in `label`, in UTF-16 code units, in signature order.
    pub parameters: Vec<(u32, u32)>,
}

/// One parameter, as Ruby writes it.
fn spell(graph: &Graph, parameter: &Parameter) -> String {
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
}

/// How long a string is to a client that holds it as UTF-16, saturating rather than wrapping —
/// a label long enough to overflow a `u32` is not one anybody is reading.
fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
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
    let mut prose: Vec<String> = Vec::new();
    let mut fenced = false;
    // Whether the indented lines below belong to a list item rather than to an example. RDoc
    // says the same two spaces mean both, and which one is decided by what opened above them —
    // see [`list_item`]. It survives a blank line, because a labelled list item with two
    // paragraphs is ordinary and the second is still the item's.
    let mut listing = false;

    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        if fence {
            fenced = !fenced;
        }
        if fenced || fence {
            flush(&mut chunks, &mut prose);
            chunks.push(line.to_owned());
            continue;
        }
        if let Some(heading) = heading(line) {
            flush(&mut chunks, &mut prose);
            chunks.push(heading);
            listing = false;
            continue;
        }
        if let Some(label) = list_item(line) {
            flush(&mut chunks, &mut prose);
            chunks.push(format!("- **{}**", converted(label)));
            listing = true;
            continue;
        }
        if line.trim().is_empty() {
            prose.push(line.to_owned());
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent >= 2 && !line.starts_with('\t') {
            if listing {
                // The item's own text, and it keeps its indentation: markdown reads an indented
                // line under a `-` as a continuation of it, which is what RDoc means by it too.
                prose.push(line.to_owned());
            } else {
                // A verbatim block, which is RDoc's **two** spaces and markdown's four. Every
                // example in Rails' own comments is written this way, and reading one as prose
                // is the whole of the report's "examples are plain text".
                flush(&mut chunks, &mut prose);
                let pad = " ".repeat(4_usize.saturating_sub(indent));
                chunks.push(format!("{pad}{line}"));
            }
            continue;
        }
        if line.starts_with('\t') {
            flush(&mut chunks, &mut prose);
            chunks.push(line.to_owned());
            continue;
        }
        // Back at the margin with something on the line: whatever list was open is closed.
        listing = false;
        prose.push(line.to_owned());
    }
    flush(&mut chunks, &mut prose);
    chunks.join("\n")
}

/// Convert whatever prose has accumulated and put it in `chunks`.
fn flush(chunks: &mut Vec<String>, prose: &mut Vec<String>) {
    if !prose.is_empty() {
        chunks.push(converted(&prose.join("\n")));
        prose.clear();
    }
}

/// `== Options` -> `## Options`, and nothing for a line that is not a heading.
///
/// RDoc's heading is a run of `=` at the margin followed by a space, which is the one spelling
/// markdown does not share — markdown's own underline form never appears in these comments.
/// Six levels, because that is where markdown stops.
fn heading(line: &str) -> Option<String> {
    let level = line.len() - line.trim_start_matches('=').len();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = line[level..].strip_prefix(' ')?;
    Some(format!("{} {rest}", "#".repeat(level)))
}

/// The label of an RDoc labelled list item — `[+:autosave+]` or `autosave::` — at the margin.
///
/// The one construct that has to be recognised before the indentation is read, because it is
/// what makes the two spaces below it mean *description* rather than *example*. Rails writes 26
/// of these in `has_many`'s comment alone, and read as verbatim every option's description
/// became a code block.
fn list_item(line: &str) -> Option<&str> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    if let Some(inner) = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return (!inner.is_empty() && !inner.contains(']')).then_some(inner);
    }
    let label = line.strip_suffix("::")?;
    (!label.is_empty() && !label.contains(' ')).then_some(label)
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
        } else if let Some((rendered, taken)) = braced_link(rest) {
            out.push_str(&rendered);
            at += taken;
        } else if let Some((code, taken)) = plus_code(rest, out.chars().last()) {
            out.push_str(&code);
            at += taken;
        } else if let Some(taken) = suppressed(rest) {
            out.push_str(&rest[1..taken]);
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

/// `{text}[url]` — RDoc's own link, which is the spelling a `.rb` file uses.
///
/// [`rdoc_link`] handles `[text](url)`, which is what the *vendored signatures* carry because
/// RDoc generated them; a gem's own source is written in RDoc itself and needs this one. An
/// `rdoc-ref:` target points into a documentation tree the editor has never seen, so it goes
/// the way the other one goes — the words stay and the dead link does not; a
/// real URL is kept, because an editor can follow it.
fn braced_link(rest: &str) -> Option<(String, usize)> {
    let end = rest.strip_prefix('{')?.find("}[")?;
    let text = &rest[1..=end];
    let target = &rest[end + 3..];
    let close = target.find(']')?;
    let url = &target[..close];
    let taken = end + 3 + close + 1;
    if url.starts_with("rdoc-ref:") || url.contains(' ') {
        return Some((converted(text), taken));
    }
    Some((format!("[{}]({url})", converted(text)), taken))
}

/// `+word+` as a code span, RDoc's own emphasis for code.
///
/// The same thing `<tt>` means, and the report saw them treated differently in one card:
/// `<tt>:autosave</tt>` came out as code and `+:autosave+` as three literal characters and a
/// word. RDoc's rule is that the `+` must open at a non-word boundary and close before one, and
/// that nothing inside may be whitespace — which is what keeps `1 + 2` and `a+b` prose.
fn plus_code(rest: &str, previous: Option<char>) -> Option<(String, usize)> {
    if previous.is_some_and(|ch| ch.is_alphanumeric() || ch == '_') {
        return None;
    }
    let inner = rest.strip_prefix('+')?;
    let end = inner.find('+')?;
    let word = &inner[..end];
    if word.is_empty() || word.chars().any(char::is_whitespace) {
        return None;
    }
    // A word character straight after the closing `+` means it never closed a span.
    if inner[end + 1..]
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((fenced_code(word), end + 2))
}

/// `\Word` — RDoc's escape, which asks for the word and no link. The backslash is not text.
///
/// Only before a letter, because `\n` inside a sentence about escapes is the thing itself and
/// markdown would eat the backslash anyway.
fn suppressed(rest: &str) -> Option<usize> {
    let word = rest.strip_prefix('\\')?;
    let first = word.chars().next()?;
    first.is_alphabetic().then(|| 1 + first.len_utf8())
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

/// RDoc's own visibility directives, which are not prose and are the whole comment where they
/// are the whole comment.
///
/// `:nodoc:` above a `def` means "there is no documentation here", and the card was printing the
/// word — which is worse than the empty card it is asking for, because a reader takes a card
/// with something in it as an answer.
const RDOC_DIRECTIVES: [&str; 6] = [
    ":nodoc:",
    ":doc:",
    ":startdoc:",
    ":stopdoc:",
    ":enddoc:",
    ":yields:",
];

/// A magic comment, a linter pragma, an RDoc directive, or a shebang — never documentation.
///
/// The test is deliberately narrow: an all-lowercase word followed immediately by a colon.
/// `TODO: rewrite` and `Note: this is fine` are prose and survive.
fn is_directive(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('!') || line.starts_with("-*-") {
        return true;
    }
    // `:nodoc: all` is the spelling with an argument; both are the directive and neither is
    // documentation.
    if RDOC_DIRECTIVES
        .iter()
        .any(|directive| line == *directive || line.starts_with(&format!("{directive} ")))
    {
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
            .map(|line| Comment::new(Offset::new(0, 0), (*line).into()))
            .collect()
    }

    /// The fixture is `ActiveRecord::Associations::ClassMethods#has_many`'s own
    /// comment: a labelled list of options, a paragraph under each label, and then a run of
    /// examples. All three are two spaces in RDoc and mean two different things.
    #[test]
    fn rdoc_written_in_a_gems_own_source_renders_as_rdoc() {
        let card = documentation(&comments(&[
            "# == Options",
            "#",
            "# [+:autosave+]",
            "#   If true, always save the associated objects. This option is implemented as a",
            "#   +before_save+ callback.",
            "# [+:inverse_of+]",
            "#   Specifies the name of the association.",
            "#   See {Bi-directional}[rdoc-ref:Associations::ClassMethods@Bi] for more detail.",
            "#",
            "# Option examples:",
            "#   has_many :comments, -> { order(\"posted_on\") }",
            "#   has_many :tags, as: :taggable",
        ]))
        .expect("a card");
        assert_eq!(
            card,
            "\
## Options

- **`:autosave`**
  If true, always save the associated objects. This option is implemented as a
  `before_save` callback.
- **`:inverse_of`**
  Specifies the name of the association.
  See Bi-directional for more detail.

Option examples:
    has_many :comments, -> { order(\"posted_on\") }
    has_many :tags, as: :taggable"
        );
    }

    /// The inconsistency the report actually saw: one card, two spellings of the same thing,
    /// and only one of them read.
    #[test]
    fn plus_and_tt_are_the_same_markup() {
        let plus = documentation(&comments(&["# Set +:autosave+ to true."]));
        let tt = documentation(&comments(&["# Set <tt>:autosave</tt> to true."]));
        assert_eq!(plus.as_deref(), Some("Set `:autosave` to true."));
        assert_eq!(plus, tt);
    }

    /// What a `+` is when it is arithmetic, a word, or one of a pair with a space in it.
    #[test]
    fn a_plus_that_is_not_markup_stays_a_plus() {
        for line in [
            "# The sum of 1 + 2 is 3.",
            "# Written a+b+c in the source.",
            "# Use + to add and + to concatenate.",
        ] {
            let rendered = documentation(&comments(&[line])).expect("a card");
            assert_eq!(rendered, strip_marker(line), "{line}");
        }
    }

    /// `:nodoc:` is RDoc saying there is nothing here, and a card with the word in it is worse
    /// than no card: a reader takes something in a card as an answer.
    #[test]
    fn a_nodoc_comment_is_no_documentation_at_all() {
        assert_eq!(documentation(&comments(&["# :nodoc:"])), None);
        assert_eq!(documentation(&comments(&["# :nodoc: all"])), None);
        assert_eq!(documentation(&comments(&["# :stopdoc:"])), None);
        // And it only leads. A `:nodoc:` written *after* prose is somebody discussing the
        // directive, and the prose above it is documentation.
        assert_eq!(
            documentation(&comments(&["# Marks it hidden.", "# :nodoc:"])).as_deref(),
            Some("Marks it hidden.\n:nodoc:")
        );
    }

    /// The guarantee that must not move: what is inside a verbatim block is Ruby, and a
    /// generic in an example must not grow a backslash.
    #[test]
    fn a_verbatim_block_is_never_escaped_however_it_is_indented() {
        let card = documentation(&comments(&[
            "# Returns a hash:",
            "#   Hash<Symbol, untyped>",
            "# and a Array<Integer> in prose.",
        ]))
        .expect("a card");
        assert!(card.contains("    Hash<Symbol, untyped>"), "{card}");
        assert!(card.contains("a Array\\<Integer> in prose"), "{card}");
    }

    /// The shapes each RDoc reader declines, one per way of not being the thing.
    #[test]
    fn the_rdoc_spellings_that_are_not_markup() {
        let card = |lines: &[&str]| documentation(&comments(lines)).expect("a card");

        // A heading deeper than markdown has, and a run of `=` with no space after it.
        assert_eq!(card(&["# ======= Too deep"]), "======= Too deep");
        assert_eq!(card(&["# ==nospace"]), "==nospace");
        // A tab-indented block is verbatim and is left exactly as it was written, at one tab
        // and at two — the second is the one that gets past the two-space test first.
        assert_eq!(card(&["# Prose.", "#\tstill_code"]), "Prose.\n\tstill_code");
        assert_eq!(card(&["# Prose.", "#\t\tdeeper"]), "Prose.\n\t\tdeeper");
        // `[a]b]` is not a label: RDoc's label runs to the *first* `]`, so a line holding two
        // is prose that happens to start with a bracket.
        assert_eq!(card(&["# [a]b]"]), "[a]b]");
        // The other spelling of a labelled list, which `is_directive` would eat if it led.
        assert_eq!(
            card(&["# Options.", "# autosave::", "#   If true."]),
            "Options.\n- **autosave**\n  If true."
        );
        // A label with a space in it is a sentence ending in a colon pair, not a list; and
        // neither empty spelling of either form is one either.
        assert_eq!(card(&["# Prose.", "# see also::"]), "Prose.\nsee also::");
        assert_eq!(card(&["# Prose.", "# []"]), "Prose.\n[]");
        assert_eq!(card(&["# Prose.", "# ::"]), "Prose.\n::");
        // A link with a real target keeps it, and one whose target is not a URL at all keeps
        // only its words.
        assert_eq!(
            card(&["# See {the guide}[https://example.com/g] for more."]),
            "See [the guide](https://example.com/g) for more."
        );
        assert_eq!(
            card(&["# See {the guide}[not a url] for more."]),
            "See the guide for more."
        );
        // `++` has nothing between the pluses, and `+a+b` never closed: both are prose.
        assert_eq!(card(&["# An empty ++ pair."]), "An empty ++ pair.");
        assert_eq!(
            card(&["# Written +a+b in the source."]),
            "Written +a+b in the source."
        );
    }

    /// A `\\Word` is RDoc asking for the word without a link.
    #[test]
    fn a_suppressed_link_keeps_its_word_and_loses_its_backslash() {
        assert_eq!(
            documentation(&comments(&["# See \\Array for more."])).as_deref(),
            Some("See Array for more.")
        );
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
