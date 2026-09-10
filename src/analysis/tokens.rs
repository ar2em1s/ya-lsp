//! `textDocument/semanticTokens` — the identifiers a grammar cannot classify.
//!
//! Every editor ships a TextMate grammar for Ruby and it does the lexical half well: keywords,
//! strings, numbers, `@ivars`, `$globals`, `CONSTANTS`. Those are decidable from the characters,
//! and re-sending them from here would be work for no change on the screen.
//!
//! One thing is not decidable from the characters, and it is everywhere:
//!
//! ```ruby
//! def render(scale)
//!   size = scale * 2
//!   size          # a local variable
//!   width         # a method call on self
//! end
//! ```
//!
//! `size` and `width` are the same characters in the same position and are two different things.
//! The answer is "was this name assigned anywhere in this scope", and a scope is a parse. Prism
//! has already decided it — the same bare word arrives as a `LocalVariableReadNode` or as a
//! `CallNode` — and reading that back is the whole of this module. So the legend is three types
//! and no modifiers: everything in it is something the parse knows and the characters do not.
//!
//! # Why every call is sent, and not only the ambiguous ones
//!
//! `foo.bar` is unambiguous — the `.` gives it away — so a strict reading would send `bar` no
//! token and let the grammar colour it. That is wrong on the screen: `render` would be coloured
//! in one place and not in another, which reads as a bug rather than a rule. Semantic tokens
//! replace the grammar's answer wherever they are sent, so what has to be consistent is *the set
//! of things sent*, not the set of things that were hard.
//!
//! What is skipped is what has no name to colour: `a + b`, `list[0]` and `x <=> y` are all calls
//! in Ruby, and colouring their operators as method names is true, useless and ugly.
//!
//! # Why there is no delta
//!
//! `semanticTokens/full/delta` lets a client re-ask with a previous result id and be sent the
//! edits rather than the whole list. It is a wire optimisation, not a different answer, and it
//! costs the server a cache of every response it has sent per document, keyed by an id it must
//! invalidate on every edit. ya-lsp declines it: the whole-file answer for the largest file in a
//! real Rails application is measured in microseconds. See
//! [`analysis::threaded_tests`](super::threaded_tests) for the measurement and for what it costs
//! the requests queued behind it.

use ruby_prism::{
    BlockLocalVariableNode, BlockParameterNode, CallNode, DefNode, ItLocalVariableReadNode,
    KeywordRestParameterNode, LocalVariableAndWriteNode, LocalVariableOperatorWriteNode,
    LocalVariableOrWriteNode, LocalVariableReadNode, LocalVariableTargetNode,
    LocalVariableWriteNode, Location, OptionalKeywordParameterNode, OptionalParameterNode,
    RequiredKeywordParameterNode, RequiredParameterNode, RestParameterNode, Visit,
};

/// What a token is, as an index into [`LEGEND`].
///
/// The numbers are the wire format: a client reads them against the legend the server sent at
/// initialize, so the order of the two must never disagree. [`tests::the_legend_is_the_wire`]
/// is what holds them together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A local variable, in any of the eight ways Ruby writes one.
    Variable = 0,
    /// A parameter, at the point it is declared. Reads of it inside the body are locals, which
    /// is what they are.
    Parameter = 1,
    /// A method, by the name where it is called and where it is defined.
    Method = 2,
}

/// The token types this server sends, in the order their indices mean.
///
/// Sent verbatim as the `legend.tokenTypes` of the server's capabilities. There are no
/// modifiers: every modifier LSP defines is either something the grammar has (`readonly` on a
/// constant) or something no test would be able to say was wrong.
pub const LEGEND: [&str; 3] = ["variable", "parameter", "method"];

/// One token, as a byte span in the document and a kind.
///
/// Byte spans, not positions: turning an offset into a line and a character is
/// [`position`](super::position)'s job and depends on the encoding the client negotiated. This
/// module would have to be given the document to do it, and it has no other reason to want one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub start: u32,
    pub end: u32,
    pub kind: Kind,
}

