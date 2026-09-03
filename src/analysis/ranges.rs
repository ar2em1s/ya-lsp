//! What a file's *shape* is: what expanding the selection reaches, and what folds.
//!
//! # Why the two live together
//!
//! Both are pure functions of one buffer, both walk Prism outwards from something, and neither
//! ever asks the graph a question — so the fourth direct use of Prism, after `cursor`, `requires`
//! and `scopes`, is one module rather than two.
//!
//! # What each replaces
//!
//! Expand-selection has no fallback at all: in a Ruby file the command does nothing today, so
//! anything correct is an improvement. Folding does have one, and a decent one — VS Code guesses
//! from indentation, and on well-formatted Ruby it guesses right most of the time. What it cannot
//! see is what this is for: a heredoc's body, a literal whose closing bracket is outdented,
//! `if`/`elsif`/`else` as three regions rather than one, a run of comment lines, and `#region`
//! markers, which an indentation guess has no concept of at all.
//!
//! # `end` stays on screen
//!
//! A collapsed range hides the lines *after* its first, so a `def` whose range ended on its `end`
//! keyword would hide the keyword, and a folded `def foo` with nothing closing it reads as broken
//! code rather than as folded code. Every range here therefore ends on the last line of the
//! construct's **body**, never on its closer — which is also why a single-line construct produces
//! no range at all: `def foo; end` would hide nothing and leave a chevron that does nothing when
//! clicked.
//!
//! # Locations that do not nest
//!
//! A selection chain is *defined* by every link containing the one before it, and Prism's error
//! recovery hands out locations that do not — the trap [`locator::spans`] exists for, and
//! [`locator::nests`] is the one predicate both of them ask. Half-written code is the normal state
//! of a buffer, so the chain is built by filtering rather than by trusting the parser, and every
//! span is clamped to the buffer before it is measured.
//!
//! # What is a step here, and what is not
//!
//! The steps this adds are the ones *Ruby* has and a generic walk misses: a string's contents
//! before its quotes, one argument before the argument list, a body before the construct that
//! opens it, and a message and its receiver before the next call in a chain. What it deliberately
//! does not add is the name in an assignment — `value` inside `value = 1`. That is not a step a
//! generic walk misses; it is exactly what a word-based one finds, every client that has an
//! expand-selection command already merges such a provider in, and Prism spells it across twenty
//! node types with no accessor in common. Steps that are free elsewhere are not worth a hundred
//! lines here.
//!
//! # Prism's visitor has thirteen holes, and they are not obscure ones
//!
//! `Visit::visit` announces each node through `visit_branch_node_enter` before dispatching, which
//! is the generic hook the selection walk is built on. But thirteen node kinds are reached by
//! their *typed* method instead — `visit_arguments_node`, `visit_statements_node` and eleven
//! more — and for those the hook never fires. Those first two are "one argument before the whole
//! argument list" and "a block's body before the block", which is to say the two steps the item
//! this module exists for names by hand. Twelve are overridden below to announce themselves and
//! then defer, so the walk underneath stays Prism's own. The thirteenth is `BlockArgumentNode`,
//! which arrives that way only from an index assignment carrying a block — `a[&b] = 1`, which
//! Ruby's own parser rejects — so it is left alone rather than written and never run.

use lsp_types::{FoldingRange, FoldingRangeKind, SelectionRange};
use ruby_prism::{
    ArgumentsNode, ArrayNode, BeginNode, BlockNode, BlockParameterNode, CallNode, CaseMatchNode,
    CaseNode, ClassNode, ConstantPathNode, DefNode, ElseNode, EnsureNode, ForNode, HashNode,
    IfNode, InNode, InterpolatedStringNode, InterpolatedSymbolNode, InterpolatedXStringNode,
    LambdaNode, LocalVariableTargetNode, Location, ModuleNode, Node, NodeList, ParametersNode,
    ParseResult, RegularExpressionNode, RescueNode, SingletonClassNode, SplatNode, StatementsNode,
    StringNode, SymbolNode, UnlessNode, UntilNode, Visit, WhenNode, WhileNode, XStringNode,
};

use super::{locator, position::TextDocument};

// ---------------------------------------------------------------------------
// selectionRange
// ---------------------------------------------------------------------------

/// The chain at `offset` in the shape the protocol wants it: innermost first, each link
/// carrying the one it expands into.
///
/// The outermost link is always the buffer itself, which is what makes this total — the seed
/// below is that link rather than a special case, and a position Prism could not place anywhere
/// still answers with the file.
#[must_use]
pub fn selection_range(text: &TextDocument, offset: u32) -> SelectionRange {
    let spans = selection_chain(text.text(), offset);
    let mut chain = SelectionRange {
        range: text.range_at(0, text.len()),
        parent: None,
    };
    for &(start, end) in spans.iter().rev().skip(1) {
        chain = SelectionRange {
            range: text.range_at(start, end),
            parent: Some(Box::new(chain)),
        };
    }
    chain
}

