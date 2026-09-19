//! What a file's *shape* is: what expanding the selection reaches, and what folds.
//!
//! Both are pure functions of one buffer, both walk Prism outwards from something, and neither asks
//! the graph. So they share one module.
//!
//! Why each exists:
//! - **Expand-selection** has no fallback: in a Ruby file the command does nothing without a
//!   provider.
//! - **Folding** has a decent fallback: VS Code guesses from indentation. This covers what that
//!   guess cannot see: a heredoc's body, a literal whose closing bracket is outdented,
//!   `if`/`elsif`/`else` as three regions, a run of comment lines, and `#region` markers.
//!
//! # `end` stays on screen
//!
//! A collapsed range hides the lines *after* its first. A `def` whose range ended on its `end`
//! would hide the keyword, and a folded `def foo` with nothing closing it reads as broken code.
//!
//! So every range ends on the last line of the construct's **body**, never on its closer. That is
//! also why a single-line construct gets no range: `def foo; end` would hide nothing and show a
//! chevron that does nothing.
//!
//! # Locations that do not nest
//!
//! A selection chain is *defined* by every link containing the one before it. Prism's error
//! recovery hands out locations that do not nest: the trap [`locator::spans`] exists for, and
//! [`locator::nests`] is the one predicate both ask. Half-written code is the normal state of a
//! buffer, so:
//! - the chain is built by filtering, not by trusting the parser;
//! - every span is clamped to the buffer before it is measured.
//!
//! # What is a step here, and what is not
//!
//! Added: the steps *Ruby* has and a generic walk misses:
//! - a string's contents before its quotes;
//! - one argument before the argument list;
//! - a body before the construct that opens it;
//! - a message and its receiver before the next call in a chain.
//!
//! Not added: the name in an assignment (`value` inside `value = 1`). A word-based provider finds
//! it, every client merges one in already, and Prism spells it across twenty node types with no
//! common accessor.
//!
//! # Prism's visitor has thirteen holes
//!
//! `Visit::visit` announces each node through `visit_branch_node_enter` before dispatching; the
//! selection walk is built on that hook. Thirteen node kinds are reached by their *typed* method
//! instead (`visit_arguments_node`, `visit_statements_node` and eleven more), and for those the
//! hook never fires. The first two carry two of this module's steps: one argument before the list,
//! a block's body before the block.
//!
//! Twelve are overridden below to announce themselves and then defer, so the walk underneath stays
//! Prism's own. The thirteenth, `BlockArgumentNode`, arrives that way only from an index assignment
//! carrying a block (`a[&b] = 1`), which Ruby's own parser rejects. It is left alone rather than
//! written and never run.

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

