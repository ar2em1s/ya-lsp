//! `textDocument/semanticTokens`: the identifiers a grammar cannot classify.
//!
//! Every editor ships a TextMate grammar for Ruby, and it does the lexical half well: keywords,
//! strings, numbers, `@ivars`, `$globals`, `CONSTANTS`. Those are decidable from the characters,
//! and re-sending them from here would change nothing on the screen.
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
//! `size` and `width` are the same characters in the same position, and two different things. The
//! answer is "was this name assigned anywhere in this scope", and a scope is a parse. Prism already
//! decided it (the same bare word arrives as a `LocalVariableReadNode` or as a `CallNode`), and
//! reading that back is all this module does. So the legend is three types and no modifiers:
//! everything in it is something the parse knows and the characters do not.
//!
//! # Why every call is sent, not only the ambiguous ones
//!
//! `foo.bar` is unambiguous (the `.` gives it away), so a strict reading would send `bar` no token
//! and let the grammar colour it. That looks wrong: `render` would be coloured in one place and not
//! another, which reads as a bug, not a rule. Semantic tokens replace the grammar's answer wherever
//! they are sent, so what must be consistent is *the set of things sent*, not the set of things
//! that were hard.
//!
//! What is skipped is what has no name to colour: `a + b`, `list[0]` and `x <=> y` are all calls in
//! Ruby, and colouring their operators as method names is true, useless and ugly.
//!
//! # Why there is no delta
//!
//! `semanticTokens/full/delta` lets a client re-ask with a previous result id and get the edits
//! instead of the whole list. It is a wire optimisation, not a different answer, and it costs the
//! server a cache of every response per document, keyed by an id it must invalidate on every edit.
//! ya-lsp declines it: the whole-file answer for the largest file in a real Rails application is
//! cheap. `analysis::threaded_tests` measures it, and what it costs the requests queued behind it.

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
/// initialize, so the two orders must never disagree. `tests::the_legend_is_the_wire` holds them
/// together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A local variable, in any of the eight ways Ruby writes one.
    Variable = 0,
    /// A parameter, where it is declared. Reads of it inside the body are locals, which is what
    /// they are.
    Parameter = 1,
    /// A method, by the name where it is called and where it is defined.
    Method = 2,
}

/// The token types this server sends, in the order their indices mean.
///
/// Sent verbatim as the `legend.tokenTypes` of the server's capabilities. No modifiers: every
/// modifier LSP defines is either something the grammar has (`readonly` on a constant) or something
/// no test could call wrong.
pub const LEGEND: [&str; 3] = ["variable", "parameter", "method"];

/// One token, as a byte span in the document and a kind.
///
/// Byte spans, not positions: turning an offset into a line and character is
/// [`position`](super::position)'s job, and depends on the encoding the client negotiated. This
/// module would need the document to do it, and has no other reason to want one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub start: u32,
    pub end: u32,
    pub kind: Kind,
}