/// Every span the cursor at `offset` can expand out to, innermost first.
///
/// Never empty: the buffer itself is the last link of every chain, which is also the whole answer
/// where Prism produced no node at all — past the last statement, or in the whitespace of a file
/// that is still being written. LSP asks for one chain per position and has no spelling for "not
/// this one", so having something to say for every position is a requirement rather than a
/// courtesy.
#[must_use]
pub fn selection_chain(source: &str, offset: u32) -> Vec<(u32, u32)> {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut walk = Selection {
        offset,
        found: Vec::new(),
    };
    walk.visit(&parsed.node());

    let eof = source.len() as u32;
    let mut spans: Vec<(u32, u32)> = walk
        .found
        .into_iter()
        .map(|(start, end)| (start.min(eof), end.min(eof)))
        .filter(|&(start, end)| start < end)
        .collect();
    spans.push((0, eof));

    // Widest last, ties broken by the earlier start, so the fold below only ever has to ask
    // whether the next span contains the one it kept.
    spans.sort_by_key(|&(start, end)| (end - start, start));
    spans.dedup();

    let mut chain: Vec<(u32, u32)> = Vec::new();
    for span in spans {
        // A link that does not contain the previous one is not a step out of it. Recovery
        // produces exactly that, and sending it makes the client's own walk wrong rather than
        // merely short.
        if chain.last().is_none_or(|&last| locator::nests(span, last)) {
            chain.push(span);
        }
    }
    chain
}

/// Every span containing the cursor, in the order the walk finds them.
struct Selection {
    offset: u32,
    found: Vec<(u32, u32)>,
}

impl Selection {
    /// A span is a step in the chain when the cursor is inside it or against either edge — the
    /// caret between `foo` and `.bar` belongs to both, and sorting by width picks the one the
    /// user meant to start from.
    fn take(&mut self, at: &Location<'_>) {
        self.span(at.start_offset() as u32, at.end_offset() as u32);
    }

    fn span(&mut self, start: u32, end: u32) {
        if start <= self.offset && self.offset <= end {
            self.found.push((start, end));
        }
    }

    /// What an interpolated literal has instead of a `content_loc`: everything between the
    /// delimiters, which is one or more parts and whatever text lies around them.
    fn inside(&mut self, opening: Option<&Location<'_>>, closing: Option<&Location<'_>>) {
        if let (Some(opening), Some(closing)) = (opening, closing) {
            self.span(opening.end_offset() as u32, closing.start_offset() as u32);
        }
    }
}

impl<'pr> Visit<'pr> for Selection {
    fn visit_branch_node_enter(&mut self, node: Node<'pr>) {
        self.take(&node.location());
    }

    fn visit_leaf_node_enter(&mut self, node: Node<'pr>) {
        self.take(&node.location());
    }

    // The thirteen the generic hook never sees. Each announces itself before deferring, and does
    // so unconditionally rather than only on the typed path: a kind that arrives both ways would
    // otherwise need to know which way it came, and a repeated span costs one `dedup`.
    fn visit_arguments_node(&mut self, node: &ArgumentsNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_arguments_node(self, node);
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_block_node(self, node);
    }

    fn visit_block_parameter_node(&mut self, node: &BlockParameterNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_block_parameter_node(self, node);
    }

    fn visit_constant_path_node(&mut self, node: &ConstantPathNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_constant_path_node(self, node);
    }

    fn visit_else_node(&mut self, node: &ElseNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_else_node(self, node);
    }

    fn visit_ensure_node(&mut self, node: &EnsureNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_ensure_node(self, node);
    }

    fn visit_local_variable_target_node(&mut self, node: &LocalVariableTargetNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_local_variable_target_node(self, node);
    }

    fn visit_parameters_node(&mut self, node: &ParametersNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_parameters_node(self, node);
    }

    fn visit_rescue_node(&mut self, node: &RescueNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_rescue_node(self, node);
    }

    fn visit_splat_node(&mut self, node: &SplatNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_splat_node(self, node);
    }

    fn visit_statements_node(&mut self, node: &StatementsNode<'pr>) {
        self.take(&node.location());
        ruby_prism::visit_statements_node(self, node);
    }

    // And the steps Ruby has that its node tree does not.

    /// `foo` inside `def foo(a)`, and `bar` and then `foo.bar` inside `foo.bar(1)`.
    ///
    /// The receiver step is taken only through a real call operator: `a + b` is a call too, and
    /// "the receiver through the message" there is `a +`, which is not a thing anyone meant to
    /// select.
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        self.take(&node.location());
        if let Some(message) = node.message_loc() {
            self.take(&message);
            if let (Some(receiver), Some(_)) = (node.receiver(), node.call_operator_loc()) {
                self.span(
                    receiver.location().start_offset() as u32,
                    message.end_offset() as u32,
                );
            }
        }
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        self.take(&node.location());
        self.take(&node.name_loc());
        ruby_prism::visit_def_node(self, node);
    }

    /// The contents before the quotes: pulling `hello` out of `"hello"` is the first expansion
    /// anybody reaches for, and there is no node for it.
    fn visit_string_node(&mut self, node: &StringNode<'pr>) {
        self.take(&node.location());
        if node.opening_loc().is_some() {
            self.take(&node.content_loc());
        }
        ruby_prism::visit_string_node(self, node);
    }

    fn visit_x_string_node(&mut self, node: &XStringNode<'pr>) {
        self.take(&node.location());
        self.take(&node.content_loc());
        ruby_prism::visit_x_string_node(self, node);
    }

    fn visit_regular_expression_node(&mut self, node: &RegularExpressionNode<'pr>) {
        self.take(&node.location());
        self.take(&node.content_loc());
        ruby_prism::visit_regular_expression_node(self, node);
    }

    /// `name` inside `:name`, and inside `name:` — a hash key's colon is part of the symbol, and
    /// selecting the key without it is what renaming one starts from.
    fn visit_symbol_node(&mut self, node: &SymbolNode<'pr>) {
        self.take(&node.location());
        // Only where there is punctuation to step out of: `:name` carries a leading colon and
        // a hash key `name:` a trailing one, while a `%i[]` element and the `b` in `alias b a`
        // are bare names already, whose value is the whole node.
        if let Some(value) = node
            .opening_loc()
            .or(node.closing_loc())
            .and(node.value_loc())
        {
            self.take(&value);
        }
        ruby_prism::visit_symbol_node(self, node);
    }

    fn visit_interpolated_string_node(&mut self, node: &InterpolatedStringNode<'pr>) {
        self.take(&node.location());
        self.inside(node.opening_loc().as_ref(), node.closing_loc().as_ref());
        ruby_prism::visit_interpolated_string_node(self, node);
    }

    fn visit_interpolated_symbol_node(&mut self, node: &InterpolatedSymbolNode<'pr>) {
        self.take(&node.location());
        self.inside(node.opening_loc().as_ref(), node.closing_loc().as_ref());
        ruby_prism::visit_interpolated_symbol_node(self, node);
    }

    fn visit_interpolated_x_string_node(&mut self, node: &InterpolatedXStringNode<'pr>) {
        self.take(&node.location());
        self.inside(Some(&node.opening_loc()), Some(&node.closing_loc()));
        ruby_prism::visit_interpolated_x_string_node(self, node);
    }
}