/// The chain at `offset` in the shape the protocol wants: innermost first, each link carrying the
/// one it expands into.
///
/// The outermost link is always the buffer itself. That makes this total: a position Prism could
/// not place still answers with the file.
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
/// Never empty: the buffer is the last link of every chain. It is the whole answer where Prism
/// produced no node: past the last statement, or in the whitespace of a file being written. LSP
/// asks for one chain per position and cannot say "not this one", so an answer for every position
/// is required.
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

    // Widest last, ties broken by the earlier start. The fold below then only asks whether the next
    // span contains the one it kept.
    spans.sort_by_key(|&(start, end)| (end - start, start));
    spans.dedup();

    let mut chain: Vec<(u32, u32)> = Vec::new();
    for span in spans {
        // A link that does not contain the previous one is not a step out of it. Recovery produces
        // exactly that, and sending it makes the client's walk wrong, not merely short.
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
    /// A span is a step when the cursor is inside it or against either edge. The caret between
    /// `foo` and `.bar` belongs to both; sorting by width picks the one the user meant.
    fn take(&mut self, at: &Location<'_>) {
        self.span(at.start_offset() as u32, at.end_offset() as u32);
    }

    fn span(&mut self, start: u32, end: u32) {
        if start <= self.offset && self.offset <= end {
            self.found.push((start, end));
        }
    }

    /// What an interpolated literal has instead of a `content_loc`: everything between the
    /// delimiters, meaning its parts and the text around them.
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

    // The thirteen the generic hook never sees. Each announces itself before deferring, always, not
    // only on the typed path: a kind that arrives both ways would otherwise need to know how it
    // came, and a repeated span costs one `dedup`.
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

    /// `foo` inside `def foo(a)`, and `bar` then `foo.bar` inside `foo.bar(1)`.
    ///
    /// The receiver step needs a real call operator. `a + b` is a call too, and "receiver through
    /// message" there is `a +`, which nobody means to select.
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

    /// The contents before the quotes. Pulling `hello` out of `"hello"` is the first expansion
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

    /// `name` inside `:name` and inside `name:`. A hash key's colon is part of the symbol, and
    /// renaming the key starts from selecting it without the colon.
    fn visit_symbol_node(&mut self, node: &SymbolNode<'pr>) {
        self.take(&node.location());
        // Only where there is punctuation to step out of: `:name` has a leading colon, a hash key
        // `name:` a trailing one. A `%i[]` element and the `b` in `alias b a` are bare names
        // already.
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

    // An editor draws one chevron per line, so two ranges opening on one line are one range plus
    // noise. The widest wins, being the outer construct: on `xs = ys.map do |y|` the assignment and
    // the block both open there, and the reader clicking meant the assignment.
    ranges.sort_by_key(|range| (range.start_line, std::cmp::Reverse(range.end_line)));
    ranges.dedup_by_key(|range| range.start_line);
    ranges
}

/// One collapsible region, or `None` where it would collapse nothing: a single-line construct, or a
/// lone comment. The one place that rule is decided, so syntax folds and comment folds cannot
/// disagree.
///
/// Line-granular on purpose: every client that matters sets `lineFoldingOnly` and drops character
/// offsets, the others still understand whole-line ranges, and a folded Ruby construct is whole
/// lines either way.
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
    /// A construct that opens at `keyword`, folds down to the last line of `body`, and is closed by
    /// `closer`.
    ///
    /// The closer matters for one shape only, where the body's location lies. A `def`, `class` or
    /// block with a bare `rescue` gets an implicit `BeginNode` body, and its location runs to the
    /// *enclosing* `end`. Measured by the body, the fold would swallow that `end`. So an implicit
    /// begin is measured by the line above its closer. An explicit `begin ... end` lands on the
    /// same answer, since its own `end` is the line above the outer one.
    fn body(&mut self, keyword: &Location<'_>, body: Option<Node<'_>>, closer: Option<&Location>) {
        let Some(body) = body else {
            return;
        };
        let end = match (&body, closer) {
            (Node::BeginNode { .. }, Some(closer)) => {
                self.line_of(closer.start_offset()).saturating_sub(1)
            }
            // The body's end is one past its last byte, so measure the line of that last *byte*. A
            // body ending with its own newline would otherwise measure as the line below.
            _ => self.line_of(body.location().end_offset().saturating_sub(1)),
        };
        self.take(keyword, end);
    }

    fn statements(&mut self, keyword: &Location<'_>, body: Option<StatementsNode<'_>>) {
        self.body(keyword, body.map(|at| at.as_node()), None);
    }

    /// A `case` as a whole, measured by the line above its `end`.
    ///
    /// Not by its last branch, the obvious and wrong reading: an `else` carries the `end` keyword
    /// *inside* its location, so folding there hides the `end`. Each branch also folds on its own,
    /// so both the whole and the parts are on offer.
    fn case(&mut self, keyword: &Location<'_>, closer: &Location<'_>) {
        let end = self.line_of(closer.start_offset()).saturating_sub(1);
        self.take(keyword, end);
    }

    /// A literal whose delimiters sit on their own lines: fold from the opening bracket to the last
    /// element. The closing bracket stays visible, for the same reason `end` does.
    fn literal(&mut self, opening: Option<&Location<'_>>, elements: &NodeList<'_>) {
        if let Some(opening) = opening {
            self.body(opening, elements.last(), None);
        }
    }

    /// A heredoc: the one literal whose body is nowhere near its expression. `<<~SQL` opens the
    /// fold; the terminator is the closer and stays visible.
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

    /// `if`, `elsif` and `else` are three regions, not one. An `elsif` arrives as its own `IfNode`
    /// in the first one's `subsequent`, so nothing here needs to know the word.
    ///
    /// The guard keeps a modifier (`x if y`) and a ternary out: both are `IfNode`s, and only the
    /// statement form starts at its keyword.
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

    /// Only a `begin` the user wrote. A `def` with a `rescue` carries an implicit keyword-less
    /// `BeginNode`, and the `def`'s fold already covers those lines.
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

    /// A call whose arguments run past the line its *message* is on: `foo(\n  a,\n  b\n)`, and
    /// paren-less `validates :name,\n  presence: true`.
    ///
    /// Needed because advertising a provider *replaces* the editor's indentation guess, and a
    /// multi-line argument list is the commonest thing that guess folds in Rails code. Ends at the
    /// last argument, so a closing paren on its own line stays visible.
    ///
    /// Measured from the message, not the call start where the receiver is. In a chain written down
    /// the page, `store.constants\n  .collect(gates)` is one call starting two lines above its own
    /// name, and folding from there hides a chain line for no visible reason.
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

    /// An interpolated heredoc has no `content_loc`. Its body is its parts, and the last one ends
    /// on the line before the terminator.
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
/// A comment after code on the same line is that line's tail, so it neither joins a run nor breaks
/// one. Otherwise `x = 1 # why` between two commented lines would cut the block in half.
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
                // An unmatched `#endregion` is a typo, and so is an unmatched `#region`. So a
                // region still open at the end of the file is dropped, not folded to EOF:
                // collapsing the rest of a file over a typo is worse than collapsing nothing.
                if let Some(opened) = regions.pop() {
                    ranges.extend(line_fold(opened, first, Some(FoldingRangeKind::Region)));
                }
            }
            // `=begin`/`=end` arrives as one comment several lines tall, so it opens and closes a
            // run by itself.
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