/// Every token in `source`, in source order.
///
/// Sorted here rather than by the caller because the protocol requires it — the wire format is a
/// list of *deltas* from the previous token, so an out-of-order entry is not a misplaced colour
/// but a corrupted rest-of-file. The walk emits in Prism's order, which is close to source order
/// and is not it: a call's arguments are visited after its receiver, and a `rescue` after the
/// body it guards.
#[must_use]
pub fn of(source: &str) -> Vec<Token> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut walk = Walk {
        source,
        found: Vec::new(),
    };
    walk.visit(&result.node());
    walk.found.sort_by_key(|token| (token.start, token.end));
    walk.found
}

struct Walk<'s> {
    source: &'s str,
    found: Vec<Token>,
}

impl Walk<'_> {
    fn push(&mut self, at: &Location<'_>, kind: Kind) {
        self.push_span(at.start_offset() as u32, at.end_offset() as u32, kind);
    }

    fn push_span(&mut self, start: u32, end: u32, kind: Kind) {
        // A zero-width span is Prism recovering from something half-written — `foo.` gives the
        // call an empty message exactly at the cursor. A token of length zero is legal on the
        // wire and invisible on the screen, so it is only weight.
        if start < end {
            self.found.push(Token { start, end, kind });
        }
    }

    /// Whether a call's message is a name rather than an operator.
    ///
    /// `a + b`, `list[0]` and `x <=> y` are calls, and their messages are `+`, `[]` and `<=>`.
    /// Ruby really does dispatch them, and colouring them as method names would be true and
    /// unreadable. A trailing `?`, `!` or `=` is part of a name and not an operator, so the test
    /// is on the *first* character.
    fn is_named(&self, at: &Location<'_>) -> bool {
        self.source[at.start_offset()..at.end_offset()]
            .chars()
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_')
    }
}