// ---------------------------------------------------------------------------
// foldingRange
// ---------------------------------------------------------------------------

/// Every region of `text` an editor may collapse, in the order they open.
#[must_use]
pub fn folds(text: &TextDocument) -> Vec<FoldingRange> {
    let parsed = ruby_prism::parse(text.text().as_bytes());
    let mut walk = Folds {
        text,
        found: Vec::new(),
    };
    walk.visit(&parsed.node());

    let mut ranges = walk.found;
    ranges.extend(comment_folds(text, &parsed));

    // One chevron per line is all an editor can draw, so two ranges opening on the same line are
    // one range and one piece of noise. The widest wins, because it is the outer construct: on
    // `xs = ys.map do |y|` the assignment and the block both open on that line, and folding the
    // assignment is what the reader clicking there meant.
    ranges.sort_by_key(|range| (range.start_line, std::cmp::Reverse(range.end_line)));
    ranges.dedup_by_key(|range| range.start_line);
    ranges
}

/// One collapsible region, or `None` where it would collapse nothing — a construct written on a
/// single line, or a comment that has no neighbour. The one place that rule is decided, so that
/// a fold coming from syntax and a fold coming from a comment cannot disagree about it.
///
/// Line-granular, deliberately: every client that matters sets `lineFoldingOnly` and discards the
/// character offsets, the ones that do not still understand a whole-line range, and a folded Ruby
/// construct is a run of whole lines under either reading.
fn line_fold(
    start_line: u32,
    end_line: u32,
    kind: Option<FoldingRangeKind>,
) -> Option<FoldingRange> {
    (end_line > start_line).then(|| FoldingRange {
        start_line,
        end_line,
        kind,
        ..FoldingRange::default()
    })
}

struct Folds<'t> {
    text: &'t TextDocument,
    found: Vec<FoldingRange>,
}

impl Folds<'_> {
    /// A construct that opens at `keyword`, folds down to the last line of `body`, and is closed
    /// by `closer`.
    ///
    /// The closer is consulted only for the one shape whose body location lies. A `def`, `class`
    /// or block with a bare `rescue` in it is given an implicit `BeginNode` for a body, and that
    /// node's location runs all the way to the *enclosing* construct's `end`, because that is the
    /// keyword closing it. Measured by the body, such a fold swallows the `end` this whole module
    /// exists to keep on screen — so an implicit begin is measured by the line above its closer
    /// instead. An explicit `begin ... end` arrives the same way and lands on the same answer,
    /// since its own `end` is the line above the outer one.
    fn body(&mut self, keyword: &Location<'_>, body: Option<Node<'_>>, closer: Option<&Location>) {
        let Some(body) = body else {
            return;
        };
        let end = match (&body, closer) {
            (Node::BeginNode { .. }, Some(closer)) => {
                self.line_of(closer.start_offset()).saturating_sub(1)
            }
            // One past the body, so the line wanted is the one its last *byte* is on: a body that
            // ends with its own newline would otherwise measure as the line below itself.
            _ => self.line_of(body.location().end_offset().saturating_sub(1)),
        };
        self.take(keyword, end);
    }

    fn statements(&mut self, keyword: &Location<'_>, body: Option<StatementsNode<'_>>) {
        self.body(keyword, body.map(|at| at.as_node()), None);
    }

    /// A `case` as a whole, measured by the line above its `end`.
    ///
    /// Not by its last branch, which is the obvious reading and the wrong one: an `else` carries
    /// the `end` keyword *inside* its own location, so folding to there hides the `end`. Each
    /// branch folds separately as well, so both the whole and the parts are on offer.
    fn case(&mut self, keyword: &Location<'_>, closer: &Location<'_>) {
        let end = self.line_of(closer.start_offset()).saturating_sub(1);
        self.take(keyword, end);
    }

    /// A literal whose delimiters are on lines of their own: fold from the opening bracket to the
    /// last element, leaving the closing one visible for the same reason `end` stays visible.
    fn literal(&mut self, opening: Option<&Location<'_>>, elements: &NodeList<'_>) {
        if let Some(opening) = opening {
            self.body(opening, elements.last(), None);
        }
    }

    /// A heredoc, which is the one literal whose body is nowhere near the expression it belongs
    /// to. `<<~SQL` opens the fold; the terminator is the closer and stays visible.
    fn heredoc(&mut self, opening: Option<&Location<'_>>, body: Option<&Location<'_>>) {
        if let (Some(opening), Some(body)) = (opening, body)
            && opening.as_slice().starts_with(b"<<")
        {
            let end = self.line_of(body.end_offset().saturating_sub(1));
            self.take(opening, end);
        }
    }

    fn take(&mut self, keyword: &Location<'_>, end_line: u32) {
        let start_line = self.line_of(keyword.start_offset());
        self.found.extend(line_fold(start_line, end_line, None));
    }

    fn line_of(&self, offset: usize) -> u32 {
        self.text.position_at(offset as u32).line
    }
}