/// A region marker, with or without a space after the `#` and with or without a label after the
/// word: the two spellings every editor that supports these accepts.
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
    use crate::analysis::testing::*;

    /// Every fold drawn down the left of its file:
    /// - `┌` where one opens;
    /// - `│` through each line it hides;
    /// - `┘` on the last line it hides;
    /// - `c` opens a comment run and `r` a region, since for those two the *kind* is part of the
    ///   answer.
    ///
    /// Drawn, not asserted as line numbers, for the same reason as the highlight map. The rule
    /// likeliest to break is "the closer stays visible", and here you can see it: the `end` line
    /// carries no mark. `assert_eq!(end_line, 4)` can only be checked against the arithmetic that
    /// produced it. Source and drawing are written a line at a time so they line up in the file as
    /// on screen.
    fn drawn(source: &[&str]) -> String {
        let text = TextDocument::new(source.join("\n") + "\n", PositionEncoding::Utf16);
        let found = folds(&text);

        // One lane per fold, reused left to right once its fold has closed. Nesting reads as
        // indentation, and unrelated folds do not each claim a column.
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

    /// The text each link of the chain covers, innermost first. Newlines are shown so a multi-line
    /// link stays one line of the assertion.
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
        // Six constructs nested six deep, and the module's whole rule in one picture: none of the
        // five `end` keywords carries a mark. A collapsed `def build` with nothing closing it reads
        // as broken code.
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
        // `if`, `elsif` and `else` fold as three, like the editor's indentation guess and as anyone
        // reading a long conditional wants. An `elsif` arrives as an `IfNode` in the first one's
        // `subsequent`, so this module knows neither that word nor `when`.
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
        // A `case` folds as a whole *and* as its branches. The whole ends at the last branch, not
        // at `end`, hence the last two lines.
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
        // `super do ... end` is the one place Prism hands a block to its typed visitor instead of
        // `visit`. It is in the fixture to keep that override honest.
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
        // The setter's argument list and the block both open on the first line, and an editor draws
        // one chevron there. Keep the wider: it is the outer construct, and what a reader clicking
        // that line meant.
        assert_eq!(
            drawn(&["obj.items = list.map do |x|", "  x", "end"]),
            rows(&["┌ obj.items = list.map do |x|", "│   x", "┘ end"])
        );
    }

    #[test]
    fn literals_a_heredoc_and_an_argument_list() {
        // The four an indentation guess gets wrong or cannot see. A heredoc's body is nowhere near
        // its expression. `%w[]` is here because its elements are strings with no quotes of their
        // own, which decides whether the heredoc test asks about the right location.
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
        // A range whose two lines are the same draws a chevron that does nothing, which is worse
        // than none. Every line below holds a construct this module otherwise folds, and the answer
        // is an empty gutter.
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
        // Four rules in one picture:
        // 1. A run needs a second line.
        // 2. A comment after code is that line's tail, not a line of its own.
        // 3. `# regional` merely starts like a marker.
        // 4. A real marker ends the run it interrupts and opens a fold the syntax knows nothing
        //    about.
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
        // Both directions of the same typo. Collapsing the rest of a file over a forgotten
        // `#endregion` is worse than leaving those lines alone.
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
        // A `def` with a `rescue` gets an implicit keyword-less `BeginNode`. The `def`'s own fold
        // already covers those lines, so a second fold would be a duplicate chevron on the same
        // line.
        assert_eq!(
            drawn(&["def f", "  1", "rescue", "  2", "end"]),
            rows(&["┌  def f", "│    1", "│┌ rescue", "┘┘   2", "   end"])
        );
    }

    #[test]
    fn a_half_typed_buffer_still_folds_what_it_has() {
        // The normal state of a file. Prism recovers, the `class` still has a body, and the folds
        // match the finished file's.
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
        // `ArgumentsNode` is one of the thirteen reached through the typed visitor. Without that
        // override, this chain jumps from `2` straight to the whole call.
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
        // The step a generic walk misses, in Ruby's commonest shape. `a.b.c` nests as `((a.b).c)`:
        // the outer call's receiver is the whole inner call, and no node spells `order.line_items`
        // alone.
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
        // Past the last statement there is no node to start from, and the file is the answer, not
        // an empty array. The protocol pairs chains to positions by index, so every position needs
        // one.
        assert_eq!(chain("x = 1\n\n~"), "x = 1⏎⏎");
        assert_eq!(chain("~"), "");
    }

    #[test]
    fn every_link_contains_the_one_before_it_however_the_buffer_parses() {
        // The invariant the protocol *defines* the response by, swept over a file typed one
        // character at a time. That is where Prism's recovery hands out locations that do not nest.
        // `locator::nests` is the predicate; this proves it is asked everywhere.
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
        // Four spellings that need a step Prism has no node for. In one, the interpolated string,
        // the step is what lies between the delimiters, not a `content`.
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
        // The rest of the thirteen, each in the one shape that arrives through its typed visitor:
        // - a constant path being assigned;
        // - a `super` with a block;
        // - a named capture;
        // - a find pattern and a capture pattern;
        // - a block parameter.
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

    #[test]
    fn a_selection_chain_arrives_as_a_nest_of_parents() {
        // The half `ranges`' own tests cannot see. LSP spells a chain as one range carrying its
        // parent, the innermost at the top, and the outermost has no `parent` key at all.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        assert_eq!(
            harness.selection(&uri, "puts \"he~llo\"\n"),
            serde_json::json!([{
                "range": { "start": { "line": 0, "character": 6 },
                           "end": { "line": 0, "character": 11 } },
                "parent": {
                    "range": { "start": { "line": 0, "character": 5 },
                               "end": { "line": 0, "character": 12 } },
                    "parent": {
                        "range": { "start": { "line": 0, "character": 0 },
                                   "end": { "line": 0, "character": 12 } },
                        "parent": {
                            "range": { "start": { "line": 0, "character": 0 },
                                       "end": { "line": 1, "character": 0 } }
                        }
                    }
                }
            }])
        );
    }

    #[test]
    fn one_chain_comes_back_per_position_asked_about_in_the_order_asked() {
        // The protocol pairs the two arrays by index and cannot say "not this one". So a position
        // that resolved to nothing still answers, with the buffer: the second of these.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();
        harness.open(&uri, "call(1)\n\n");

        let found = harness.ask(
            "textDocument/selectionRange",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "positions": [
                    { "line": 0, "character": 5 },
                    { "line": 1, "character": 0 },
                ],
            }),
        );

        let chains = found.as_array().expect("one chain per position");
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0]["range"]["end"]["character"], 6);
        assert_eq!(
            chains[1]["range"]["end"],
            serde_json::json!({ "line": 2, "character": 0 })
        );
        assert_eq!(chains[1]["parent"], serde_json::Value::Null);
    }

    #[test]
    fn folding_ranges_are_whole_lines_and_carry_no_characters() {
        // Every client that matters sends `lineFoldingOnly`, so a character offset would be a field
        // it ignores and that can only be wrong. `kind` is absent too: syntax folds have none, and
        // `null` is not one of LSP's three.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        assert_eq!(
            harness.folding(&uri, "# note\n# more\ndef foo\n  1\nend\n"),
            serde_json::json!([
                { "startLine": 0, "endLine": 1, "kind": "comment" },
                { "startLine": 2, "endLine": 3 },
            ])
        );
    }

    #[test]
    fn a_file_with_nothing_to_fold_answers_null_rather_than_an_empty_list() {
        // Here `null` versus `[]` costs the user something. A client with a folding provider stops
        // guessing folds from indentation, so `[]` would remove the guess and replace it with
        // nothing. `null` hands it back.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        assert_eq!(
            harness.folding(&uri, "x = 1\ny = 2\n"),
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_file_the_editor_never_opened_still_folds_and_still_expands() {
        // Both read through `with_text`, so both answer from disk for a file no `didOpen` named, as
        // when an editor previews a file.
        let mut harness = Harness::new();
        let uri = harness.write("lib/b.rb", "def foo\n  1\nend\n");
        harness.index();

        assert_eq!(
            harness.ask(
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            ),
            serde_json::json!([{ "startLine": 0, "endLine": 1 }])
        );
        let found = harness.ask(
            "textDocument/selectionRange",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "positions": [{ "line": 1, "character": 2 }],
            }),
        );
        assert_eq!(
            found[0]["range"]["start"],
            serde_json::json!({ "line": 1, "character": 2 })
        );
    }
}