/// Every token in `source`, in source order.
///
/// Sorted here, not by the caller, because the protocol requires it: the wire format is a list of
/// *deltas* from the previous token, so an out-of-order entry is not a misplaced colour but a
/// corrupted rest of file. The walk emits in Prism's order, which is close to source order but not
/// it: a call's arguments are visited after its receiver, and a `rescue` after the body it guards.
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
        // A zero-width span is Prism recovering from something half-written. The shape is not
        // `foo.` (`is_named` turns that call's empty message away a step earlier); it is `def` at
        // the end of the file, whose name span is empty and exactly at the cursor. A zero-length
        // token is legal on the wire and invisible on the screen, so it is only weight.
        if start < end {
            self.found.push(Token { start, end, kind });
        }
    }

    /// Whether a call's message is a name rather than an operator.
    ///
    /// `a + b`, `list[0]` and `x <=> y` are calls, with messages `+`, `[]` and `<=>`. Ruby really
    /// dispatches them, and colouring them as method names would be true and unreadable. A trailing
    /// `?`, `!` or `=` is part of a name, not an operator, so the test is on the *first* character.
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

    /// The name in `def render`, which the grammar can see coming from the `def`. Sent anyway,
    /// because the same name at a call site is sent, and a colour that changes between a definition
    /// and its uses reads as a bug, not a rule.
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        self.push(&node.name_loc(), Kind::Method);
        ruby_prism::visit_def_node(self, node);
    }

    // The eight ways Ruby writes a local. Every one is a `variable`: which of them is a
    // *declaration* is a question Ruby has no real answer to, because the first assignment in a
    // scope declares it, and which one that is depends on control flow.

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

    // Parameters, in the six shapes that carry a name. `def f(a, (b, c))` destructures into plain
    // required parameters, so it needs no case of its own.

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

    /// A keyword parameter's name span includes its colon (`limit:`), which is not part of the name
    /// anywhere else it is written. Trimmed so the token covers what a reader calls the name.
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
    /// A keyword parameter, whose name span includes the colon Ruby writes after it.
    ///
    /// Trimmed unconditionally, not behind an `ends_with`: a keyword parameter always has one, and
    /// a branch for an impossible case is a branch no test can take. `scopes` guards the same trim,
    /// rightly: there the span comes from every kind of variable, and most have no colon.
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
    use crate::analysis::testing::*;
    use crate::analysis::tokens;

    /// The source with every token underlined beneath its line, named by its kind.
    ///
    /// Drawn, not asserted as spans, for the reason `signature_help`'s fixtures are drawn: a token
    /// one character short colours the wrong text, which is visible at a glance and reads as the
    /// bug it is, while a list of triples shows nobody anything.
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
        // The reason the module exists, in four lines. `size` and `width` are the same characters
        // in the same position; only the parse knows one of them was assigned.
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
        // Every one of these is a method call in Ruby. Colouring them would be true and unreadable,
        // and the test is on the first character so `empty?`, `save!` and `name=` keep theirs.
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
        // The eight local spellings and the six parameter ones, in one file. Each is a visitor
        // override, and an override that stops firing is invisible on the screen: the identifier
        // falls back to the grammar's colour, which is a plausible one.
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
        // The wire format is deltas from the previous token, so an out-of-order entry is not one
        // misplaced colour: every token after it lands somewhere else. Prism's walk is close to
        // source order but not it: a call's arguments come after its receiver, and a `rescue` after
        // the body it guards.
        let tokens = of("outer(inner(1)) { |x| x }\nbegin\n  a = 1\nrescue => e\n  e\nend\n");
        let starts: Vec<u32> = tokens.iter().map(|token| token.start).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted, "{tokens:?}");
    }

    #[test]
    fn a_parameter_with_no_name_has_nothing_to_colour() {
        // Ruby 3.1 and 3.2 made bare `*`, `**` and `&` legal, for forwarding. There is no name, so
        // there is no token, and each of the three is its own visitor.
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
        // Two shapes, both constant: an editor asks for tokens on every keystroke. `foo.` recovers
        // into a call whose message is empty and exactly at the cursor: a zero-width token, legal
        // on the wire and invisible on the screen. `foo.()` is `call` written with no name at all,
        // and has no message span.
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
    fn a_def_with_no_name_yet_has_nothing_to_colour() {
        // The keystroke after the third character, in a file that ends there: Prism recovers a
        // `def` whose name span is empty and exactly at the cursor. It is the only shape that
        // reaches `push_span` zero-width (a call's empty message never gets that far). No
        // hand-written fixture ends in a bare `def`, so this one does.
        assert!(of("def").is_empty());
        assert_eq!(
            drawn("x = 1\ndef"),
            "\
x = 1
v
def
"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_still_answers() {
        // Prism recovers, and whatever it read is still worth colouring: a file being typed into is
        // invalid Ruby most of the time.
        let tokens = of("def f(a)\n  a\n");
        assert_eq!(tokens.len(), 3, "{tokens:?}");
        assert!(of("").is_empty());
    }

    #[test]
    fn the_legend_is_the_wire() {
        // The numbers `Kind` carries *are* the protocol: a client reads them as indices into the
        // legend the server sent at initialize. Reordering one without the other recolours every
        // token in every file, silently and consistently: the hardest kind of wrong to notice.
        assert_eq!(LEGEND.len(), 3);
        assert_eq!(LEGEND[Kind::Variable as usize], "variable");
        assert_eq!(LEGEND[Kind::Parameter as usize], "parameter");
        assert_eq!(LEGEND[Kind::Method as usize], "method");
    }

    /// A `semanticTokens/full` answer read back into absolute positions and named kinds.
    ///
    /// The wire format is deltas from the previous token: unreadable, and exactly what must be
    /// checked, since an entry off by one misplaces every colour after it. Decoding it here is the
    /// only way an assertion can be about what the user sees.
    fn decoded(answer: &serde_json::Value) -> Vec<(u64, u64, u64, &'static str)> {
        let data = answer["data"].as_array().expect("token data");
        let mut rows = Vec::new();
        let (mut line, mut start) = (0, 0);
        for token in data.chunks(5) {
            let numbers: Vec<u64> = token
                .iter()
                .map(|n| n.as_u64().unwrap_or_default())
                .collect();
            line += numbers[0];
            start = if numbers[0] == 0 {
                start + numbers[1]
            } else {
                numbers[1]
            };
            rows.push((
                line,
                start,
                numbers[2],
                *tokens::LEGEND
                    .get(numbers[3] as usize)
                    .expect("a type in the legend"),
            ));
        }
        rows
    }

    #[test]
    fn semantic_tokens_arrive_as_deltas_from_the_token_before() {
        let mut harness = Harness::new();
        let source = "def render(scale)\n  size = scale\n  size\nend\n";
        let uri = harness.write("app/big.rb", source);
        harness.index();
        harness.open(&uri, source);

        let answer = harness.ask(
            "textDocument/semanticTokens/full",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );

        assert_eq!(
            decoded(&answer),
            vec![
                (0, 4, 6, "method"),
                (0, 11, 5, "parameter"),
                (1, 2, 4, "variable"),
                (1, 9, 5, "variable"),
                (2, 2, 4, "variable"),
            ]
        );
    }

    #[test]
    fn a_token_length_is_counted_in_the_encoding_the_client_negotiated() {
        // `имя` is a legal Ruby local: three characters of two bytes each. A length taken as
        // `end - start` in bytes underlines six units where the client counts three, painting the
        // colour over whatever follows. The offsets go through `TextDocument` for exactly this
        // reason, and an all-ASCII fixture cannot see it.
        let mut harness = Harness::new();
        let source = "имя = 1\nимя\n";
        let uri = harness.write("app/utf.rb", source);
        harness.index();
        harness.open(&uri, source);

        let answer = harness.ask(
            "textDocument/semanticTokens/full",
            serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
        );

        assert_eq!(
            decoded(&answer),
            vec![(0, 0, 3, "variable"), (1, 0, 3, "variable")],
            "three UTF-16 code units, not six bytes"
        );
    }
}