impl<'pr> Visit<'pr> for Folds<'_> {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.body(
            &node.class_keyword_loc(),
            node.body(),
            Some(&node.end_keyword_loc()),
        );
        ruby_prism::visit_class_node(self, node);
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.body(
            &node.module_keyword_loc(),
            node.body(),
            Some(&node.end_keyword_loc()),
        );
        ruby_prism::visit_module_node(self, node);
    }

    fn visit_singleton_class_node(&mut self, node: &SingletonClassNode<'pr>) {
        self.body(
            &node.class_keyword_loc(),
            node.body(),
            Some(&node.end_keyword_loc()),
        );
        ruby_prism::visit_singleton_class_node(self, node);
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        self.body(
            &node.def_keyword_loc(),
            node.body(),
            node.end_keyword_loc().as_ref(),
        );
        ruby_prism::visit_def_node(self, node);
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.body(&node.opening_loc(), node.body(), Some(&node.closing_loc()));
        ruby_prism::visit_block_node(self, node);
    }

    fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
        self.body(&node.operator_loc(), node.body(), Some(&node.closing_loc()));
        ruby_prism::visit_lambda_node(self, node);
    }

    /// `if`, `elsif` and `else` are three regions, not one: an `elsif` arrives as an `IfNode` of
    /// its own in the first one's `subsequent`, so nothing here has to know the word.
    ///
    /// The guard is what keeps a modifier (`x if y`) and a ternary out — both are `IfNode`s, and
    /// only the statement form starts where its keyword does.
    fn visit_if_node(&mut self, node: &IfNode<'pr>) {
        if let Some(keyword) = node.if_keyword_loc()
            && keyword.start_offset() == node.location().start_offset()
        {
            self.statements(&keyword, node.statements());
        }
        ruby_prism::visit_if_node(self, node);
    }

    fn visit_unless_node(&mut self, node: &UnlessNode<'pr>) {
        let keyword = node.keyword_loc();
        if keyword.start_offset() == node.location().start_offset() {
            self.statements(&keyword, node.statements());
        }
        ruby_prism::visit_unless_node(self, node);
    }

    fn visit_else_node(&mut self, node: &ElseNode<'pr>) {
        self.statements(&node.else_keyword_loc(), node.statements());
        ruby_prism::visit_else_node(self, node);
    }

    fn visit_case_node(&mut self, node: &CaseNode<'pr>) {
        self.case(&node.case_keyword_loc(), &node.end_keyword_loc());
        ruby_prism::visit_case_node(self, node);
    }

    fn visit_case_match_node(&mut self, node: &CaseMatchNode<'pr>) {
        self.case(&node.case_keyword_loc(), &node.end_keyword_loc());
        ruby_prism::visit_case_match_node(self, node);
    }

    fn visit_when_node(&mut self, node: &WhenNode<'pr>) {
        self.statements(&node.keyword_loc(), node.statements());
        ruby_prism::visit_when_node(self, node);
    }

    fn visit_in_node(&mut self, node: &InNode<'pr>) {
        self.statements(&node.in_loc(), node.statements());
        ruby_prism::visit_in_node(self, node);
    }

    fn visit_while_node(&mut self, node: &WhileNode<'pr>) {
        let keyword = node.keyword_loc();
        if keyword.start_offset() == node.location().start_offset() {
            self.statements(&keyword, node.statements());
        }
        ruby_prism::visit_while_node(self, node);
    }

    fn visit_until_node(&mut self, node: &UntilNode<'pr>) {
        let keyword = node.keyword_loc();
        if keyword.start_offset() == node.location().start_offset() {
            self.statements(&keyword, node.statements());
        }
        ruby_prism::visit_until_node(self, node);
    }

    fn visit_for_node(&mut self, node: &ForNode<'pr>) {
        self.statements(&node.for_keyword_loc(), node.statements());
        ruby_prism::visit_for_node(self, node);
    }

    /// Only a `begin` the user wrote. A `def` with a `rescue` in it carries an implicit
    /// `BeginNode` with no keyword, and the `def`'s own fold already covers those lines.
    fn visit_begin_node(&mut self, node: &BeginNode<'pr>) {
        if let Some(keyword) = node.begin_keyword_loc() {
            self.statements(&keyword, node.statements());
        }
        ruby_prism::visit_begin_node(self, node);
    }

    fn visit_rescue_node(&mut self, node: &RescueNode<'pr>) {
        self.statements(&node.keyword_loc(), node.statements());
        ruby_prism::visit_rescue_node(self, node);
    }

    fn visit_ensure_node(&mut self, node: &EnsureNode<'pr>) {
        self.statements(&node.ensure_keyword_loc(), node.statements());
        ruby_prism::visit_ensure_node(self, node);
    }

    /// A call whose arguments run past the line the *message* is on — `foo(\n  a,\n  b\n)`, and
    /// the paren-less `validates :name,\n  presence: true` with it.
    ///
    /// Here because advertising the provider *replaces* the editor's indentation guess rather
    /// than adding to it, and a multi-line argument list is the commonest thing that guess folds
    /// in Rails code. Ends at the last argument, so a closing paren on its own line stays visible
    /// like every other closer.
    ///
    /// Measured from the message and not from the call, which is where the receiver is: in a
    /// chain written down the page, `store.constants\n  .collect(gates)` is one call whose start
    /// is two lines above its own name, and folding from there hides a line of the chain for no
    /// reason anybody looking at it would recognise.
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if let Some(arguments) = node.arguments() {
            let opens = node.message_loc().unwrap_or_else(|| node.location());
            self.body(&opens, arguments.arguments().last(), None);
        }
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_array_node(&mut self, node: &ArrayNode<'pr>) {
        self.literal(node.opening_loc().as_ref(), &node.elements());
        ruby_prism::visit_array_node(self, node);
    }

    fn visit_hash_node(&mut self, node: &HashNode<'pr>) {
        self.literal(Some(&node.opening_loc()), &node.elements());
        ruby_prism::visit_hash_node(self, node);
    }

    fn visit_string_node(&mut self, node: &StringNode<'pr>) {
        self.heredoc(node.opening_loc().as_ref(), Some(&node.content_loc()));
        ruby_prism::visit_string_node(self, node);
    }

    /// An interpolated heredoc has no `content_loc` — its body is its parts, and the last of them
    /// ends on the last line before the terminator.
    fn visit_interpolated_string_node(&mut self, node: &InterpolatedStringNode<'pr>) {
        let last = node.parts().last().map(|at| at.location());
        self.heredoc(node.opening_loc().as_ref(), last.as_ref());
        ruby_prism::visit_interpolated_string_node(self, node);
    }
}