impl<'pr> Visit<'pr> for Walk<'_> {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if let Some(message) = node.message_loc()
            && self.is_named(&message)
        {
            self.push(&message, Kind::Method);
        }
        ruby_prism::visit_call_node(self, node);
    }

    /// The name in `def render`, which the grammar can see coming from the `def` — and which is
    /// sent anyway, because the same name at a call site is sent and a colour that changes
    /// between a definition and its uses reads as a bug rather than as a rule.
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        self.push(&node.name_loc(), Kind::Method);
        ruby_prism::visit_def_node(self, node);
    }

    // The eight ways Ruby writes a local. Every one of them is a `variable`: which of them is a
    // *declaration* is a question Ruby does not really have an answer to — the first assignment
    // in a scope declares it, and which one that is depends on control flow.

    fn visit_local_variable_read_node(&mut self, node: &LocalVariableReadNode<'pr>) {
        self.push(&node.location(), Kind::Variable);
    }

    fn visit_local_variable_write_node(&mut self, node: &LocalVariableWriteNode<'pr>) {
        self.push(&node.name_loc(), Kind::Variable);
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    /// `a, b = 1, 2`, `rescue => e`, and `in [a, b]` all arrive here.
    fn visit_local_variable_target_node(&mut self, node: &LocalVariableTargetNode<'pr>) {
        self.push(&node.location(), Kind::Variable);
    }

    fn visit_local_variable_and_write_node(&mut self, node: &LocalVariableAndWriteNode<'pr>) {
        self.push(&node.name_loc(), Kind::Variable);
        ruby_prism::visit_local_variable_and_write_node(self, node);
    }

    fn visit_local_variable_or_write_node(&mut self, node: &LocalVariableOrWriteNode<'pr>) {
        self.push(&node.name_loc(), Kind::Variable);
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_local_variable_operator_write_node(
        &mut self,
        node: &LocalVariableOperatorWriteNode<'pr>,
    ) {
        self.push(&node.name_loc(), Kind::Variable);
        ruby_prism::visit_local_variable_operator_write_node(self, node);
    }

    fn visit_block_local_variable_node(&mut self, node: &BlockLocalVariableNode<'pr>) {
        self.push(&node.location(), Kind::Variable);
    }

    /// `it` inside a block, which Ruby 3.4 made a read of a local the block declares.
    fn visit_it_local_variable_read_node(&mut self, node: &ItLocalVariableReadNode<'pr>) {
        self.push(&node.location(), Kind::Variable);
    }

    // Parameters, in the six shapes that carry a name. `def f(a, (b, c))` destructures into
    // plain required parameters, so the awkward spelling needs no case of its own.

    fn visit_required_parameter_node(&mut self, node: &RequiredParameterNode<'pr>) {
        self.push(&node.location(), Kind::Parameter);
    }

    fn visit_optional_parameter_node(&mut self, node: &OptionalParameterNode<'pr>) {
        self.push(&node.name_loc(), Kind::Parameter);
        ruby_prism::visit_optional_parameter_node(self, node);
    }

    fn visit_rest_parameter_node(&mut self, node: &RestParameterNode<'pr>) {
        if let Some(name) = node.name_loc() {
            self.push(&name, Kind::Parameter);
        }
    }

    fn visit_keyword_rest_parameter_node(&mut self, node: &KeywordRestParameterNode<'pr>) {
        if let Some(name) = node.name_loc() {
            self.push(&name, Kind::Parameter);
        }
    }

    /// A keyword parameter's name span carries its colon (`limit:`), which is not part of the
    /// name anywhere else it is written. Trimmed so the token covers what a reader would call
    /// the name.
    fn visit_required_keyword_parameter_node(&mut self, node: &RequiredKeywordParameterNode<'pr>) {
        self.push_keyword(&node.name_loc());
    }

    fn visit_optional_keyword_parameter_node(&mut self, node: &OptionalKeywordParameterNode<'pr>) {
        self.push_keyword(&node.name_loc());
        ruby_prism::visit_optional_keyword_parameter_node(self, node);
    }

    fn visit_block_parameter_node(&mut self, node: &BlockParameterNode<'pr>) {
        if let Some(name) = node.name_loc() {
            self.push(&name, Kind::Parameter);
        }
    }
}

impl Walk<'_> {
    /// A keyword parameter, whose name span carries the colon Ruby writes after it.
    ///
    /// Trimmed unconditionally rather than behind an `ends_with`, because a keyword parameter
    /// always has one and a branch for the case that cannot happen is a branch no test can
    /// take. `scopes` guards the same trim, and rightly: there the span arrives from every kind
    /// of variable and most of them have no colon.
    fn push_keyword(&mut self, at: &Location<'_>) {
        let start = at.start_offset() as u32;
        let name = self.source[at.start_offset()..at.end_offset()].trim_end_matches(':');
        self.push_span(start, start + name.len() as u32, Kind::Parameter);
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// The source with every token underlined beneath the line it is on, named by its kind.
    ///
    /// Drawn rather than asserted as spans, for the reason `signature_help`'s fixtures are
    /// drawn: a token one character short colours the wrong text, which is visible at a glance
    /// and reads as the bug it is, while a list of triples shows nobody anything.
    fn drawn(source: &str) -> String {
        let tokens = of(source);
        let mut out = String::new();
        let mut consumed = 0;
        for (number, line) in source.lines().enumerate() {
            let start = consumed;
            consumed += line.len() + 1;
            out.push_str(line);
            out.push('\n');
            let mine: Vec<&Token> = tokens
                .iter()
                .filter(|token| (token.start as usize) >= start && (token.end as usize) <= consumed)
                .collect();
            if mine.is_empty() {
                continue;
            }
            let mut underline = vec![b' '; line.len()];
            for token in &mine {
                for slot in token.start as usize - start..token.end as usize - start {
                    if let Some(cell) = underline.get_mut(slot) {
                        *cell = match token.kind {
                            Kind::Variable => b'v',
                            Kind::Parameter => b'p',
                            Kind::Method => b'm',
                        };
                    }
                }
            }
            let _ = number;
            out.push_str(String::from_utf8_lossy(&underline).trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_ambiguity_no_grammar_can_resolve() {
        // The reason the whole module exists, in four lines. `size` and `width` are the same
        // characters in the same position; only the parse knows that one of them was assigned.
        assert_eq!(
            drawn("def render(scale)\n  size = scale * 2\n  size\n  width\nend\n"),
            "\
def render(scale)
    mmmmmm ppppp
  size = scale * 2
  vvvv   vvvvv
  size
  vvvv
  width
  mmmmm
end
"
        );
    }

    #[test]
    fn an_operator_is_a_call_with_no_name_to_colour() {
        // Every one of these is a method call in Ruby. Colouring them would be true and
        // unreadable, and the test is on the first character so that `empty?`, `save!` and
        // `name=` keep theirs.
        assert_eq!(
            drawn("a = [1]\na[0] <=> a.size\na.name = 1\na.empty?\n"),
            "\
a = [1]
v
a[0] <=> a.size
v        v mmmm
a.name = 1
v mmmm
a.empty?
v mmmmmm
"
        );
    }

    #[test]
    fn every_shape_of_local_and_parameter() {
        // The eight local spellings and the six parameter ones, in one file: each of them is a
        // visitor override, and an override that stops firing is invisible on the screen
        // — the identifier simply falls back to the grammar's colour, which is a plausible one.
        assert_eq!(
            drawn(
                "\
def f(a, b = 1, *rest, key:, opt: 2, **kw, &blk)
  a, c = 1, 2
  c &&= 1
  c ||= 2
  c += 3
  [1].each { |x; local| x }
  [1].each { it }
  begin
  rescue => e
    e
  end
end
"
            ),
            "\
def f(a, b = 1, *rest, key:, opt: 2, **kw, &blk)
    m p  p       pppp  ppp   ppp       pp   ppp
  a, c = 1, 2
  v  v
  c &&= 1
  v
  c ||= 2
  v
  c += 3
  v
  [1].each { |x; local| x }
      mmmm    p  vvvvv  v
  [1].each { it }
      mmmm   vv
  begin
  rescue => e
            v
    e
    v
  end
end
"
        );
    }

    #[test]
    fn the_order_is_the_source_and_not_the_walk() {
        // The wire format is a list of deltas from the previous token, so an entry out of order
        // is not one misplaced colour — every token after it lands somewhere else. Prism's walk
        // is close to source order and is not it: a call's arguments come after its receiver,
        // and a `rescue` after the body it guards.
        let tokens = of("outer(inner(1)) { |x| x }\nbegin\n  a = 1\nrescue => e\n  e\nend\n");
        let starts: Vec<u32> = tokens.iter().map(|token| token.start).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted, "{tokens:?}");
    }

    #[test]
    fn a_parameter_with_no_name_has_nothing_to_colour() {
        // Ruby 3.1 and 3.2 made `*`, `**` and `&` legal on their own, to be forwarded. There is
        // no name, so there is no token, and each of the three is a visitor of its own.
        assert_eq!(
            drawn("def f(*, **, &)\n  g(*, **, &)\nend\n"),
            "\
def f(*, **, &)
    m
  g(*, **, &)
  m
end
"
        );
    }

    #[test]
    fn a_half_typed_call_has_no_name_to_colour_either() {
        // Two shapes, and both arrive constantly: an editor asks for tokens on every keystroke.
        // `foo.` recovers into a call whose message is empty and exactly at the cursor — a
        // zero-width token, legal on the wire and invisible on the screen. `foo.()` is `call`
        // written with no name at all, and has no message span.
        assert_eq!(
            drawn("x = 1\nx.\n"),
            "\
x = 1
v
x.
v
"
        );
        assert_eq!(
            drawn("x = 1\nx.()\n"),
            "\
x = 1
v
x.()
v
"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_still_answers() {
        // Prism recovers; whatever it managed to read is still worth colouring, and a file
        // being typed into is not valid Ruby most of the time.
        let tokens = of("def f(a)\n  a\n");
        assert_eq!(tokens.len(), 3, "{tokens:?}");
        assert!(of("").is_empty());
    }

    #[test]
    fn the_legend_is_the_wire() {
        // The numbers `Kind` carries *are* the protocol: a client reads them as indices into
        // the legend the server sent at initialize. Reordering either without the other
        // recolours every token in every file, silently and consistently, which is the hardest
        // kind of wrong to notice.
        assert_eq!(LEGEND.len(), 3);
        assert_eq!(LEGEND[Kind::Variable as usize], "variable");
        assert_eq!(LEGEND[Kind::Parameter as usize], "parameter");
        assert_eq!(LEGEND[Kind::Method as usize], "method");
    }
}