// ---------------------------------------------------------------------------
// The folds that are comments rather than syntax
// ---------------------------------------------------------------------------

/// `#region` and `#endregion`, the one folding convention that lives in a comment.
#[derive(Debug, Clone, Copy)]
enum Marker {
    Start,
    End,
}

/// Runs of whole-line comments, and the regions markers delimit.
///
/// A comment that follows code on the same line is that line's tail rather than a line of its
/// own, so it neither joins a run nor breaks one: `x = 1 # why` between two commented lines would
/// otherwise cut the block in half.
fn comment_folds(text: &TextDocument, parsed: &ParseResult<'_>) -> Vec<FoldingRange> {
    let source = text.text();
    let mut ranges = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    let mut regions: Vec<u32> = Vec::new();

    for comment in parsed.comments() {
        let at = comment.location();
        let (start, end) = (at.start_offset(), at.end_offset());
        if !starts_its_line(source, start) {
            continue;
        }
        let first = text.position_at(start as u32).line;
        let last = text.position_at((end as u32).saturating_sub(1)).line;

        match marker(&source[start..end]) {
            Some(Marker::Start) => {
                close_run(&mut run, &mut ranges);
                regions.push(first);
            }
            Some(Marker::End) => {
                close_run(&mut run, &mut ranges);
                // An unmatched `#endregion` is a typo, and so is an unmatched `#region` — which
                // is why an open one left over at the end of the file is dropped rather than
                // folded to EOF. Collapsing the rest of a file over a typo is worse than not
                // collapsing anything.
                if let Some(opened) = regions.pop() {
                    ranges.extend(line_fold(opened, first, Some(FoldingRangeKind::Region)));
                }
            }
            // `=begin`/`=end` arrives as one comment several lines tall, so it opens a run and
            // closes it by itself.
            None => match run {
                Some((from, to)) if to + 1 == first => run = Some((from, last)),
                _ => {
                    close_run(&mut run, &mut ranges);
                    run = Some((first, last));
                }
            },
        }
    }
    close_run(&mut run, &mut ranges);
    ranges
}

fn close_run(run: &mut Option<(u32, u32)>, ranges: &mut Vec<FoldingRange>) {
    if let Some((from, to)) = run.take() {
        ranges.extend(line_fold(from, to, Some(FoldingRangeKind::Comment)));
    }
}

/// Whether nothing but whitespace precedes `start` on its line.
fn starts_its_line(source: &str, start: usize) -> bool {
    let head = &source[..start];
    let (_, prefix) = head.rsplit_once('\n').unwrap_or(("", head));
    prefix.trim().is_empty()
}

/// A region marker, written with or without a space after the `#` and with or without a label
/// after the word — the two spellings every editor that supports these accepts.
fn marker(comment: &str) -> Option<Marker> {
    let rest = comment.strip_prefix('#')?.trim_start();
    for (word, marker) in [("endregion", Marker::End), ("region", Marker::Start)] {
        if let Some(tail) = rest.strip_prefix(word)
            && !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_')
        {
            return Some(marker);
        }
    }
    None
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::position::PositionEncoding;

    /// Every fold drawn down the left of the file it was computed from: `┌` where one opens, `│`
    /// through each line it hides, and `┘` on the last line it hides. A comment run opens with a
    /// `c` and a region with an `r`, since for those two the *kind* is part of the answer.
    ///
    /// Drawn rather than asserted as line numbers for the reason the highlight map is: the rule
    /// this module is likeliest to break is "the closer stays visible", and here that is a thing
    /// you can see — the line holding `end` carries no mark — where `assert_eq!(end_line, 4)` can
    /// only be checked against the same arithmetic that produced it. Source and drawing are both
    /// written a line at a time so that the two line up in the file the way they do on screen.
    fn drawn(source: &[&str]) -> String {
        let text = TextDocument::new(source.join("\n") + "\n", PositionEncoding::Utf16);
        let found = folds(&text);

        // One lane per fold, reused left to right once the fold holding it has closed, so nesting
        // reads as indentation and two unrelated folds do not each claim a column of their own.
        let mut lanes: Vec<Vec<&FoldingRange>> = Vec::new();
        for range in &found {
            let free = lanes
                .iter()
                .position(|lane| lane[lane.len() - 1].end_line < range.start_line);
            match free {
                Some(lane) => lanes[lane].push(range),
                None => lanes.push(vec![range]),
            }
        }

        let mut drawn = Vec::new();
        for (number, line) in source.iter().enumerate() {
            let number = number as u32;
            let gutter: String = lanes
                .iter()
                .map(|lane| {
                    lane.iter()
                        .find_map(|range| mark(range, number))
                        .unwrap_or(' ')
                })
                .collect();
            drawn.push(format!("{gutter} {line}").trim_end().to_owned());
        }
        drawn.join("\n")
    }

    fn mark(range: &FoldingRange, line: u32) -> Option<char> {
        if range.start_line == line {
            return Some(match range.kind {
                Some(FoldingRangeKind::Comment) => 'c',
                Some(FoldingRangeKind::Region) => 'r',
                _ => '┌',
            });
        }
        if range.end_line == line {
            return Some('┘');
        }
        (range.start_line < line && line < range.end_line).then_some('│')
    }

    fn rows(rows: &[&str]) -> String {
        rows.join("\n")
    }

    /// The text each link of the chain covers, innermost first, with newlines shown so that a
    /// multi-line link stays one line of the assertion.
    fn chain(marked: &str) -> String {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        let source = marked.replace('~', "");
        selection_chain(&source, offset)
            .into_iter()
            .map(|(start, end)| source[start as usize..end as usize].replace('\n', "⏎"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -------------------------------------------------------------- folding

    #[test]
    fn every_definition_folds_and_every_closer_survives_it() {
        // Six constructs nested six deep, and the whole rule of the module visible in one
        // picture: not one of the five `end` keywords carries a mark, because a collapsed
        // `def build` with nothing closing it reads as broken code rather than as folded code.
        assert_eq!(
            drawn(&[
                "module Shop",
                "  class Order",
                "    class << self",
                "      def build(rows)",
                "        rows.map do |row|",
                "          row.to_h",
                "        end",
                "      end",
                "    end",
                "  end",
                "end",
            ]),
            rows(&[
                "┌     module Shop",
                "│┌      class Order",
                "││┌       class << self",
                "│││┌        def build(rows)",
                "││││┌         rows.map do |row|",
                "││││┘           row.to_h",
                "│││┘          end",
                "││┘         end",
                "│┘        end",
                "┘       end",
                "      end",
            ])
        );
    }

    #[test]
    fn a_branch_is_a_region_of_its_own() {
        // `if`, `elsif` and `else` fold as three rather than one, which is what the editor's
        // indentation guess already does and what anyone reading a long conditional wants. An
        // `elsif` arrives as an `IfNode` inside the first one's `subsequent`, so nothing in this
        // module knows that word — and nothing knows `when` either.
        assert_eq!(
            drawn(&[
                "if a", "  one", "elsif b", "  two", "else", "  three", "end", "case x", "when 1",
                "  one", "else", "  other", "end",
            ]),
            rows(&[
                "┌  if a",
                "┘    one",
                "┌  elsif b",
                "┘    two",
                "┌  else",
                "┘    three",
                "   end",
                "┌  case x",
                "│┌ when 1",
                "│┘   one",
                "│┌ else",
                "┘┘   other",
                "   end",
            ])
        );
    }

    #[test]
    fn loops_and_the_rescue_family() {
        assert_eq!(
            drawn(&[
                "begin",
                "  while a",
                "    step",
                "  end",
                "rescue Boom => e",
                "  log e",
                "else",
                "  done",
                "ensure",
                "  close",
                "end",
            ]),
            rows(&[
                "┌  begin",
                "│┌   while a",
                "│┘     step",
                "┘    end",
                "┌  rescue Boom => e",
                "┘    log e",
                "┌  else",
                "┘    done",
                "┌  ensure",
                "┘    close",
                "   end",
            ])
        );
    }

    #[test]
    fn for_until_unless_and_a_pattern_match() {
        // A `case` folds as a whole *and* as its branches; the whole one ends at the last branch
        // rather than at `end`, which is why the last two lines look the way they do.
        assert_eq!(
            drawn(&[
                "for i in list",
                "  i",
                "end",
                "until done",
                "  step",
                "end",
                "unless ok",
                "  raise",
                "end",
                "case value",
                "in [a]",
                "  a",
                "else",
                "  b",
                "end",
            ]),
            rows(&[
                "┌  for i in list",
                "┘    i",
                "   end",
                "┌  until done",
                "┘    step",
                "   end",
                "┌  unless ok",
                "┘    raise",
                "   end",
                "┌  case value",
                "│┌ in [a]",
                "│┘   a",
                "│┌ else",
                "┘┘   b",
                "   end",
            ])
        );
    }

    #[test]
    fn a_lambda_a_brace_block_and_a_block_on_super() {
        // `super do ... end` is the one place Prism hands a block to its typed visitor rather
        // than through `visit`, so it is in the fixture to keep that override honest.
        assert_eq!(
            drawn(&[
                "run = ->(a) {",
                "  a",
                "}",
                "list.each { |a|",
                "  a",
                "}",
                "def call",
                "  super do",
                "    1",
                "  end",
                "end",
            ]),
            rows(&[
                "┌  run = ->(a) {",
                "┘    a",
                "   }",
                "┌  list.each { |a|",
                "┘    a",
                "   }",
                "┌  def call",
                "│┌   super do",
                "│┘     1",
                "┘    end",
                "   end",
            ])
        );
    }

    #[test]
    fn two_constructs_opening_on_one_line_leave_one_chevron() {
        // The setter's argument list and the block both open on the first line, and an editor
        // can only draw one chevron there. The wider is the one to keep: it is the outer
        // construct, and it is what a reader clicking that line meant to collapse.
        assert_eq!(
            drawn(&["obj.items = list.map do |x|", "  x", "end"]),
            rows(&["┌ obj.items = list.map do |x|", "│   x", "┘ end"])
        );
    }

    #[test]
    fn literals_a_heredoc_and_an_argument_list() {
        // The four an indentation guess gets wrong or cannot see at all. A heredoc is the one
        // literal whose body is nowhere near the expression it belongs to, and `%w[]` is here
        // because its elements are strings with no quotes of their own — the shape that decides
        // whether the heredoc test is asking about the right location.
        assert_eq!(
            drawn(&[
                "ROWS = [",
                "  1,",
                "  2,",
                "]",
                "OPTS = {",
                "  a: 1,",
                "}",
                "sql = <<~SQL",
                "  select 1",
                "SQL",
                "validates :name,",
                "  presence: true",
                "words = %w[a b]",
            ]),
            rows(&[
                "┌ ROWS = [",
                "│   1,",
                "┘   2,",
                "  ]",
                "┌ OPTS = {",
                "┘   a: 1,",
                "  }",
                "┌ sql = <<~SQL",
                "┘   select 1",
                "  SQL",
                "┌ validates :name,",
                "┘   presence: true",
                "  words = %w[a b]",
            ])
        );
    }

    #[test]
    fn an_interpolated_heredoc_ends_where_its_last_part_does() {
        assert_eq!(
            drawn(&["sql = <<~SQL", "  select #{id}", "  from t", "SQL"]),
            rows(&["┌ sql = <<~SQL", "│   select #{id}", "┘   from t", "  SQL",])
        );
    }

    #[test]
    fn a_single_line_construct_folds_nothing() {
        // A range whose two lines are the same renders a chevron that does nothing when it is
        // clicked, which is worse than no chevron at all. Every line below holds a construct this
        // module otherwise recognises, and the whole answer is the empty gutter.
        assert_eq!(
            drawn(&[
                "def foo; end",
                "x = [1, 2]",
                "n = a.(1)",
                "y = if a then b else c end",
                "t = a ? b : c",
                "z = 1 if a",
                "w = 2 while a",
                "v = 3 unless a",
                "u = 4 until a",
                "s = 1, 2",
            ]),
            rows(&[
                " def foo; end",
                " x = [1, 2]",
                " n = a.(1)",
                " y = if a then b else c end",
                " t = a ? b : c",
                " z = 1 if a",
                " w = 2 while a",
                " v = 3 unless a",
                " u = 4 until a",
                " s = 1, 2",
            ])
        );
    }

    #[test]
    fn comment_runs_regions_and_the_lines_that_are_neither() {
        // Four rules in one picture: a run needs a second line, a comment that follows code is
        // that line's tail rather than a line of its own, `# regional` is a word that merely
        // starts like a marker, and a real marker ends whatever run it interrupts and opens a
        // fold the syntax knows nothing about.
        assert_eq!(
            drawn(&[
                "# one",
                "# two",
                "x = 1 # not part of it",
                "# alone",
                "y = 2",
                "# regional",
                "# accent",
                "# region setup",
                "z = 3",
                "# endregion",
            ]),
            rows(&[
                "c # one",
                "┘ # two",
                "  x = 1 # not part of it",
                "  # alone",
                "  y = 2",
                "c # regional",
                "┘ # accent",
                "r # region setup",
                "│ z = 3",
                "┘ # endregion",
            ])
        );
    }

    #[test]
    fn an_embdoc_is_a_comment_several_lines_tall() {
        assert_eq!(
            drawn(&["=begin", "prose", "=end", "x = 1"]),
            rows(&["c =begin", "│ prose", "┘ =end", "  x = 1"])
        );
    }

    #[test]
    fn an_unmatched_region_marker_folds_nothing() {
        // Both directions of the same typo. Collapsing the rest of a file because somebody forgot
        // an `#endregion` is a worse answer than leaving those lines alone.
        assert_eq!(
            drawn(&["# region open", "x = 1", "y = 2"]),
            rows(&[" # region open", " x = 1", " y = 2"])
        );
        assert_eq!(
            drawn(&["x = 1", "# endregion", "y = 2"]),
            rows(&[" x = 1", " # endregion", " y = 2"])
        );
    }

    #[test]
    fn a_rescue_the_user_did_not_open_a_begin_for_folds_once() {
        // Prism gives a `def` with a `rescue` in it an implicit `BeginNode` with no keyword. The
        // `def`'s own fold already covers those lines, so a second one opening on the same line
        // would be a duplicate chevron sitting on top of the first.
        assert_eq!(
            drawn(&["def f", "  1", "rescue", "  2", "end"]),
            rows(&["┌  def f", "│    1", "│┌ rescue", "┘┘   2", "   end"])
        );
    }

    #[test]
    fn a_half_typed_buffer_still_folds_what_it_has() {
        // The normal state of a file. Prism recovers, the `class` still has a body, and the folds
        // are the ones the finished file would have had.
        assert_eq!(
            drawn(&["class Foo", "  def bar", "    x"]),
            rows(&["┌  class Foo", "│┌   def bar", "┘┘     x"])
        );
    }

    #[test]
    fn an_empty_buffer_folds_nothing() {
        assert!(folds(&TextDocument::new(String::new(), PositionEncoding::Utf16)).is_empty());
    }

    // ------------------------------------------------------------ selection

    #[test]
    fn the_contents_of_a_string_come_before_its_quotes() {
        // The first step anybody expands to, and the one Prism has no node for.
        assert_eq!(
            chain("puts \"hello ~world\"\n"),
            rows(&[
                "hello world",
                "\"hello world\"",
                "puts \"hello world\"",
                "puts \"hello world\"⏎",
            ])
        );
    }

    #[test]
    fn one_argument_comes_before_the_whole_argument_list() {
        // `ArgumentsNode` is one of the thirteen Prism reaches through its typed visitor, so
        // without that override this chain jumps straight from `2` to the whole call.
        assert_eq!(
            chain("total(1, ~2, 3)\n"),
            rows(&["2", "1, 2, 3", "total(1, 2, 3)", "total(1, 2, 3)⏎"])
        );
    }

    #[test]
    fn a_body_comes_before_the_def_and_the_def_before_the_class() {
        assert_eq!(
            chain("class Foo\n  def bar(a)\n    a~ + 1\n  end\nend\n"),
            rows(&[
                "a",
                "a + 1",
                "def bar(a)⏎    a + 1⏎  end",
                "class Foo⏎  def bar(a)⏎    a + 1⏎  end⏎end",
                "class Foo⏎  def bar(a)⏎    a + 1⏎  end⏎end⏎",
            ])
        );
    }

    #[test]
    fn a_message_comes_before_its_receiver_and_the_call_before_the_next_one() {
        // The step a generic walk misses in the shape Ruby writes most: `a.b.c` nests as
        // `((a.b).c)`, so the receiver of the outer call is the whole inner one and there is no
        // node spelling `order.line_items` on its own.
        assert_eq!(
            chain("order.li~ne_items.first\n"),
            rows(&[
                "line_items",
                "order.line_items",
                "order.line_items.first",
                "order.line_items.first⏎",
            ])
        );
    }

    #[test]
    fn the_buffer_is_the_answer_where_there_is_nothing_else() {
        // Past the last statement there is no node to start from, and the file is the answer
        // rather than an empty array: the protocol pairs chains to positions by index, so every
        // position has to have one.
        assert_eq!(chain("x = 1\n\n~"), "x = 1⏎⏎");
        assert_eq!(chain("~"), "");
    }

    #[test]
    fn every_link_contains_the_one_before_it_however_the_buffer_parses() {
        // The invariant the protocol *defines* the response by, swept over a file being typed one
        // character at a time — which is where Prism's recovery hands out the locations that do
        // not nest. `locator::nests` is the predicate; this is what says it is asked everywhere.
        let finished = "class Foo\n  def bar(a)\n    a + [1, \"two\"]\n  end\nend\n";
        for typed in 1..=finished.len() {
            let Some(source) = finished.get(..typed) else {
                continue;
            };
            for offset in 0..=source.len() as u32 {
                let chain = selection_chain(source, offset);
                for pair in chain.windows(2) {
                    assert!(
                        locator::nests(pair[1], pair[0]),
                        "{:?} does not contain {:?} at {offset} in {source:?}",
                        pair[1],
                        pair[0]
                    );
                }
                assert_eq!(
                    chain.last(),
                    Some(&(0, source.len() as u32)),
                    "the buffer is not the last link at {offset} in {source:?}"
                );
            }
        }
    }

    #[test]
    fn the_awkward_literals_each_offer_their_contents() {
        // Four spellings that all need a step Prism has no node for, and one — the interpolated
        // string — where the step is what lies between the delimiters rather than a `content`.
        assert_eq!(chain("x = :na~me\n").lines().next(), Some("name"));
        assert_eq!(chain("x = /ab~c/\n").lines().next(), Some("abc"));
        assert_eq!(chain("x = `ec~ho`\n").lines().next(), Some("echo"));
        assert_eq!(
            chain("x = \"a#{b~}c\"\n").lines().last(),
            Some("x = \"a#{b}c\"⏎")
        );
        assert_eq!(
            chain("x = :\"a#{b~}c\"\n").lines().last(),
            Some("x = :\"a#{b}c\"⏎")
        );
        assert_eq!(
            chain("x = `a#{b~}c`\n").lines().last(),
            Some("x = `a#{b}c`⏎")
        );
        assert_eq!(chain("h = { na~me: 1 }\n").lines().next(), Some("name"));
        // A symbol already spelled as a bare name has no punctuation to step out of.
        assert_eq!(chain("x = %i[a~ b]\n").lines().next(), Some("a"));
        // Two adjacent literals are one interpolated string with no delimiters of its own.
        assert_eq!(
            chain("x = \"a\" \"b#{c~}\"\n").lines().last(),
            Some("x = \"a\" \"b#{c}\"⏎")
        );
        // A call with no message at all: `a.(1)` is `a.call(1)` spelled without the name.
        assert_eq!(chain("x = a.(1~)\n").lines().next(), Some("1"));
    }

    #[test]
    fn the_typed_visitor_kinds_are_each_reached() {
        // The rest of the thirteen, each in the one shape that arrives through its typed visitor
        // rather than through `visit`: a constant path being assigned, a `super` with a block, a
        // named capture, a find pattern, a capture pattern, and a block parameter.
        for marked in [
            "Foo::B~ar = 1\n",
            "def f\n  super do\n    1~\n  end\nend\n",
            "/(?<a>x)/ =~ s~\n",
            "case x\nin [*, 1~, *]\nend\n",
            "case x\nin Integer => n~\nend\n",
            "def f(&b~)\nend\n",
            "def f(a~)\nend\n",
            "begin\n  1\nrescue\n  2~\nelse\n  3\nensure\n  4\nend\n",
        ] {
            assert!(!chain(marked).is_empty(), "no chain for {marked:?}");
        }
    }
}
